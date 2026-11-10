//! Prueft, dass alle Vibemind-Integrationsvorlagen (`integrations/*.toml`) laden.

use openfang_extensions::registry::IntegrationRegistry;
use std::path::PathBuf;

#[test]
fn alle_vibemind_vorlagen_laden() {
    let dir = match std::env::var_os("VIBEMIND_INTEGRATIONS_DIR") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../integrations"),
    };
    if !dir.is_dir() {
        eprintln!("Vorlagenordner fehlt ({}), Test uebersprungen", dir.display());
        return;
    }
    let mut reg = IntegrationRegistry::new(&std::env::temp_dir().join("vibemind-vorlagen-test"));
    let report = reg.load_template_dirs(&[dir]);
    assert!(report.skipped.is_empty(), "Vorlagen mit Fehlern: {:?}", report.skipped);
    assert_eq!(report.unreadable_dirs, 0);
    for t in reg.list_templates() {
        if let Some(c) = &t.catalog {
            assert!(
                c.replaces_openai_plugin.as_deref().is_some_and(|s| !s.trim().is_empty()),
                "Vorlage {} ohne replaces_openai_plugin",
                t.id
            );
        }
    }
}
