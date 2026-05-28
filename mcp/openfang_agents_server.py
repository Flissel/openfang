"""
openfang-agents MCP server.

Wraps OpenFang's agent-CRUD HTTP API (default :4200) as 8 stdio-MCP tools so
Claude Code (or any MCP client) can spawn, configure and kill OpenFang agents
by name — same operations the :4200 dashboard does via click. Builder-focused
surface; session/file/identity/clone ops stay in the dashboard.

Direct HTTP wrap, NOT a bridge through OpenFang's own :4200/mcp aggregator —
that would be OpenFang calling itself for self-modification (debug-hostile +
semantically confused). Same pattern as rowboat_chat_server.py.

Env (read once at startup; main .env supplies these via .mcp.json):
  OPENFANG_URL          default http://127.0.0.1:4200
  OPENFANG_HTTP_TIMEOUT default 60 (seconds)

Degrades gracefully: every connection / timeout / non-200 returns an error
TextContent string. Never raises — the LLM can react.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys

import requests
from mcp.server import Server
from mcp.server.stdio import stdio_server
from mcp.types import TextContent, Tool

# ── config ───────────────────────────────────────────────────────────────────

DEFAULT_URL = "http://127.0.0.1:4200"
DEFAULT_TIMEOUT_SECONDS = 60

server = Server("openfang-agents")


def _cfg() -> tuple[str, float]:
    url = (os.environ.get("OPENFANG_URL") or DEFAULT_URL).rstrip("/")
    try:
        timeout = float(os.environ.get("OPENFANG_HTTP_TIMEOUT") or DEFAULT_TIMEOUT_SECONDS)
    except ValueError:
        timeout = float(DEFAULT_TIMEOUT_SECONDS)
    return url, timeout


def _log(msg: str) -> None:
    """Log to stderr — stdout is reserved for JSON-RPC frames."""
    print(f"[openfang-agents-mcp] {msg}", file=sys.stderr, flush=True)


def _err(msg: str) -> list[TextContent]:
    return [TextContent(type="text", text=f"error: {msg}")]


def _ok(payload) -> list[TextContent]:
    if isinstance(payload, (dict, list)):
        text = json.dumps(payload, ensure_ascii=False, indent=2)
    else:
        text = str(payload)
    return [TextContent(type="text", text=text)]


def _http(method: str, path: str, json_body: dict | None = None) -> list[TextContent]:
    """
    Make a single HTTP call to OpenFang and turn the response into MCP
    TextContent. All error modes (network down, timeout, non-200) become a
    structured error message, never an exception.
    """
    url, timeout = _cfg()
    full_url = f"{url}{path}"
    try:
        resp = requests.request(
            method=method,
            url=full_url,
            json=json_body,
            timeout=timeout,
            headers={"Content-Type": "application/json"} if json_body is not None else {},
        )
    except requests.exceptions.ConnectionError:
        return _err(
            f"cannot reach OpenFang at {url} — is the daemon running on :4200? "
            f"(start: Vibemind.debug.ps1 -Modules openfang, or "
            f"target/release/openfang.exe start --config openfang.vibemind.toml)"
        )
    except requests.exceptions.Timeout:
        return _err(f"OpenFang timed out after {timeout}s on {method} {path}")
    except Exception as e:  # noqa: BLE001
        return _err(f"{method} {path} failed: {e}")

    if resp.status_code >= 400:
        body = (resp.text or "")[:500]
        return _err(f"OpenFang returned HTTP {resp.status_code} on {method} {path}: {body}")

    if not resp.content:
        return _ok({"status": "ok"})

    try:
        return _ok(resp.json())
    except ValueError:
        return _ok(resp.text)


# ── tool registry ────────────────────────────────────────────────────────────

@server.list_tools()
async def list_tools() -> list[Tool]:
    return [
        Tool(
            name="openfang_agents_list",
            description=(
                "List all OpenFang agents currently running on the daemon (:4200). "
                "Returns each agent's id, name, state (Created/Running/Suspended/"
                "Terminated/Crashed), model_name, model_provider and ready flag. "
                "Use this first to see what's already alive before spawning a "
                "duplicate, or to find an id for the get/patch/kill tools."
            ),
            inputSchema={"type": "object", "properties": {}, "required": []},
        ),
        Tool(
            name="openfang_agent_get",
            description=(
                "Fetch the full AgentEntry for ONE agent by id — includes manifest "
                "(model config, capabilities, resource limits, tools, mcp_servers, "
                "system_prompt), state, mode, session_id, identity (emoji/avatar/"
                "archetype) and lineage. Use after a list to inspect or before a "
                "patch to see current values."
            ),
            inputSchema={
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "the agent's id (UUID string from openfang_agents_list)",
                    },
                },
                "required": ["agent_id"],
            },
        ),
        Tool(
            name="openfang_agent_spawn_from_template",
            description=(
                "Spawn a new OpenFang agent from a named template under "
                "~/.openfang/agents/{template}/agent.toml. Common templates "
                "include: brain-coder, brain-researcher, brain-orchestrator, "
                "brain-knowledge, brain-security, brain-planner, brain-devops, "
                "analyst, architect, assistant (49 total). Returns the new "
                "agent's id and name. Agent lives in memory only — restart of "
                "OpenFang loses it (by design)."
            ),
            inputSchema={
                "type": "object",
                "properties": {
                    "template": {
                        "type": "string",
                        "description": "template name, e.g. 'brain-coder', 'brain-researcher'",
                    },
                },
                "required": ["template"],
            },
        ),
        Tool(
            name="openfang_agent_spawn_from_toml",
            description=(
                "Spawn a new OpenFang agent from a raw TOML manifest string "
                "(when you need custom config not covered by a template). The "
                "manifest_toml must include at minimum [agent] name, [model] "
                "provider+model, and is parsed as openfang_types::AgentManifest. "
                "Prefer spawn_from_template + patch unless you know the schema."
            ),
            inputSchema={
                "type": "object",
                "properties": {
                    "manifest_toml": {
                        "type": "string",
                        "description": "complete TOML manifest as a single string",
                    },
                },
                "required": ["manifest_toml"],
            },
        ),
        Tool(
            name="openfang_agent_patch",
            description=(
                "Update mutable fields of a running agent in-place: display name, "
                "description, model (with optional provider override). Other fields "
                "(system_prompt, tools, mcp_servers, capabilities) need their own "
                "dedicated endpoints or a full kill+respawn. All fields are "
                "optional — send only what you want to change."
            ),
            inputSchema={
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string", "description": "agent id (UUID)"},
                    "name": {"type": "string", "description": "new display name"},
                    "description": {"type": "string", "description": "new description"},
                    "model": {
                        "type": "string",
                        "description": "model id, e.g. 'claude-sonnet-4-20250514' or 'llama-3.1-70b-versatile'",
                    },
                    "provider": {
                        "type": "string",
                        "description": "provider id when changing model, e.g. 'anthropic', 'groq', 'openai'",
                    },
                },
                "required": ["agent_id"],
            },
        ),
        Tool(
            name="openfang_agent_set_tools",
            description=(
                "Set the agent's tool allow/block lists (kernel-level filter on "
                "which tool calls the agent is allowed to make). Provide at least "
                "one of tool_allowlist / tool_blocklist — empty body is rejected. "
                "Allowlist semantics: if set, ONLY listed tools are callable; "
                "blocklist: listed tools are denied."
            ),
            inputSchema={
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string", "description": "agent id (UUID)"},
                    "tool_allowlist": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "list of tool names the agent may call (e.g. ['file_read','web_fetch'])",
                    },
                    "tool_blocklist": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "list of tool names the agent must NOT call (e.g. ['shell_exec'])",
                    },
                },
                "required": ["agent_id"],
            },
        ),
        Tool(
            name="openfang_agent_set_mcp_servers",
            description=(
                "Set which MCP servers the agent may connect to (from the "
                "daemon-level mcp server registry, see GET /api/mcp/servers for "
                "available names). Pass an empty list to deny all; pass specific "
                "names to allowlist."
            ),
            inputSchema={
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string", "description": "agent id (UUID)"},
                    "mcp_servers": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "list of mcp server names, e.g. ['fungus-search','vibemind-db']",
                    },
                },
                "required": ["agent_id", "mcp_servers"],
            },
        ),
        Tool(
            name="openfang_agent_kill",
            description=(
                "Terminate a running OpenFang agent and unregister it from the "
                "kernel registry. Idempotent on already-dead agents (returns "
                "status from the daemon). Use to clean up after a spawn test or "
                "when an agent has gone bad."
            ),
            inputSchema={
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string", "description": "agent id (UUID)"},
                },
                "required": ["agent_id"],
            },
        ),
    ]


# ── tool dispatch ────────────────────────────────────────────────────────────

@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[TextContent]:
    args = arguments or {}

    if name == "openfang_agents_list":
        return _http("GET", "/api/agents")

    if name == "openfang_agent_get":
        agent_id = (args.get("agent_id") or "").strip()
        if not agent_id:
            return _err("agent_id is required")
        return _http("GET", f"/api/agents/{agent_id}")

    if name == "openfang_agent_spawn_from_template":
        template = (args.get("template") or "").strip()
        if not template:
            return _err("template is required (e.g. 'brain-coder')")
        return _http("POST", "/api/agents", {"template": template})

    if name == "openfang_agent_spawn_from_toml":
        manifest_toml = args.get("manifest_toml") or ""
        if not manifest_toml.strip():
            return _err("manifest_toml is required (full agent manifest as TOML string)")
        return _http("POST", "/api/agents", {"manifest_toml": manifest_toml})

    if name == "openfang_agent_patch":
        agent_id = (args.get("agent_id") or "").strip()
        if not agent_id:
            return _err("agent_id is required")
        body: dict = {}
        for key in ("name", "description", "model", "provider"):
            val = args.get(key)
            if val is not None and str(val).strip():
                body[key] = val
        if not body:
            return _err("nothing to patch — provide at least one of: name, description, model, provider")
        return _http("PATCH", f"/api/agents/{agent_id}", body)

    if name == "openfang_agent_set_tools":
        agent_id = (args.get("agent_id") or "").strip()
        if not agent_id:
            return _err("agent_id is required")
        body: dict = {}
        allow = args.get("tool_allowlist")
        block = args.get("tool_blocklist")
        if isinstance(allow, list):
            body["tool_allowlist"] = [str(x) for x in allow]
        if isinstance(block, list):
            body["tool_blocklist"] = [str(x) for x in block]
        if not body:
            return _err("provide at least one of tool_allowlist / tool_blocklist (both as arrays of strings)")
        return _http("PUT", f"/api/agents/{agent_id}/tools", body)

    if name == "openfang_agent_set_mcp_servers":
        agent_id = (args.get("agent_id") or "").strip()
        if not agent_id:
            return _err("agent_id is required")
        servers = args.get("mcp_servers")
        if not isinstance(servers, list):
            return _err("mcp_servers must be a list of strings (use [] to deny all)")
        return _http(
            "PUT",
            f"/api/agents/{agent_id}/mcp_servers",
            {"mcp_servers": [str(x) for x in servers]},
        )

    if name == "openfang_agent_kill":
        agent_id = (args.get("agent_id") or "").strip()
        if not agent_id:
            return _err("agent_id is required")
        return _http("DELETE", f"/api/agents/{agent_id}")

    return _err(f"unknown tool: {name}")


# ── entry point ──────────────────────────────────────────────────────────────

async def _main() -> None:
    url, timeout = _cfg()
    _log(f"starting; OPENFANG_URL={url} timeout={timeout}s")
    async with stdio_server() as (read_stream, write_stream):
        await server.run(
            read_stream,
            write_stream,
            server.create_initialization_options(),
        )


if __name__ == "__main__":
    try:
        asyncio.run(_main())
    except KeyboardInterrupt:
        sys.exit(0)
