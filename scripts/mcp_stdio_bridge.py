"""
stdio -> HTTP MCP bridge.

A consumer (e.g. acpx) speaks the MCP JSON-RPC protocol over stdio. This bridge
forwards each request to OpenFang's `POST /mcp` endpoint and writes the
response back to stdout.

Why: acpx (OpenClaw's ACP runtime backend) only supports stdio MCP servers.
OpenFang aggregates ~30 MCP servers behind a single HTTP endpoint. This bridge
gives every acpx-backed agent access to the full aggregated tool surface via
exactly one mcpServers entry, with no per-agent config drift and a single warm
SentenceTransformer/Torch process inside OpenFang.

Usage (in acpx mcpServers):
    {
      "openfang": {
        "command": "python",
        "args": ["<abs path>/vibemind-os/openfang/scripts/mcp_stdio_bridge.py"],
        "env": { "OPENFANG_MCP_URL": "http://127.0.0.1:4200/mcp" }
      }
    }
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

DEFAULT_URL = "http://127.0.0.1:4200/mcp"
TIMEOUT_SECONDS = float(os.environ.get("OPENFANG_MCP_TIMEOUT", "120"))


def forward(request_bytes: bytes, url: str) -> bytes:
    req = urllib.request.Request(
        url,
        data=request_bytes,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=TIMEOUT_SECONDS) as resp:
        return resp.read()


def make_error(request_id, code: int, message: str) -> bytes:
    payload = {
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": code, "message": message},
    }
    return json.dumps(payload).encode("utf-8")


def main() -> int:
    url = os.environ.get("OPENFANG_MCP_URL", DEFAULT_URL)

    stdin = sys.stdin.buffer
    stdout = sys.stdout.buffer

    for raw_line in stdin:
        line = raw_line.strip()
        if not line:
            continue

        request_id = None
        try:
            parsed = json.loads(line)
            request_id = parsed.get("id")
        except json.JSONDecodeError:
            stdout.write(make_error(None, -32700, "Parse error") + b"\n")
            stdout.flush()
            continue

        try:
            response = forward(line, url)
        except urllib.error.URLError as exc:
            response = make_error(
                request_id,
                -32000,
                f"OpenFang MCP unreachable at {url}: {exc}",
            )
        except Exception as exc:
            response = make_error(request_id, -32000, f"Bridge error: {exc}")

        if not response.endswith(b"\n"):
            response += b"\n"
        stdout.write(response)
        stdout.flush()

    return 0


if __name__ == "__main__":
    sys.exit(main())
