//! Integrations-Kern: reine Funktionen, die der Kernel an Boot/Reload/Reconnect,
//! bei der Freigabe-Pruefung und beim Werkzeug-Filter benutzt. Kein I/O hier.

use openfang_extensions::IntegrationStatus;
use openfang_runtime::mcp::{
    classify_connect_error, format_mcp_tool_name, scrub_secrets, ConnectErrorClass, McpServerConfig,
    McpTransport,
};
use openfang_types::config::{displayable_header_name, McpServerConfigEntry, McpTransportEntry};
use zeroize::Zeroizing;

pub fn to_runtime_transport(t: &McpTransportEntry) -> McpTransport {
    match t {
        McpTransportEntry::Stdio { command, args } => McpTransport::Stdio {
            command: command.clone(),
            args: args.clone(),
        },
        McpTransportEntry::Sse { url } => McpTransport::Sse { url: url.clone() },
        McpTransportEntry::Http { url } => McpTransport::Http { url: url.clone() },
    }
}

#[derive(Debug)]
pub enum BuildError {
    /// Referenz-Namen, die der Tresor nicht kennt.
    MissingCredentials(Vec<String>),
    /// Referenz-Name, dessen Wert oder Header-Name nicht header-tauglich ist.
    InvalidCredentialValue(String),
}

pub struct BuiltConfig {
    pub config: McpServerConfig,
    /// Aufgeloeste Werte, nur zum Schwaerzen von Fehlertexten.
    pub secrets: Vec<Zeroizing<String>>,
}

pub fn build_runtime_config(
    entry: &McpServerConfigEntry,
    resolve: &dyn Fn(&str) -> Option<Zeroizing<String>>,
) -> Result<BuiltConfig, BuildError> {
    let mut headers = entry.headers.clone();
    let mut secrets = Vec::new();
    let mut missing = Vec::new();
    for h in &entry.auth_headers {
        match resolve(&h.credential) {
            None => missing.push(h.credential.clone()),
            Some(value) => {
                if value.chars().any(|c| c.is_control()) {
                    return Err(BuildError::InvalidCredentialValue(h.credential.clone()));
                }
                // Defence in depth: Vorlagen sind validiert, der Name wird trotzdem geprueft.
                let Some(name) = displayable_header_name(&h.name) else {
                    return Err(BuildError::InvalidCredentialValue(h.credential.clone()));
                };
                headers.push(format!("{}: {}", name, h.format.replacen("{credential}", &value, 1)));
                secrets.push(value);
            }
        }
    }
    if !missing.is_empty() {
        return Err(BuildError::MissingCredentials(missing));
    }
    Ok(BuiltConfig {
        config: McpServerConfig {
            name: entry.name.clone(),
            transport: to_runtime_transport(&entry.transport),
            timeout_secs: entry.timeout_secs,
            env: entry.env.clone(),
            headers,
        },
        secrets,
    })
}

pub fn status_from_connect_error(err: &str, secrets: &[Zeroizing<String>]) -> IntegrationStatus {
    // scrub_secrets ersetzt nacheinander: laengste zuerst, sonst bleibt ein Rest
    // eines laengeren Geheimnisses stehen, das ein kuerzeres enthaelt.
    let mut refs: Vec<&str> = secrets.iter().map(|s| s.as_str()).collect();
    refs.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let clean = scrub_secrets(err, &refs);
    match classify_connect_error(&clean) {
        ConnectErrorClass::KeyRejected => IntegrationStatus::KeyRejected,
        ConnectErrorClass::InvalidHeader => IntegrationStatus::Unreachable("ungueltiger Header".into()),
        ConnectErrorClass::Unreachable => IntegrationStatus::Unreachable(unreachable_detail(&clean)),
    }
}

