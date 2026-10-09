//! Integration Registry — manages bundled + installed integration templates.
//!
//! Loads 25 bundled MCP server templates at compile time, merges with user's
//! installed state from `~/.openfang/integrations.toml`, and converts installed
//! integrations to `McpServerConfigEntry` for kernel consumption.

use crate::{
    ExtensionError, ExtensionResult, InstalledIntegration, IntegrationCategory, IntegrationInfo,
    IntegrationStatus, IntegrationTemplate, IntegrationsFile,
};
use openfang_types::config::{McpServerConfigEntry, McpTransportEntry};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Prueft eine Vorlage auf Konsistenz (auth_headers nur bei http/sse, gueltige Felder).
pub fn validate_template(t: &crate::IntegrationTemplate) -> Result<(), String> {
    if !t.auth_headers.is_empty() {
        match t.transport {
            crate::McpTransportTemplate::Http { .. } | crate::McpTransportTemplate::Sse { .. } => {}
            crate::McpTransportTemplate::Stdio { .. } => {
                return Err("auth_headers nur bei http/sse erlaubt".into());
            }
        }
    }
    for h in &t.auth_headers {
        h.validate()?;
    }
    if let Some(auth) = &t.auth {
        let _ = auth;
        match t.transport {
            crate::McpTransportTemplate::Http { .. } | crate::McpTransportTemplate::Sse { .. } => {}
            crate::McpTransportTemplate::Stdio { .. } => return Err("oauth nur bei http/sse erlaubt".into()),
        }
        if !t.auth_headers.is_empty() {
            return Err("oauth schliesst auth_headers aus".into());
        }
    }
    if t.catalog.is_some()
        && t.auth_headers.iter().any(|h| !h.credential.starts_with(crate::INTEGRATION_PREFIX))
    {
        return Err("auth_headers: Referenz muss mit INTEGRATION_ beginnen".into());
    }
    Ok(())
}

/// Ergebnis des Ladens von Vorlagen-Ordnern.
#[derive(Debug, Default)]
pub struct TemplateDirReport {
    /// Geladene Vorlagen (Dateien).
    pub loaded: usize,
    /// Uebersprungene Dateien und nicht lesbare Ordner (Pfad + Grund ohne Inhalt).
    pub skipped: Vec<(std::path::PathBuf, String)>,
    /// Ordner, die fehlen oder nicht lesbar sind.
    pub unreadable_dirs: usize,
}

impl TemplateDirReport {
    /// Anzahl uebersprungener Vorlagen-Dateien (ohne nicht lesbare Ordner).
    pub fn skipped_files(&self) -> usize {
        self.skipped.len() - self.unreadable_dirs
    }
}

/// The integration registry — holds all known templates and install state.
pub struct IntegrationRegistry {
    /// All known templates (bundled + custom).
    templates: HashMap<String, IntegrationTemplate>,
    /// Current installed state.
    installed: HashMap<String, InstalledIntegration>,
    /// Path to integrations.toml.
    integrations_path: PathBuf,
    /// Ids, deren Ordner-Vorlage ungueltig war (kein stiller Rueckfall auf die eingebaute).
    invalid_overrides: HashSet<String>,
}

impl IntegrationRegistry {
    /// Create a new registry with no templates.
    pub fn new(home_dir: &Path) -> Self {
        Self {
            templates: HashMap::new(),
            installed: HashMap::new(),
            integrations_path: home_dir.join("integrations.toml"),
            invalid_overrides: HashSet::new(),
        }
    }

    /// Load bundled templates (compile-time embedded). Returns count loaded.
    pub fn load_bundled(&mut self) -> usize {
        let bundled = crate::bundled::bundled_integrations();
        let count = bundled.len();
        for (id, toml_content) in bundled {
            match toml::from_str::<IntegrationTemplate>(toml_content) {
                Ok(template) => {
                    if let Err(why) = validate_template(&template) {
                        warn!("Bundled integration '{}' invalid: {}", id, why);
                        continue;
                    }
                    self.templates.insert(id.to_string(), template);
                }
                Err(e) => {
                    warn!("Failed to parse bundled integration '{}': {}", id, e);
                }
            }
        }
        debug!("Loaded {count} bundled integration template(s)");
        count
    }

