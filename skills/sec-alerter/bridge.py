"""Bridge: sec-alerter skill for OpenFang."""
import json
import sys
import os
import asyncio
from pathlib import Path

_env_path = Path(__file__).resolve().parent.parent.parent.parent / "security" / ".env"
if _env_path.exists():
    try:
        from dotenv import load_dotenv
        load_dotenv(_env_path)
    except ImportError:
        pass

_poc_dir = str(Path(__file__).resolve().parent.parent.parent.parent / "security" / "poc_alerter")
sys.path.insert(0, _poc_dir)


def main():
    request = json.loads(sys.stdin.read())
    tool = request.get("tool", "")
    inp = request.get("input", {})

    try:
        from alerter import send_alert, send_alert_batch

        if tool == "send_alert":
            result = asyncio.run(send_alert(
                severity=inp.get("severity", "INFO"),
                title=inp.get("title", "Alert"),
                details=inp.get("details", "")
            ))
            output = {"sent": result}
        elif tool == "send_alert_batch":
            results = asyncio.run(send_alert_batch(
                findings=inp.get("findings", []),
                source=inp.get("source", "Security Auditor")
            ))
            output = {"results": results}
        else:
            output = {"error": f"Unknown tool: {tool}"}

        json.dump({"output": output, "is_error": False}, sys.stdout)
    except Exception as e:
        json.dump({"output": {"error": str(e)}, "is_error": True}, sys.stdout)


if __name__ == "__main__":
    main()
