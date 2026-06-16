"""
Agent lifecycle hooks for OpenFang spawn (pre/post).

Inspired by ruvnet/agentic-flow's `.claude/agents/*.md` `hooks.pre`/`hooks.post`
shell strings. OpenFang's Rust `AgentManifest` has no `[hooks]` field and does
NOT use `#[serde(deny_unknown_fields)]`, so an extra `[hooks]` block in the TOML
is silently ignored by the kernel. We parse it here in the Python MCP layer and
run the shell commands around the spawn HTTP call — no Rust rebuild needed.

Manifest addition (optional, additive, ignored by the kernel):

    [hooks]
    pre  = "echo spawning $AGENT_NAME"        # runs BEFORE spawn; non-zero blocks
    post = "python scripts/notify.py up"      # runs AFTER successful spawn

Scope / honesty: these fire only when an agent is spawned via the MCP tools
(openfang_agent_spawn_from_template / _from_toml). Agents loaded directly by the
daemon at boot do NOT trigger these. For full lifecycle coverage (per-tool-call,
AgentLoopEnd) the Rust HookEvent enum would need wiring — out of scope here.

Security: hook commands come from local, trusted agent.toml files (same trust
level as the manifest itself). They run via the shell. Do not feed untrusted
TOML to the spawn tools.
"""

from __future__ import annotations

import subprocess
import sys
import tomllib
from dataclasses import dataclass
from typing import Dict, Optional

DEFAULT_HOOK_TIMEOUT_S = 30


@dataclass
class HookResult:
    exit_code: int
    output: str = ""
    timed_out: bool = False
    skipped: bool = False


def extract_hooks(manifest_toml: str) -> Dict[str, Optional[str]]:
    """Parse the optional [hooks] block. Never raises.

    Returns {"pre": str|None, "post": str|None}. Invalid/empty TOML yields both
    None so the caller proceeds to spawn without hooks.
    """
    result: Dict[str, Optional[str]] = {"pre": None, "post": None}
    if not manifest_toml or not manifest_toml.strip():
        return result
    try:
        data = tomllib.loads(manifest_toml)
    except (tomllib.TOMLDecodeError, ValueError, TypeError):
        return result
    hooks = data.get("hooks")
    if not isinstance(hooks, dict):
        return result
    pre = hooks.get("pre")
    post = hooks.get("post")
    result["pre"] = pre if isinstance(pre, str) and pre.strip() else None
    result["post"] = post if isinstance(post, str) and post.strip() else None
    return result


def run_hook(
    command: str,
    env: Dict[str, str],
    timeout: int = DEFAULT_HOOK_TIMEOUT_S,
) -> HookResult:
    """Run a hook command via the shell. Never raises.

    `env` is merged onto the current process environment (so $AGENT_NAME etc.
    are available). Returns a HookResult with the exit code, combined output,
    and timed_out/skipped flags. An empty command is a no-op success.
    """
    if not command or not command.strip():
        return HookResult(exit_code=0, skipped=True)

    import os

    merged_env = dict(os.environ)
    merged_env.update({k: str(v) for k, v in (env or {}).items()})

    try:
        proc = subprocess.run(
            command,
            shell=True,
            env=merged_env,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or "") + (e.stderr or "")
        return HookResult(exit_code=124, output=out, timed_out=True)
    except Exception as e:  # noqa: BLE001 — hooks must never crash the spawn path
        return HookResult(exit_code=1, output=f"hook execution error: {e}")

    output = (proc.stdout or "") + (proc.stderr or "")
    return HookResult(exit_code=proc.returncode, output=output)


def _log(msg: str) -> None:
    print(f"[agent-hooks] {msg}", file=sys.stderr, flush=True)