    /// Vorlagen aus Ordnern laden; gleiche id ueberschreibt die eingebaute.
    /// Ungueltige Dateien werden uebersprungen (Grund ohne Dateiinhalt).
    pub fn load_template_dirs(&mut self, dirs: &[std::path::PathBuf]) -> TemplateDirReport {
        let mut report = TemplateDirReport::default();
        for dir in dirs {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        dir = %dir.display(),
                        kind = ?e.kind(),
                        "Integrations-Vorlagen-Ordner nicht lesbar"
                    );
                    report.skipped.push((dir.clone(), format!("Ordner nicht lesbar: {}", e.kind())));
                    report.unreadable_dirs += 1;
                    continue;
                }
            };
            let mut paths: Vec<_> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("toml"))
                .collect();
            paths.sort();
            for path in paths {
                let mut failed_id: Option<String> = None;
                let parsed = std::fs::read_to_string(&path)
                    .map_err(|e| format!("nicht lesbar: {}", e.kind()))
                    .and_then(|s| {
                        toml::from_str::<IntegrationTemplate>(&s)
                            .map_err(|_| "TOML ungueltig".to_string())
                    })
                    .and_then(|t| match validate_template(&t) {
                        Ok(()) => Ok(t),
                        Err(why) => {
                            failed_id = Some(t.id.clone());
                            Err(why)
                        }
                    });
                match parsed {
                    Ok(t) => {
                        self.invalid_overrides.remove(&t.id);
                        self.templates.insert(t.id.clone(), t);
                        report.loaded += 1;
                    }
                    Err(why) => {
                        if let Some(id) = failed_id {
                            self.invalid_overrides.insert(id);
                        }
                        warn!(file = %path.display(), reason = %why, "Integrations-Vorlage uebersprungen");
                        report.skipped.push((path, why));
                    }
                }
            }
        }
        report
    }

    /// Load installed state from integrations.toml.
    pub fn load_installed(&mut self) -> ExtensionResult<usize> {
        if !self.integrations_path.exists() {
            return Ok(0);
        }
        let content = std::fs::read_to_string(&self.integrations_path)?;
        let file: IntegrationsFile =
            toml::from_str(&content).map_err(|e| ExtensionError::TomlParse(e.to_string()))?;
        let count = file.installed.len();
        for entry in file.installed {
            self.installed.insert(entry.id.clone(), entry);
        }
        info!("Loaded {count} installed integration(s)");
        Ok(count)
    }

    /// Save installed state to integrations.toml.
    pub fn save_installed(&self) -> ExtensionResult<()> {
        let file = IntegrationsFile {
            installed: self.installed.values().cloned().collect(),
        };
        let content =
            toml::to_string_pretty(&file).map_err(|e| ExtensionError::TomlParse(e.to_string()))?;
        if let Some(parent) = self.integrations_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.integrations_path, content)?;
        Ok(())
    }

    /// Ids, deren Ordner-Vorlage ungueltig war; installierte davon werden nie verbunden.
    pub fn invalid_overrides(&self) -> &HashSet<String> {
        &self.invalid_overrides
    }

    /// Get a template by ID.
    pub fn get_template(&self, id: &str) -> Option<&IntegrationTemplate> {
        self.templates.get(id)
    }

    /// Get an installed record by ID.
    pub fn get_installed(&self, id: &str) -> Option<&InstalledIntegration> {
        self.installed.get(id)
    }

    /// Check if an integration is installed.
    pub fn is_installed(&self, id: &str) -> bool {
        self.installed.contains_key(id)
    }

    /// Mark an integration as installed.
    pub fn install(&mut self, entry: InstalledIntegration) -> ExtensionResult<()> {
        if self.installed.contains_key(&entry.id) {
            return Err(ExtensionError::AlreadyInstalled(entry.id.clone()));
        }
        self.installed.insert(entry.id.clone(), entry);
        self.save_installed()
    }

    /// Remove an installed integration.
    pub fn uninstall(&mut self, id: &str) -> ExtensionResult<()> {
        if self.installed.remove(id).is_none() {
            return Err(ExtensionError::NotInstalled(id.to_string()));
        }
        self.save_installed()
    }

    /// Enable/disable an installed integration.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> ExtensionResult<()> {
        let entry = self
            .installed
            .get_mut(id)
            .ok_or_else(|| ExtensionError::NotInstalled(id.to_string()))?;
        entry.enabled = enabled;
        self.save_installed()
    }

    /// List all templates.
    pub fn list_templates(&self) -> Vec<&IntegrationTemplate> {
        let mut templates: Vec<_> = self.templates.values().collect();
        templates.sort_by(|a, b| a.id.cmp(&b.id));
        templates
    }

    /// List templates by category.
    pub fn list_by_category(&self, category: &IntegrationCategory) -> Vec<&IntegrationTemplate> {
        self.templates
            .values()
            .filter(|t| &t.category == category)
            .collect()
    }

    /// Search templates by query (matches id, name, description, tags).
    pub fn search(&self, query: &str) -> Vec<&IntegrationTemplate> {
        let q = query.to_lowercase();
        self.templates
            .values()
            .filter(|t| {
                t.id.to_lowercase().contains(&q)
                    || t.name.to_lowercase().contains(&q)
                    || t.description.to_lowercase().contains(&q)
                    || t.tags.iter().any(|tag| tag.to_lowercase().contains(&q))
            })
            .collect()
    }

    /// Get combined info for all integrations (template + install state).
    pub fn list_all_info(&self) -> Vec<IntegrationInfo> {
        self.templates
            .values()
            .map(|t| {
                let installed = self.installed.get(&t.id);
                let status = match installed {
                    Some(inst) if !inst.enabled => IntegrationStatus::Disabled,
                    Some(_) => IntegrationStatus::Ready,
                    None => IntegrationStatus::Available,
                };
                IntegrationInfo {
                    template: t.clone(),
                    status,
                    installed: installed.cloned(),
                    tool_count: 0,
                }
            })
            .collect()
    }

    /// Convert all enabled installed integrations to MCP server config entries.
    /// These can be merged into the kernel's MCP server list.
    pub fn to_mcp_configs(&self) -> Vec<McpServerConfigEntry> {
        self.installed
            .values()
            .filter(|inst| inst.enabled)
            .filter_map(|inst| {
                if self.invalid_overrides.contains(&inst.id) {
                    return None;
                }
                let template = self.templates.get(&inst.id)?;
                // Nicht zugelassene Vorlagen werden nie verbunden (Spec 4.1/4.4).
                if !template.is_admitted() {
                    return None;
                }
                let transport = match &template.transport {
                    crate::McpTransportTemplate::Stdio { command, args } => {
                        McpTransportEntry::Stdio {
                            command: command.clone(),
                            args: args.clone(),
                        }
                    }
                    crate::McpTransportTemplate::Sse { url } => {
                        McpTransportEntry::Sse { url: url.clone() }
                    }
                    crate::McpTransportTemplate::Http { url } => {
                        McpTransportEntry::Http { url: url.clone() }
                    }
                };
                let is_remote =
                    !matches!(template.transport, crate::McpTransportTemplate::Stdio { .. });
                let env: Vec<String> = if is_remote {
                    Vec::new()
                } else {
                    template.required_env.iter().map(|e| e.name.clone()).collect()
                };
                Some(McpServerConfigEntry {
                    name: inst.id.clone(),
                    transport,
                    timeout_secs: 30,
                    env,
                    headers: Vec::new(),
                    auth_headers: if template.auth.is_some() {
                        vec![openfang_types::config::AuthHeaderRef {
                            name: "Authorization".into(),
                            format: "Bearer {credential}".into(),
                            credential: crate::oauth_reference(&inst.id),
                        }]
                    } else {
                        template.auth_headers.clone()
                    },
                    oauth: template.auth.is_some(),
                })
            })
            .collect()
    }

    /// Ids installierter Integrationen, deren Vorlage nicht zugelassen ist
    /// (sortiert). Diese werden nie verbunden.
    pub fn not_admitted_installed(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .installed
            .keys()
            .filter(|id| self.templates.get(*id).map(|t| !t.is_admitted()).unwrap_or(false))
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    /// Get the path to integrations.toml.
    pub fn integrations_path(&self) -> &Path {
        &self.integrations_path
    }

    /// Total template count.
    pub fn template_count(&self) -> usize {
        self.templates.len()
    }

    /// Total installed count.
    pub fn installed_count(&self) -> usize {
        self.installed.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REMOTE: &str = r#"
id = "github"
name = "GitHub"
description = "GitHub ueber den offiziellen Remote-MCP-Server"
category = "devtools"
read_only_tools = ["get_me"]
[transport]
type = "http"
url = "https://api.githubcopilot.com/mcp/"
[[auth_headers]]
name = "Authorization"
format = "Bearer {credential}"
credential = "INTEGRATION_GITHUB_PAT"
[[required_env]]
name = "INTEGRATION_GITHUB_PAT"
label = "GitHub PAT"
help = "fein granuliert"
[catalog]
replaces_openai_plugin = "github"
license = "MIT"
admission = "admitted"
"#;

    /// Wie REMOTE, aber mit der alten Referenz (nicht INTEGRATION_) — fuer Negativtests.
    const REMOTE_LEGACY: &str = r#"
id = "github"
name = "GitHub"
description = "GitHub ueber den offiziellen Remote-MCP-Server"
category = "devtools"
read_only_tools = ["get_me"]
[transport]
type = "http"
url = "https://api.githubcopilot.com/mcp/"
[[auth_headers]]
name = "Authorization"
format = "Bearer {credential}"
credential = "GITHUB_PAT_TOKEN"
[[required_env]]
name = "GITHUB_PAT_TOKEN"
label = "GitHub PAT"
help = "fein granuliert"
[catalog]
replaces_openai_plugin = "github"
license = "MIT"
admission = "admitted"
"#;

    const VERCEL: &str = r#"
id = "vercel"
name = "Vercel"
description = "Vercel ueber den offiziellen Remote-MCP-Server"
category = "devtools"
read_only_tools = ["list_projects"]
[transport]
type = "http"
url = "https://mcp.vercel.com/"
[auth]
type = "oauth"
scopes = ["offline_access"]
[catalog]
replaces_openai_plugin = "vercel"
license = "proprietary-service"
admission = "admitted"
"#;

    #[test]
    fn oauth_template_parses_and_maps_to_oauth_entry_with_bearer_reference() {
        let t: crate::IntegrationTemplate = toml::from_str(VERCEL).unwrap();
        assert!(validate_template(&t).is_ok());
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vercel.toml"), VERCEL).unwrap();
        let mut reg = IntegrationRegistry::new(home.path());
        reg.load_bundled();
        reg.load_template_dirs(&[dir.path().to_path_buf()]);
        reg.install(crate::InstalledIntegration { id: "vercel".into(), installed_at: chrono::Utc::now(),
            enabled: true, oauth_provider: None, config: Default::default() }).unwrap();
        let e = reg.to_mcp_configs().into_iter().find(|c| c.name == "vercel").unwrap();
        assert!(e.oauth);
        assert!(e.env.is_empty());
        assert_eq!(e.auth_headers.len(), 1);
        assert_eq!(e.auth_headers[0].name, "Authorization");
        assert_eq!(e.auth_headers[0].format, "Bearer {credential}");
        assert_eq!(e.auth_headers[0].credential, crate::oauth_reference("vercel"));
    }

    #[test]
    fn oauth_reference_shape() {
        assert_eq!(crate::oauth_reference("vercel"), "INTEGRATION_OAUTH_VERCEL");
        assert_eq!(crate::oauth_reference("github-probe"), "INTEGRATION_OAUTH_GITHUB_PROBE");
    }

    #[test]
    fn oauth_rejected_on_stdio_and_together_with_auth_headers() {
        let mut t: crate::IntegrationTemplate = toml::from_str(VERCEL).unwrap();
        t.transport = crate::McpTransportTemplate::Stdio { command: "npx".into(), args: vec![] };
        assert!(validate_template(&t).is_err());
        let mut t: crate::IntegrationTemplate = toml::from_str(VERCEL).unwrap();
        t.auth_headers.push(openfang_types::config::AuthHeaderRef {
            name: "Authorization".into(), format: "Bearer {credential}".into(), credential: "INTEGRATION_X".into() });
        assert!(validate_template(&t).is_err());
    }

    #[test]
    fn catalog_templates_require_integration_prefix_for_static_keys() {
        let t: crate::IntegrationTemplate = toml::from_str(REMOTE_LEGACY).unwrap();
        assert!(validate_template(&t).is_err(), "catalog template with non-INTEGRATION_ reference must be rejected");
        let good = REMOTE_LEGACY.replace("credential = \"GITHUB_PAT_TOKEN\"", "credential = \"INTEGRATION_GITHUB_PAT\"");
        let t: crate::IntegrationTemplate = toml::from_str(&good).unwrap();
        assert!(validate_template(&t).is_ok());
    }

    #[test]
    fn invalid_folder_override_blocks_installed_id_instead_of_falling_back_to_stdio() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // REMOTE_LEGACY uses GITHUB_PAT_TOKEN -> invalid for a catalog template
        std::fs::write(dir.path().join("github.toml"), REMOTE_LEGACY).unwrap();
        let mut reg = IntegrationRegistry::new(home.path());
        reg.load_bundled();
        let report = reg.load_template_dirs(&[dir.path().to_path_buf()]);
        assert_eq!(report.loaded, 0);
        assert!(reg.invalid_overrides().contains("github"));
        reg.install(crate::InstalledIntegration { id: "github".into(), installed_at: chrono::Utc::now(),
            enabled: true, oauth_provider: None, config: Default::default() }).unwrap();
        assert!(reg.to_mcp_configs().iter().all(|c| c.name != "github"), "no silent stdio fallback");
    }
    fn write(dir: &std::path::Path, file: &str, body: &str) {
        std::fs::write(dir.join(file), body).unwrap();
    }

    #[test]
    fn folder_template_overrides_bundled_and_is_remote_without_env() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "github.toml", REMOTE);
        let mut reg = IntegrationRegistry::new(home.path());
        reg.load_bundled();
        let report = reg.load_template_dirs(&[dir.path().to_path_buf()]);
        assert_eq!(report.loaded, 1);
        assert!(report.skipped.is_empty());
        let t = reg.get_template("github").unwrap();
        assert!(matches!(t.transport, crate::McpTransportTemplate::Http { .. }));
        reg.install(crate::InstalledIntegration {
            id: "github".into(), installed_at: chrono::Utc::now(), enabled: true,
            oauth_provider: None, config: Default::default(),
        }).unwrap();
        let cfgs = reg.to_mcp_configs();
        let gh = cfgs.iter().find(|c| c.name == "github").unwrap();
        assert!(gh.env.is_empty(), "remote template must not export env: {:?}", gh.env);
        assert_eq!(gh.auth_headers.len(), 1);
        assert_eq!(gh.auth_headers[0].credential, "INTEGRATION_GITHUB_PAT");
        assert!(gh.headers.is_empty());
    }

    #[test]
    fn invalid_folder_template_is_skipped_and_bundled_stays() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "github.toml", &REMOTE.replace("Bearer {credential}", "Bearer"));
        write(dir.path(), "kaputt.toml", "das ist kein toml = = =");
        let mut reg = IntegrationRegistry::new(home.path());
        reg.load_bundled();
        let report = reg.load_template_dirs(&[dir.path().to_path_buf()]);
        assert_eq!(report.loaded, 0);
        assert_eq!(report.skipped.len(), 2);
        assert!(report.skipped.iter().all(|(_, why)| !why.contains("Bearer")));
        let t = reg.get_template("github").unwrap();
        assert!(matches!(t.transport, crate::McpTransportTemplate::Stdio { .. }), "bundled stays active");
    }

    #[test]
    fn validation_messages_never_contain_values() {
        let t: crate::IntegrationTemplate = toml::from_str(
            &REMOTE
                .replace("INTEGRATION_GITHUB_PAT\"\n[[required_env]]", "KANARIE-SECRET-1\"\n[[required_env]]")
                .replace("name = \"Authorization\"", "name = \"Auth KANARIE-SECRET-2\""),
        )
        .unwrap();
        let e = validate_template(&t).unwrap_err();
        assert!(!e.contains("KANARIE"), "{e}");

        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.toml",
            &REMOTE.replace("credential = \"INTEGRATION_GITHUB_PAT\"", "credential = \"KANARIE-SECRET-1\""),
        );
        write(
            dir.path(),
            "b.toml",
            &REMOTE.replace("name = \"Authorization\"", "name = \"Auth KANARIE-SECRET-2\""),
        );
        let mut reg = IntegrationRegistry::new(home.path());
        let report = reg.load_template_dirs(&[dir.path().to_path_buf()]);
        assert_eq!(report.skipped.len(), 2);
        assert!(report.skipped.iter().all(|(_, why)| !why.contains("KANARIE")));
    }

    /// Exakte Kopie von vibemind-os/integrations/github.toml (Pilot-Vorlage).
    const PILOT_GITHUB: &str = r##"id = "github"
