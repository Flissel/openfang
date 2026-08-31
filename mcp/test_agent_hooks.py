"""
Unit tests for agent_hooks (pre/post lifecycle hooks around OpenFang spawn).

Self-contained: no live OpenFang daemon, no MCP. Shell execution is exercised
with trivial, cross-platform commands and a temp file so the test asserts the
hook actually ran.

Run:  python -m pytest test_agent_hooks.py -q   (from vibemind-os/openfang/mcp)
"""

import os
import sys
import tempfile

import pytest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from agent_hooks import HookResult, _safe_hook_env, extract_hooks, run_hook  # noqa: E402


# ─── extract_hooks ─────────────────────────────────────────────────────────

def test_extract_hooks_none_when_absent():
    toml = '[model]\nprovider = "groq"\nmodel = "llama"\n'
    hooks = extract_hooks(toml)
    assert hooks == {"pre": None, "post": None}


def test_extract_hooks_reads_pre_and_post():
    toml = (
        'name = "x"\n'
        "[hooks]\n"
        'pre = "echo before"\n'
        'post = "echo after"\n'
    )
    hooks = extract_hooks(toml)
    assert hooks["pre"] == "echo before"
    assert hooks["post"] == "echo after"


def test_extract_hooks_partial():
    toml = '[hooks]\npre = "echo only-pre"\n'
    hooks = extract_hooks(toml)
    assert hooks["pre"] == "echo only-pre"
    assert hooks["post"] is None


def test_extract_hooks_invalid_toml_is_safe():
    # Broken TOML must not raise — spawning should still proceed without hooks.
    hooks = extract_hooks("this is = = not toml [[[")
    assert hooks == {"pre": None, "post": None}


def test_extract_hooks_empty_string():
    assert extract_hooks("") == {"pre": None, "post": None}


# ─── run_hook ──────────────────────────────────────────────────────────────

def test_run_hook_success():
    res = run_hook("python -c \"print('hi')\"", env={}, timeout=10)
    assert isinstance(res, HookResult)
    assert res.exit_code == 0
    assert "hi" in res.output


def test_run_hook_nonzero_exit():
    res = run_hook("python -c \"import sys; sys.exit(3)\"", env={}, timeout=10)
    assert res.exit_code == 3


def test_run_hook_writes_side_effect(tmp_path):
    marker = tmp_path / "hook_ran.txt"
    # Use python for cross-platform file write (no shell-builtin differences).
    cmd = f"python -c \"open(r'{marker}','w').write('AGENT='+__import__('os').environ.get('AGENT_NAME',''))\""
    res = run_hook(cmd, env={"AGENT_NAME": "brain-coder"}, timeout=10)
    assert res.exit_code == 0
    assert marker.read_text() == "AGENT=brain-coder"


def test_run_hook_timeout():
    # Sleeps longer than the timeout → must report a timeout, not hang/raise.
    res = run_hook("python -c \"import time; time.sleep(5)\"", env={}, timeout=1)
    assert res.exit_code != 0
    assert res.timed_out is True


def test_run_hook_empty_command_is_noop():
    res = run_hook("", env={}, timeout=10)
    assert res.exit_code == 0
    assert res.skipped is True


# ─── _safe_hook_env (loader-hijack defense) ────────────────────────────────

def test_safe_hook_env_passes_normal_vars():
    out = _safe_hook_env({"AGENT_NAME": "x", "OPENFANG_URL": "http://127.0.0.1:4200"})
    assert out == {"AGENT_NAME": "x", "OPENFANG_URL": "http://127.0.0.1:4200"}


@pytest.mark.parametrize("key", [
    "LD_PRELOAD", "DYLD_INSERT_LIBRARIES", "PYTHONPATH", "PYTHONSTARTUP",
    "NODE_OPTIONS", "BASH_ENV", "PATH", "IFS", "GIT_SSH_COMMAND",
])
def test_safe_hook_env_blocks_loader_vars(key):
    out = _safe_hook_env({key: "/evil", "AGENT_NAME": "ok"})
    assert key not in out
    assert out == {"AGENT_NAME": "ok"}


def test_safe_hook_env_blocking_is_case_insensitive():
    out = _safe_hook_env({"ld_preload": "/evil", "Path": "/evil"})
    assert out == {}


def test_run_hook_cannot_override_path(tmp_path):
    # A hook env that tries to hijack PATH must not affect the spawned shell:
    # python (resolved via the real PATH) still runs and reports the REAL,
    # non-hijacked PATH back.
    marker = tmp_path / "p.txt"
    cmd = f"python -c \"open(r'{marker}','w').write(__import__('os').environ.get('PATH',''))\""
    res = run_hook(cmd, env={"PATH": "/totally/bogus"}, timeout=10)
    assert res.exit_code == 0
    assert marker.read_text() != "/totally/bogus"