/// Feste, wertfreie Beschreibung eines Netzwerkfehlers: nur eine Klasse
/// (`dns`, `tls`, `timeout`, `connect`, `http <status>`), nie Antwortkoerper,
/// URL, Query-String oder Userinfo. Best effort auf dem kleingeschriebenen Text.
pub fn unreachable_detail(err: &str) -> String {
    let m = err.to_ascii_lowercase();
    if let Some(code) = http_status_code(&m) {
        return format!("http {code}");
    }
    let has = |needles: &[&str]| needles.iter().any(|n| m.contains(n));
    if has(&["dns error", "failed to lookup", "name resolution", "no such host", "nodename nor servname"]) {
        "dns".into()
    } else if has(&["certificate", "tls", "ssl", "handshake"]) {
        "tls".into()
    } else if has(&["timed out", "timeout", "deadline"]) {
        "timeout".into()
    } else if has(&["error sending request", "connection refused", "connection reset", "error trying to connect", "connect error", "connection closed", "broken pipe"]) {
        "connect".into()
    } else {
        "verbindung fehlgeschlagen".into()
    }
}

/// Liest einen HTTP-Status (100..=599) nach "http", "status" oder "code".
/// Ziffern in Ports, Pfaden, Adressen oder laengeren Zahlen zaehlen nicht.
fn http_status_code(m: &str) -> Option<u16> {
    let bytes = m.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let is_three_digits = bytes[i..i + 3].iter().all(|b| b.is_ascii_digit());
        let prev = i.checked_sub(1).map(|j| bytes[j]);
        let next = bytes.get(i + 3).copied();
        if is_three_digits
            && !matches!(prev, Some(b) if b.is_ascii_digit() || b == b':' || b == b'/' || b == b'.')
            && !matches!(next, Some(b) if b.is_ascii_digit())
        {
            let before = m[..i].trim_end_matches([' ', ':', '=']);
            if before.ends_with("http") || before.ends_with("status") || before.ends_with("code") {
                if let Ok(code) = m[i..i + 3].parse::<u16>() {
                    if (100..=599).contains(&code) {
                        return Some(code);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

pub fn integration_tool_requires_approval(tool_name: &str, server: &str, read_only_tools: &[String]) -> bool {
    !read_only_tools.iter().any(|ro| format_mcp_tool_name(server, ro) == tool_name)
}

/// Wem gehoert ein MCP-Werkzeug, aus Sicht der Freigabe?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// Genau eine installierte Integration besitzt das Werkzeug.
    Single(String),
    /// Mehrere moegliche Besitzer: fail closed, Freigabe noetig.
    Ambiguous,
    /// Kein Integrations-Werkzeug (bestehendes Verhalten).
    NotIntegration,
}

/// Bestimmt den Besitzer eines Werkzeugs. Massgeblich ist `origins` — die
/// Server, deren Verbindung das Werkzeug registriert hat. Nur wenn es keinen
/// Origin-Eintrag gibt, wird ueber das Namenspraefix `mcp_<id>_` der
/// installierten Integrationen zugeordnet; passt es auf mehrere, ist das
/// Ergebnis mehrdeutig.
pub fn owning_integration(
    origins: Option<&std::collections::HashSet<String>>,
    tool_name: &str,
    installed: &[String],
) -> Owner {
    if let Some(origins) = origins.filter(|o| !o.is_empty()) {
        let mut iter = origins.iter();
        return match (iter.next(), iter.next()) {
            (Some(server), None) if installed.iter().any(|i| i == server) => {
                Owner::Single(server.clone())
            }
            (Some(_), None) => Owner::NotIntegration,
            _ => Owner::Ambiguous,
        };
    }
    let mut matches = installed.iter().filter(|id| {
        tool_name.starts_with(&format!("mcp_{}_", openfang_runtime::mcp::normalize_name(id)))
    });
    match (matches.next(), matches.next()) {
        (None, _) => Owner::NotIntegration,
        (Some(id), None) => Owner::Single(id.clone()),
        (Some(_), Some(_)) => Owner::Ambiguous,
    }
}

/// Darf ein Hand-Agent dieses Werkzeug ohne Freigabe ausfuehren? Haende sind
/// kuratierte Pakete und werden auto-freigegeben — Integrations-Werkzeuge
/// (Zugriff auf Fremdsysteme mit Tresor-Schluessel) sind davon ausgenommen.
pub fn hand_auto_approve_allowed(is_hand: bool, is_integration_tool: bool) -> bool {
    is_hand && !is_integration_tool
}

pub fn mcp_server_visible(server: &str, allowlist: &[String], is_integration: bool) -> bool {
    let listed = allowlist.iter().any(|a| a == server);
    if is_integration { listed } else { allowlist.is_empty() || listed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_auto_approve_never_covers_integration_tools() {
        assert!(hand_auto_approve_allowed(true, false));
        assert!(!hand_auto_approve_allowed(true, true));
        assert!(!hand_auto_approve_allowed(false, false));
        assert!(!hand_auto_approve_allowed(false, true));
    }

    use openfang_types::config::{AuthHeaderRef, McpServerConfigEntry, McpTransportEntry};
    use zeroize::Zeroizing;

    fn entry() -> McpServerConfigEntry {
        McpServerConfigEntry {
            name: "github".into(),
            transport: McpTransportEntry::Http { url: "https://api.githubcopilot.com/mcp/".into() },
            timeout_secs: 30,
            env: vec![],
            headers: vec![],
            auth_headers: vec![AuthHeaderRef {
                name: "Authorization".into(),
                format: "Bearer {credential}".into(),
                credential: "INTEG_KERN_TEST_KEY".into(),
            }],
            oauth: false,
        }
    }

    #[test]
    fn builds_header_only_into_runtime_config_never_into_env() {
        let resolve = |k: &str| (k == "INTEG_KERN_TEST_KEY").then(|| Zeroizing::new("KANARIE-789".to_string()));
        let built = build_runtime_config(&entry(), &resolve).unwrap();
        assert_eq!(built.config.headers, vec!["Authorization: Bearer KANARIE-789".to_string()]);
        assert!(built.config.env.is_empty());
        assert!(std::env::var("INTEG_KERN_TEST_KEY").is_err());
        assert!(!format!("{:?}", built.config).contains("KANARIE-789"));
        assert_eq!(built.secrets.len(), 1);
    }

    #[test]
    fn missing_credential_is_reported_by_name_without_connecting() {
        let resolve = |_: &str| None;
        match build_runtime_config(&entry(), &resolve) {
            Err(BuildError::MissingCredentials(names)) => assert_eq!(names, vec!["INTEG_KERN_TEST_KEY".to_string()]),
            other => panic!("expected MissingCredentials, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn credential_value_with_newline_is_rejected() {
        let resolve = |_: &str| Some(Zeroizing::new("abc\r\nX-Evil: 1".to_string()));
        assert!(matches!(build_runtime_config(&entry(), &resolve), Err(BuildError::InvalidCredentialValue(_))));
    }

    #[test]
    fn non_displayable_header_name_is_rejected() {
        let mut e = entry();
        e.auth_headers[0].name = "Bad Name\r\nX: 1".into();
        let resolve = |_: &str| Some(Zeroizing::new("KANARIE-789".to_string()));
        match build_runtime_config(&e, &resolve) {
            Err(BuildError::InvalidCredentialValue(n)) => assert_eq!(n, "INTEG_KERN_TEST_KEY"),
            other => panic!("expected InvalidCredentialValue, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn status_from_errors_scrubs_secret() {
        let s = vec![Zeroizing::new("KANARIE-789".to_string())];
        assert_eq!(status_from_connect_error("HTTP 401 for KANARIE-789", &s), IntegrationStatus::KeyRejected);
        match status_from_connect_error("dns error near KANARIE-789", &s) {
            IntegrationStatus::Unreachable(msg) => assert!(!msg.contains("KANARIE-789")),
            other => panic!("{other:?}"),
        }
        match status_from_connect_error("MCP header invalid: Authorization", &s) {
            IntegrationStatus::Unreachable(msg) => assert!(msg.contains("ungueltiger Header")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unreachable_detail_never_carries_vendor_body_url_or_userinfo() {
        let err = "unexpected server response: HTTP 503 Service Unavailable: {\"echo\":\"KANARIE-BODY\"} for url (https://user:pw@h/mcp?token=KANARIE-Q)";
        match status_from_connect_error(err, &[]) {
            IntegrationStatus::Unreachable(msg) => {
                assert!(!msg.contains("KANARIE-BODY"), "{msg}");
                assert!(!msg.contains("KANARIE-Q"), "{msg}");
                assert!(!msg.contains("user:pw"), "{msg}");
                assert!(msg.contains("503"), "{msg}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unreachable_detail_is_a_fixed_class() {
        let detail = |e: &str| match status_from_connect_error(e, &[]) {
            IntegrationStatus::Unreachable(msg) => msg,
            other => panic!("{other:?}"),
        };
        assert_eq!(detail("error trying to connect: dns error: failed to lookup address KANARIE"), "dns");
        assert_eq!(detail("invalid peer certificate: UnknownIssuer KANARIE"), "tls");
        assert_eq!(detail("operation timed out KANARIE"), "timeout");
        assert_eq!(
            detail("Client error: error sending request for url (http://127.0.0.1:4010/mcp?k=KANARIE)"),
            "connect"
        );
        assert_eq!(detail("HTTP status 502 Bad Gateway <html>KANARIE</html>"), "http 502");
        assert_eq!(detail("irgendwas KANARIE"), "verbindung fehlgeschlagen");
        // A port must not be read as an HTTP status.
        assert_eq!(detail("error sending request for url (http://h:5030/mcp)"), "connect");
    }

    #[test]
    fn status_scrubs_longest_secret_first() {
        let s = vec![Zeroizing::new("AB".to_string()), Zeroizing::new("ABCDEF".to_string())];
        match status_from_connect_error("x ABCDEF y", &s) {
            IntegrationStatus::Unreachable(msg) => assert!(!msg.contains("CDEF"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn approval_default_is_on_except_read_only() {
        let ro = vec!["get_me".to_string()];
        assert!(!integration_tool_requires_approval("mcp_github_get_me", "github", &ro));
        assert!(integration_tool_requires_approval("mcp_github_create_issue", "github", &ro));
        assert!(integration_tool_requires_approval("mcp_github_brand_new_tool", "github", &ro));
        assert!(integration_tool_requires_approval("mcp_github_get_me", "github", &[]));
    }

    fn set(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn owner_comes_from_connection_origin_not_from_name_prefix() {
        let installed = vec!["github".to_string()];
        // Origin wins: a plain server owns a tool even if its name looks like an integration's.
        assert_eq!(
            owning_integration(Some(&set(&["rowboat"])), "mcp_github_create_issue", &installed),
            Owner::NotIntegration
        );
        assert_eq!(
            owning_integration(Some(&set(&["github"])), "mcp_github_create_issue", &installed),
            Owner::Single("github".to_string())
        );
    }

    #[test]
    fn several_origins_are_ambiguous() {
        let installed = vec!["github".to_string()];
        assert_eq!(
            owning_integration(Some(&set(&["github", "rowboat"])), "mcp_x_y", &installed),
            Owner::Ambiguous
        );
        assert_eq!(
            owning_integration(Some(&set(&["a", "b"])), "mcp_x_y", &installed),
            Owner::Ambiguous
        );
    }

    #[test]
    fn prefix_fallback_only_without_origin_and_ambiguous_prefix_is_ambiguous() {
        let installed = vec!["github".to_string(), "github-enterprise".to_string()];
        assert_eq!(
            owning_integration(None, "mcp_github_get_me", &installed),
            Owner::Single("github".to_string())
        );
        assert_eq!(
            owning_integration(Some(&set(&[])), "mcp_github_get_me", &installed),
            Owner::Single("github".to_string())
        );
        // "mcp_github_enterprise_x" matches both "mcp_github_" and "mcp_github_enterprise_".
        assert_eq!(
            owning_integration(None, "mcp_github_enterprise_get_me", &installed),
            Owner::Ambiguous
        );
        assert_eq!(owning_integration(None, "mcp_rowboat_status", &installed), Owner::NotIntegration);
        assert_eq!(owning_integration(None, "shell_exec", &installed), Owner::NotIntegration);
        assert_eq!(owning_integration(None, "mcp_github_get_me", &[]), Owner::NotIntegration);
    }

    #[test]
    fn integrations_are_opt_in_while_plain_servers_keep_empty_means_all() {
        assert!(mcp_server_visible("rowboat", &[], false));
        assert!(!mcp_server_visible("github", &[], true));
        assert!(mcp_server_visible("github", &["github".to_string()], true));
        assert!(!mcp_server_visible("github", &["rowboat".to_string()], true));
        assert!(!mcp_server_visible("rowboat", &["github".to_string()], false));
    }
}