name = "GitHub"
description = "GitHub ueber den offiziellen Remote-MCP-Server (GitHub Copilot MCP)"
category = "devtools"
icon = "🐙"
tags = ["git", "code", "issues", "pull-requests"]
read_only_tools = ["get_me", "search_repositories", "get_file_contents", "list_issues", "get_issue", "list_pull_requests", "get_pull_request"]

[transport]
type = "http"
url = "https://api.githubcopilot.com/mcp/"

[[auth_headers]]
name = "Authorization"
format = "Bearer {credential}"
credential = "INTEGRATION_GITHUB_PAT"

[[required_env]]
name = "INTEGRATION_GITHUB_PAT"
label = "GitHub Personal Access Token (fein granuliert)"
help = "Fein granulierter Token; Rechte nach Bedarf, fuer get_me reichen keine"
is_secret = true
get_url = "https://github.com/settings/personal-access-tokens/new"

[catalog]
replaces_openai_plugin = "github"
license = "MIT"
admission = "admitted"
"##;

    #[test]
    fn pilot_github_template_parses_and_validates() {
        let t: crate::IntegrationTemplate = toml::from_str(PILOT_GITHUB).unwrap();
        assert!(validate_template(&t).is_ok());
        assert!(t.is_admitted());
        match &t.transport {
            crate::McpTransportTemplate::Http { url } => {
                assert_eq!(url, "https://api.githubcopilot.com/mcp/")
            }
            other => panic!("expected http transport, got {other:?}"),
        }
        assert_eq!(t.auth_headers[0].credential, "INTEGRATION_GITHUB_PAT");
        assert!(t.read_only_tools.iter().any(|n| n == "get_me"));
    }

    #[test]
    fn auth_headers_on_stdio_are_rejected() {
        let mut t: crate::IntegrationTemplate = toml::from_str(REMOTE).unwrap();
        t.transport = crate::McpTransportTemplate::Stdio { command: "npx".into(), args: vec![] };
        assert!(validate_template(&t).is_err());
    }

    #[test]
    fn bundled_templates_unchanged_and_valid() {
        let home = tempfile::tempdir().unwrap();
        let mut reg = IntegrationRegistry::new(home.path());
        assert_eq!(reg.load_bundled(), 25);
        assert_eq!(reg.template_count(), 25);
        for t in reg.list_templates() {
            assert!(validate_template(t).is_ok(), "{}", t.id);
            assert!(t.is_admitted());
            assert!(t.auth_headers.is_empty());
        }
    }

    #[test]
    fn to_mcp_configs_skips_installed_templates_that_are_not_admitted() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "github.toml",
            &REMOTE.replace("admission = \"admitted\"", "admission = \"review_required\""),
        );
        let mut reg = IntegrationRegistry::new(home.path());
        reg.load_template_dirs(&[dir.path().to_path_buf()]);
        reg.install(crate::InstalledIntegration {
            id: "github".into(), installed_at: chrono::Utc::now(), enabled: true,
            oauth_provider: None, config: Default::default(),
        }).unwrap();
        assert!(reg.to_mcp_configs().iter().all(|c| c.name != "github"));
        assert_eq!(reg.not_admitted_installed(), vec!["github".to_string()]);
    }

    #[test]
    fn missing_template_dir_is_counted_and_others_still_load() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "github.toml", REMOTE);
        let missing = dir.path().join("gibt-es-nicht");
        let mut reg = IntegrationRegistry::new(home.path());
        let report = reg.load_template_dirs(&[missing, dir.path().to_path_buf()]);
        assert_eq!(report.loaded, 1);
        assert_eq!(report.unreadable_dirs, 1);
        assert_eq!(report.skipped_files(), 0);
    }

    #[test]
    fn review_required_template_is_not_admitted() {
        let t: crate::IntegrationTemplate =
            toml::from_str(&REMOTE.replace("admission = \"admitted\"", "admission = \"review_required\"")).unwrap();
        assert!(!t.is_admitted());
    }

    #[test]
    fn registry_load_bundled() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = IntegrationRegistry::new(dir.path());
        let count = reg.load_bundled();
        assert_eq!(count, 25);
        assert_eq!(reg.template_count(), 25);
    }

    #[test]
    fn registry_get_template() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = IntegrationRegistry::new(dir.path());
        reg.load_bundled();
        let gh = reg.get_template("github").unwrap();
        assert_eq!(gh.name, "GitHub");
        assert_eq!(gh.category, IntegrationCategory::DevTools);
    }

    #[test]
    fn registry_search() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = IntegrationRegistry::new(dir.path());
        reg.load_bundled();
        let results = reg.search("search");
        assert!(results.len() >= 2); // brave-search, exa-search
    }

    #[test]
    fn registry_install_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = IntegrationRegistry::new(dir.path());
        reg.load_bundled();

        let entry = InstalledIntegration {
            id: "github".to_string(),
            installed_at: chrono::Utc::now(),
            enabled: true,
            oauth_provider: None,
            config: HashMap::new(),
        };
        reg.install(entry).unwrap();
        assert!(reg.is_installed("github"));
        assert_eq!(reg.installed_count(), 1);

        // Double install should fail
        let entry2 = InstalledIntegration {
            id: "github".to_string(),
            installed_at: chrono::Utc::now(),
            enabled: true,
            oauth_provider: None,
            config: HashMap::new(),
        };
        assert!(reg.install(entry2).is_err());

        reg.uninstall("github").unwrap();
        assert!(!reg.is_installed("github"));
    }

    #[test]
    fn registry_to_mcp_configs() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = IntegrationRegistry::new(dir.path());
        reg.load_bundled();

        let entry = InstalledIntegration {
            id: "github".to_string(),
            installed_at: chrono::Utc::now(),
            enabled: true,
            oauth_provider: None,
            config: HashMap::new(),
        };
        reg.install(entry).unwrap();

        let configs = reg.to_mcp_configs();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "github");
    }

    #[test]
    fn registry_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = IntegrationRegistry::new(dir.path());
        reg.load_bundled();

        let entry = InstalledIntegration {
            id: "notion".to_string(),
            installed_at: chrono::Utc::now(),
            enabled: true,
            oauth_provider: None,
            config: HashMap::new(),
        };
        reg.install(entry).unwrap();

        // Load from same path
        let mut reg2 = IntegrationRegistry::new(dir.path());
        reg2.load_bundled();
        let count = reg2.load_installed().unwrap();
        assert_eq!(count, 1);
        assert!(reg2.is_installed("notion"));
    }

    #[test]
    fn registry_list_by_category() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = IntegrationRegistry::new(dir.path());
        reg.load_bundled();
        let devtools = reg.list_by_category(&IntegrationCategory::DevTools);
        assert_eq!(devtools.len(), 6);
    }

    #[test]
    fn registry_set_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = IntegrationRegistry::new(dir.path());
        reg.load_bundled();

        let entry = InstalledIntegration {
            id: "github".to_string(),
            installed_at: chrono::Utc::now(),
            enabled: true,
            oauth_provider: None,
            config: HashMap::new(),
        };
        reg.install(entry).unwrap();

        reg.set_enabled("github", false).unwrap();
        let configs = reg.to_mcp_configs();
        assert!(configs.is_empty()); // disabled = not in MCP configs
    }
}
