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
        ConnectErrorClass::Unreachable => IntegrationStatus::Unreachable(clean),
    }
}

pub fn integration_tool_requires_approval(tool_name: &str, server: &str, read_only_tools: &[String]) -> bool {
    !read_only_tools.iter().any(|ro| format_mcp_tool_name(server, ro) == tool_name)
}

pub fn mcp_server_visible(server: &str, allowlist: &[String], is_integration: bool) -> bool {
    let listed = allowlist.iter().any(|a| a == server);
    if is_integration { listed } else { allowlist.is_empty() || listed }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn integrations_are_opt_in_while_plain_servers_keep_empty_means_all() {
        assert!(mcp_server_visible("rowboat", &[], false));
        assert!(!mcp_server_visible("github", &[], true));
        assert!(mcp_server_visible("github", &["github".to_string()], true));
        assert!(!mcp_server_visible("github", &["rowboat".to_string()], true));
        assert!(!mcp_server_visible("rowboat", &["github".to_string()], false));
    }
}
