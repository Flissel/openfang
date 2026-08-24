use openfang_types::agent::AgentManifest;
use openfang_types::config::{KernelConfig, McpTransportEntry};

const EXPECTED_SERVERS: [&str; 2] = ["laura", "vibemind-db"];

fn repository_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn vibemind_configs_register_the_laura_mcp_server() {
    for relative_path in ["openfang.vibemind.toml.template", "openfang.vibemind.toml"] {
        let contents = std::fs::read_to_string(repository_root().join(relative_path)).unwrap();
        let config: KernelConfig = toml::from_str(&contents).unwrap();
        let laura = config
            .mcp_servers
            .iter()
            .find(|server| server.name == "laura")
            .unwrap_or_else(|| panic!("{relative_path} must declare the laura MCP server"));

        assert_eq!(laura.env, ["LAURA_TOKEN"]);
        match &laura.transport {
            McpTransportEntry::Stdio { command, args } => {
                assert_eq!(command, "uv");
                assert_eq!(args.first().map(String::as_str), Some("run"));
                assert_eq!(args.get(1).map(String::as_str), Some("--directory"));
                assert!(
                    args.get(2).is_some_and(|path| path
                        .replace('\\', "/")
                        .ends_with("/vibemind-os/spaces/video/laura/services/mcp")),
                    "{relative_path} must point at Laura's versioned MCP service"
                );
                assert_eq!(args.get(3).map(String::as_str), Some("laura-mcp"));
            }
            transport => panic!("{relative_path} must use stdio for Laura, got {transport:?}"),
        }
    }
}

#[test]
fn brain_video_uses_the_effective_top_level_laura_scope() {
    let relative_path = "agents/brain-video/agent.toml";
    let contents = std::fs::read_to_string(repository_root().join(relative_path)).unwrap();
    let document: toml::Value = toml::from_str(&contents).unwrap();
    let manifest: AgentManifest = toml::from_str(&contents).unwrap();

    assert!(document.get("mcp_allowed").is_none());
    assert_eq!(manifest.mcp_servers, EXPECTED_SERVERS);
}
