# OpenFang ↔ VibeMind Integration

How VibeMind's Brain routes intents to OpenFang agents, and how those agents use MCP tools.

## Architecture

```
Voice / Text / Channel Input
        ↓
Brain (Tahlamus :5000)
  /api/cortex/route  →  space + confidence + routing_id
        ↓
OpenFang Daemon (:4200)
  /api/agents/{id}/message
        ↓
brain-* Agent (10 agents, Claude Code)
        ↓
MCP Tools (32+ servers)
  ├── Security POCs (Python stdio)
  │   ├── network-monitor, firewall, event-log
  │   ├── botnet-detector, endpoint-hardening
  │   ├── site-verifier, issue-detector
  │   └── driver/power/process/registry/scheduled-tasks managers
  │
  └── Web/Code MCP (npm/uvx stdio)
      ├── tavily, brave-search, fetch, playwright, context7
      ├── filesystem, git, github
      ├── docker, redis, postgres
      ├── memory, qdrant, taskmanager, time
      └── brain (SSE to Brain server)
        ↓
Result + Reward signal back to Brain
```

## The 10 Brain-Routed Agents

Each agent receives a specific slice of the intent space, has its own system prompt, and a strict `mcp_allowed` allowlist.

| Agent | Routes For | MCP Tools |
|---|---|---|
| **brain-coder** | `coding.*` | filesystem, git, github, context7, qdrant, fetch |
| **brain-devops** | `desktop.*`, system ops | docker, redis, postgres, filesystem, git, process-manager, driver-manager, power-manager, scheduled-tasks, registry, env-manager |
| **brain-researcher** | `research.*` | tavily, brave-search, fetch, playwright, context7, filesystem, memory, qdrant |
| **brain-planner** | `ideas.*`, planning | taskmanager, memory, qdrant, filesystem, time |
| **brain-writer** | `bubbles.*`, `minibook.*` | filesystem, memory, context7, tavily |
| **brain-knowledge** | `rowboat.*`, queries | qdrant, memory, context7, filesystem, issue-detector |
| **brain-monitor** | health/diagnostics | docker, process-manager, event-log, issue-detector, network-monitor, redis, postgres |
| **brain-security** | security tasks | botnet-detector, endpoint-hardening, firewall, network-monitor, event-log, site-verifier, issue-detector |
| **brain-fallback** | unknown intents | brain |
| **brain-orchestrator** | multi-space | delegates to specialized agents via agent_send |

## MCP Server Catalog

### Always-On (Python stdio, no dependencies)

These ship with VibeMind and only need Python:

| Server | Purpose | Location |
|---|---|---|
| `brain` | Cognitive state queries | `http://localhost:8900` (SSE) |
| `issue-detector` | Auto-scan + GitHub issue creation | `vibemind-os/issue-detector/` |
| `network-monitor` | WiFi, ARP, DNS, TLS, ports | `security/poc_network_monitor/` |
| `firewall` | Windows Firewall management | `security/poc_firewall/` |
| `event-log` | Windows Event Log analysis | `security/poc_event_log/` |
| `botnet-detector` | DGA analysis, C2 beacons | `security/poc_botnet_detector/` |
| `endpoint-hardening` | Defender, BitLocker, secrets audit | `security/poc_endpoint_hardening/` |
| `site-verifier` | SSL, headers, CMS detection | `security/poc_site_verifier/` |
| `driver-manager` | Drivers: list, conflicts, unsigned | `system/poc_driver_manager/` |
| `power-manager` | Battery, sleep, shutdown | `system/poc_power_manager/` |
| `display-audio` | Resolution, brightness, volume | `system/poc_display_audio/` |
| `process-manager` | List, kill, priority | `system/poc_process_manager/` |
| `registry` | Read, search, backup | `system/poc_registry/` |
| `scheduled-tasks` | List, create, enable | `system/poc_scheduled_tasks/` |
| `update-manager` | Windows Update control | `system/poc_update_manager/` |
| `env-manager` | Environment variables | `system/poc_env_manager/` |
| `backup-sync` | Backup, cloud sync | `devops/poc_backup_sync/` |
| `pitch-deck` | Pitch deck generator | `business/poc_pitch_deck/` |

### Installed On Demand (npm / uvx / pip)

These require installation before first use. OpenFang lazy-loads them when an agent tries to call a tool.

**npm packages** (Node.js 18+):
```bash
npm install -g \
  @playwright/mcp \
  @modelcontextprotocol/server-filesystem \
  @modelcontextprotocol/server-github \
  @modelcontextprotocol/server-brave-search \
  @modelcontextprotocol/server-redis \
  @modelcontextprotocol/server-postgres \
  @modelcontextprotocol/server-memory \
  @upstash/context7-mcp \
  @kazuph/mcp-taskmanager \
  tavily-mcp
```

