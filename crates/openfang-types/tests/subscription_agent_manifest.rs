use openfang_types::agent::AgentManifest;

const EXPECTED_MCP_SERVERS: &[&str] = &[
    "vibemind-db",
    "filesystem",
    "git",
    "github",
    "context7",
    "qdrant",
    "fetch",
    "brain",
    "issue-detector",
    "memory-search",
];

fn assert_subscription_manifest(raw: &str, expected_name: &str, expected_wrapper: &str) {
    assert!(
        !raw.contains("[mcp_allowed]"),
        "MCP allowlists must use AgentManifest's root mcp_servers field"
    );

    let manifest: AgentManifest = toml::from_str(raw).expect("valid AgentManifest TOML");
    assert_eq!(manifest.name, expected_name);
    assert_eq!(manifest.model.provider, "claude-code");
    assert_eq!(manifest.model.base_url.as_deref(), Some(expected_wrapper));
    assert_eq!(manifest.mcp_servers, EXPECTED_MCP_SERVERS);
}

#[test]
fn subscription_agent_templates_deserialize_with_root_mcp_allowlists() {
    assert_subscription_manifest(
        include_str!("../../../agents/brain-coder-openai/agent.toml.tmpl"),
        "brain-coder-openai",
        "openfang_opencode_wrapper.cmd",
    );
    assert_subscription_manifest(
        include_str!("../../../agents/brain-coder-anthropic/agent.toml.tmpl"),
        "brain-coder-anthropic",
        "openfang_claude_subscription_wrapper.cmd",
    );
}
