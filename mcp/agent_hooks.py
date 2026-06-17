"""
Agent lifecycle hooks for OpenFang spawn (pre/post).

Inspired by ruvnet/agentic-flow's `.claude/agents/*.md` `hooks.pre`/`hooks.post`
shell strings. OpenFang's Rust `AgentManifest` has no `[hooks]` field and does
NOT use `#[serde(deny_unknown_fields)]`, so an extra `[hooks]` block in the TOML
is silently ignored by the kernel. We parse it here in the Python MCP layer and
run the shell commands around the spawn HTTP call — no Rust rebuild needed.

Manifest addition (optional, additive, ignored by the kernel):

    [hooks]
    pre  = "python scripts/notify.py up"      # runs BEFORE spawn; non-zero blocks
    post = "python scripts/notify.py done"    # runs AFTER successful spawn

The hook env (AGENT_NAME, OPENFANG_URL) is always set in the child process, so
reading it from inside an interpreter (os.environ / process.env) works on every
platform. SHELL variable expansion in the command string itself is platform-
specific because the command runs via the OS shell: use ``$AGENT_NAME`` on
POSIX (bash/sh) and ``%AGENT_NAME%`` on Windows (cmd.exe). A ``$AGENT_NAME``
literal will NOT expand under cmd.exe (verified live 2026-06-17). Prefer a
script that reads the env var over relying on shell expansion if hooks must be
cross-platform.

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

# Env vars that change how the dynamic loader / interpreters bootstrap and are a
# classic privilege-escalation / code-injection vector. A hook's [hooks] env
# block may NOT override these — even though hooks come from local trusted TOML,
# this is cheap defense-in-depth: it means a hook env can never silently inject
# LD_PRELOAD / NODE_OPTIONS / BASH_ENV etc. into the spawned shell or its
# children. PATH is included so a hook cannot redirect bare command lookups.
_BLOCKED_ENV_KEYS = frozenset({"PATH", "IFS", "ENV", "BASH_ENV"})
_BLOCKED_ENV_PREFIXES = (
    "LD_", "DYLD_", "PYTHON", "NODE_OPTIONS", "NODE_PATH", "RUBYOPT",
    "PERL5OPT", "GCONV_PATH", "GIT_SSH", "GIT_EXTERNAL_DIFF",
)


def _safe_hook_env(env: Optional[Dict[str, str]]) -> Dict[str, str]:
    """Filter a hook-supplied env mapping, dropping loader-hijack keys.

    Blocked keys are skipped silently (logged once) rather than raising — a hook
    that tries to set PATH/LD_PRELOAD just doesn't get it; the spawn proceeds.
    """
    safe: Dict[str, str] = {}
    for k, v in (env or {}).items():
        ku = k.upper()
        if ku in _BLOCKED_ENV_KEYS or any(ku.startswith(p) for p in _BLOCKED_ENV_PREFIXES):
            _log(f"refused to set loader-sensitive hook env var: {k}")
            continue
        safe[k] = str(v)
    return safe


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

    # Trust boundary: `command` comes from a local agent.toml that the operator
    # placed in ~/.openfang/agents/ — same trust level as the manifest the
    # kernel already executes. Hooks are DESIGNED to run shell syntax (pipes,
    # $VAR expansion — see module docstring), so shell=True is intentional, not
    # an oversight. Do NOT pass attacker-controlled TOML to the spawn tools.
    # We still filter the hook-supplied env (loader-hijack defense, see above);
    # the inherited process env is trusted as-is.
    merged_env = dict(os.environ)
    merged_env.update(_safe_hook_env(env))

    try:
        proc = subprocess.run(
            command,
            shell=True,  # noqa: S602 — intentional; see trust-boundary note above
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
