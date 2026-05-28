# openfang-agents MCP server

A stdio-MCP server that exposes OpenFang's agent-CRUD HTTP API (`:4200/api/agents/*`) as 8 MCP tools — so Claude Code, acpx, or any MCP client can spawn, configure and kill OpenFang agents by name instead of clicking through the dashboard at `:4200`.

This is a **direct HTTP wrap**, not a bridge through OpenFang's own `:4200/mcp` aggregator. Rationale: agent-CRUD is the kernel control layer; routing it through the aggregator (OpenFang → OpenFang) is debug-hostile and semantically confused. See `scripts/mcp_stdio_bridge.py` for the aggregator bridge — that's a separate concern.

## Tools (Builder-Fokus, 8)

| Tool | HTTP |
|---|---|
| `openfang_agents_list` | `GET /api/agents` |
| `openfang_agent_get` | `GET /api/agents/{id}` |
| `openfang_agent_spawn_from_template` | `POST /api/agents` `{template}` |
| `openfang_agent_spawn_from_toml` | `POST /api/agents` `{manifest_toml}` |
| `openfang_agent_patch` | `PATCH /api/agents/{id}` `{name?, description?, model?, provider?}` |
| `openfang_agent_set_tools` | `PUT /api/agents/{id}/tools` `{tool_allowlist?, tool_blocklist?}` |
| `openfang_agent_set_mcp_servers` | `PUT /api/agents/{id}/mcp_servers` `{mcp_servers}` |
| `openfang_agent_kill` | `DELETE /api/agents/{id}` |

Session-mgmt / files / identity / clone / model-switch stay in the `:4200` dashboard — the iframe sub-tab in AgentFarm covers those.

## Env

| Var | Default |
|---|---|
| `OPENFANG_URL` | `http://127.0.0.1:4200` |
| `OPENFANG_HTTP_TIMEOUT` | `60` (seconds) |

## Registration in Claude Code

`.mcp.json` at the repo root:

```json
"openfang-agents": {
  "type": "stdio",
  "command": "C:\\Users\\User\\Desktop\\Vibemind_V1\\.venv\\Scripts\\python.exe",
  "args": ["C:\\Users\\User\\Desktop\\Vibemind_V1\\vibemind-os\\openfang\\mcp\\openfang_agents_server.py"],
  "env": { "OPENFANG_URL": "http://127.0.0.1:4200", "OPENFANG_HTTP_TIMEOUT": "60" }
}
```

After adding, restart Claude Code; `/mcp` should list `openfang-agents` with 8 tools.

## Standalone smoketest

```powershell
# Prereq: OpenFang up on :4200
curl -s http://127.0.0.1:4200/api/health   # -> 200

# initialize + tools/list
$init = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}'
$list = '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
"$init`n$list" | & .venv\Scripts\python.exe vibemind-os\openfang\mcp\openfang_agents_server.py
# -> two JSON-RPC frames on stdout; tools/list has 8 entries

# real call
$call = '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"openfang_agents_list","arguments":{}}}'
$call | & .venv\Scripts\python.exe vibemind-os\openfang\mcp\openfang_agents_server.py
# -> agents list JSON
```

## Error behavior

Every tool returns a `TextContent` with an `error:` prefix on network failure, timeout, or HTTP non-2xx — never raises. The LLM can read the error and react (e.g. "OpenFang is down, ask the user to start it").

## See also

- Plan: `C:\Users\User\.claude\plans\plan-das-mal-soft-wren.md`
- Template wrapper: `vibemind-os/spaces/rowboat/mcp/rowboat_chat_server.py` (same pattern, single tool)
- Aggregator bridge (different purpose): `vibemind-os/openfang/scripts/mcp_stdio_bridge.py`
- Brain capability entry: `vibemind-os/brain/the_brain/data/capabilities.yaml` → `openfang_agent_create`
- Skill doc: `vibemind-os/skills/openfang/agent-create/SKILL.md`

<!-- fungus-hook-v6-test-1779358471 -->
