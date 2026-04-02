"""Bridge: sec-os-shield skill for OpenFang."""
import json
import sys
import os
import asyncio

# Load .env before any imports (env_clear strips env vars)
from pathlib import Path
_env_path = Path(__file__).resolve().parent.parent.parent.parent / "security" / ".env"
if _env_path.exists():
    try:
        from dotenv import load_dotenv
        load_dotenv(_env_path)
    except ImportError:
        pass

# Add PoC directory to path
_poc_dir = str(Path(__file__).resolve().parent.parent.parent.parent / "security" / "poc_os_shield")
_sec_dir = str(Path(__file__).resolve().parent.parent.parent.parent / "security")
sys.path.insert(0, _poc_dir)
sys.path.insert(0, _sec_dir)


def _suppress_stdout(fn, *args, **kwargs):
    """Run function with stdout suppressed (redirected to stderr)."""
    import io
    old = sys.stdout
    sys.stdout = sys.stderr  # redirect prints to stderr
    try:
        return fn(*args, **kwargs)
    finally:
        sys.stdout = old


def handle_os_shield_scan():
    from baselines import capture_baseline
    from tools import TOOL_DISPATCH
    import config

    # Capture baseline (suppress print output)
    baseline = _suppress_stdout(asyncio.run, capture_baseline())

    # Run key detection tools
    findings = []
    key_tools = [
        "list_processes", "check_registry_autoruns",
        "list_network_connections", "detect_suspicious_connections",
        "list_usb_devices", "detect_suspicious_paths",
        "detect_encoded_commands", "detect_beaconing",
    ]
    for tool_name in key_tools:
        if tool_name in TOOL_DISPATCH:
            try:
                result = asyncio.run(TOOL_DISPATCH[tool_name]())
                findings.append({"tool": tool_name, "result": result})
            except Exception as e:
                findings.append({"tool": tool_name, "error": str(e)})

    return {
        "baseline": {k: len(v) if isinstance(v, (list, dict)) else v
                     for k, v in baseline.items()},
        "findings": findings,
        "tools_available": list(TOOL_DISPATCH.keys()),
    }


def handle_os_shield_baseline():
    from baselines import capture_baseline
    baseline = _suppress_stdout(asyncio.run, capture_baseline())
    return {k: (len(v) if isinstance(v, (list, dict)) else v)
            for k, v in baseline.items()}


def main():
    request = json.loads(sys.stdin.read())
    tool = request.get("tool", "")
    try:
        if tool == "os_shield_scan":
            result = handle_os_shield_scan()
        elif tool == "os_shield_baseline":
            result = handle_os_shield_baseline()
        else:
            result = {"error": f"Unknown tool: {tool}"}
        json.dump({"output": result, "is_error": False}, sys.stdout)
    except Exception as e:
        json.dump({"output": {"error": str(e)}, "is_error": True}, sys.stdout)


if __name__ == "__main__":
    main()
