"""
Integration test for _spawn_with_hooks in openfang_agents_server.

Mocks only the leaf HTTP call (_http); the hook extraction, pre-blocks-spawn
logic, post-after-success logic, and output folding are the real code. No live
OpenFang daemon.

Run:  python -m pytest test_spawn_with_hooks.py -q   (from vibemind-os/openfang/mcp)
"""

import os
import sys

import pytest
from mcp.types import TextContent

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import openfang_agents_server as srv  # noqa: E402


def _ok_http(payload_text):
    return [TextContent(type="text", text=payload_text)]


@pytest.fixture
def fake_http(monkeypatch):
    calls = []

    def _fake(method, path, json_body=None):
        calls.append((method, path, json_body))
        return _ok_http('{"agent_id": "abc-123", "name": "x"}')

    monkeypatch.setattr(srv, "_http", _fake)
    return calls


def test_no_hooks_plain_spawn(fake_http):
    toml = '[model]\nprovider = "groq"\nmodel = "llama"\n'
    out = srv._spawn_with_hooks({"manifest_toml": toml}, toml, None)
    assert len(fake_http) == 1  # spawn happened
    assert "abc-123" in out[0].text
    assert "hook" not in out[0].text.lower()  # no hook notes


def test_pre_hook_success_then_spawn(fake_http):
    toml = (
        'name = "demo"\n'
        "[hooks]\n"
        "pre = \"python -c \\\"print('PRE-OK')\\\"\"\n"
    )
    out = srv._spawn_with_hooks({"manifest_toml": toml}, toml, None)
    assert len(fake_http) == 1  # spawn DID run
    assert "abc-123" in out[0].text
    assert "pre-hook ok" in out[0].text
    assert "PRE-OK" in out[0].text


def test_pre_hook_failure_blocks_spawn(fake_http):
    toml = (
        'name = "demo"\n'
        "[hooks]\n"
        "pre = \"python -c \\\"import sys; sys.exit(2)\\\"\"\n"
    )
    out = srv._spawn_with_hooks({"manifest_toml": toml}, toml, None)
    assert len(fake_http) == 0  # spawn was BLOCKED
    assert out[0].text.startswith("error:")
    assert "pre-hook" in out[0].text
    assert "spawn aborted" in out[0].text


def test_post_hook_fires_after_success(fake_http):
    toml = (
        'name = "demo"\n'
        "[hooks]\n"
        "post = \"python -c \\\"print('POST-RAN')\\\"\"\n"
    )
    out = srv._spawn_with_hooks({"manifest_toml": toml}, toml, None)
    assert len(fake_http) == 1
    assert "abc-123" in out[0].text
    assert "post-hook ok" in out[0].text
    assert "POST-RAN" in out[0].text


def test_post_hook_skipped_when_spawn_fails(monkeypatch):
    # _http returns the error convention → post-hook must NOT run, and the
    # error is returned unchanged.
    def _fail(method, path, json_body=None):
        return [TextContent(type="text", text="error: OpenFang returned HTTP 500")]

    monkeypatch.setattr(srv, "_http", _fail)
    toml = (
        'name = "demo"\n'
        "[hooks]\n"
        "post = \"python -c \\\"open('SHOULD_NOT_EXIST.txt','w')\\\"\"\n"
    )
    out = srv._spawn_with_hooks({"manifest_toml": toml}, toml, None)
    assert out[0].text.startswith("error:")
    assert "post-hook" not in out[0].text
    assert not os.path.exists("SHOULD_NOT_EXIST.txt")


def test_agent_name_exported_to_hook(fake_http, tmp_path):
    # Verify $AGENT_NAME reaches the hook env via the real _hook_env + run_hook
    # path. Use a TOML *literal* string (single quotes) so no escaping is needed,
    # and a python script file to avoid shell-quoting fragility on Windows.
    marker = tmp_path / "name.txt"
    script = tmp_path / "h.py"
    script.write_text(
        "import os\n"
        f"open(r'{marker}', 'w').write(os.environ.get('AGENT_NAME', ''))\n"
    )
    toml = f"name = 'brain-coder'\n[hooks]\npre = 'python \"{script}\"'\n"
    out = srv._spawn_with_hooks({"manifest_toml": toml}, toml, None)
    assert len(fake_http) == 1  # pre-hook succeeded → spawn ran
    assert marker.read_text() == "brain-coder"