**uvx packages** (uv tool):
```bash
uv tool install mcp-server-fetch
uv tool install mcp-server-git
uv tool install mcp-server-time
uv tool install mcp-server-qdrant
uv tool install docker-mcp
```

## Environment Variables

Add to `~/.openfang/secrets.env` or your shell env:

```bash
# LLM Providers
OPENROUTER_API_KEY=sk-or-v1-...
GROQ_API_KEY=gsk-...
ANTHROPIC_API_KEY=sk-ant-...

# Web Search
TAVILY_API_KEY=tvly-...
BRAVE_API_KEY=BSA-...

# GitHub
GITHUB_PERSONAL_ACCESS_TOKEN=ghp_...

# Database (optional)
POSTGRES_CONNECTION_STRING=postgresql://user:pass@localhost/db

# Supermemory (optional)
SUPERMEMORY_API_KEY=sk-...

# Channels
TELEGRAM_BOT_TOKEN=123:ABC...
```

## Startup

### Option 1: Via VibeMind Electron App (recommended)

```bash
cd vibemind-os/voice && npm start
```

The Electron app auto-starts:
- Python backend (`electron_backend.py`)
- Brain server (`python -m web.brain_server`, port 5000) — headless
- OpenFang daemon (`openfang.exe start --config openfang.vibemind.toml`, port 4200) — headless

### Option 2: Manual (for debugging)

```bash
# Terminal 1: Brain
cd vibemind-os/brain/the_brain
python -m web.brain_server

# Terminal 2: OpenFang with VibeMind config
cd vibemind-os/openfang
./target/release/openfang.exe start --config openfang.vibemind.toml

# Terminal 3: Verify
curl -s http://localhost:5000/api/cortex/route/stats
curl -s http://localhost:4200/api/health
curl -s http://localhost:4200/api/agents | grep brain-
```

## Testing the Pipeline

```bash
# Test Brain routing with context
curl -X POST http://localhost:5000/api/cortex/route \
  -H "Content-Type: application/json" \
  -d '{
    "user_text": "Check if all Docker containers are healthy",
    "event_type": "desktop.check",
    "context": {"current_space": "desktop", "active_task_count": 0}
  }'
# Expect: {"primary_space": "desktop", "confidence": 0.XX, "routing_id": "rt_..."}

# Direct message to brain-devops agent
curl -X POST http://localhost:4200/api/agents/<brain-devops-id>/message \
  -H "Content-Type: application/json" \
  -d '{
    "message": "[VibeMind Context]\nSpace: VibeMind Infrastructure\n[End Context]\n\nList all running Docker containers and report their health."
  }'

# The agent will call docker MCP tools automatically
```

## Self-Healing Loop

The `issue-detector` MCP server runs a background watcher that:
1. Monitors Windows Event Log for errors
2. Watches for process crashes
3. Watches a drop folder for JSON triggers
4. Pushes findings to GitHub as issues
5. Claude Code pulls issues and fixes them

Any brain-* agent can trigger this:
```
brain-monitor → issue-detector.scan_all_spaces(dry_run=false)
brain-monitor → issue-detector.push_to_github(finding_id)
```

## Adding a New Agent

1. Create `openfang/agents/brain-yourdomain/agent.toml`
2. Define `[model]` with system_prompt
3. Set `[mcp_allowed] servers = [...]` with tools the agent may use
4. Restart OpenFang
5. Update `BrainOpenFangBridge` in `voice/python/swarm/routing/brain_openfang_bridge.py` to map your space to the new agent name

## Adding a New MCP Server

1. Add block to `openfang.vibemind.toml`:
   ```toml
   [[mcp_servers]]
   name = "your-server"
   timeout_secs = 15
   [mcp_servers.transport]
   type = "stdio"
   command = "python"
   args = ["path/to/mcp_server.py"]
   ```
2. Add the name to the relevant agent's `[mcp_allowed] servers` list
3. Restart OpenFang

## Troubleshooting

**Agent can't find MCP tool:**
- Check the agent has the server in `[mcp_allowed]`
- Check the server name matches exactly (no typos)
- Restart OpenFang to reload config

**MCP server fails to start:**
- Check the `command` is in PATH
- For npm packages: `npm install -g <package>`
- For uvx packages: `uv tool install <package>`
- Check logs in `~/.openfang/data/logs/`

**Brain returns low confidence:**
- Brain needs shadow training data — let VibeMind voice run for a while first
- Check `/api/cortex/route/stats` for per-space centroid norms
- Low confidence is expected until Brain has seen ~100+ examples per space

**OpenFang port conflict:**
- Default port is 4200 (not 50051 — that's for Google Cloud)
- Change in `[network] listen_addr = "127.0.0.1:4200"` in config
