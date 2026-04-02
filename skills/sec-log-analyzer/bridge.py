"""Bridge: sec-log-analyzer skill for OpenFang."""
import json
import sys
import asyncio
from pathlib import Path

_env_path = Path(__file__).resolve().parent.parent.parent.parent / "security" / ".env"
if _env_path.exists():
    try:
        from dotenv import load_dotenv
        load_dotenv(_env_path)
    except ImportError:
        pass

_poc_dir = str(Path(__file__).resolve().parent.parent.parent.parent / "security" / "poc_log_analyzer")
sys.path.insert(0, _poc_dir)


def main():
    request = json.loads(sys.stdin.read())
    tool = request.get("tool", "")
    inp = request.get("input", {})
    hours = inp.get("hours", 24)

    try:
        from tools import detect_brute_force, build_timeline

        if tool == "analyze_logs":
            bf = asyncio.run(detect_brute_force(hours=hours))
            tl = asyncio.run(build_timeline(hours=hours))
            output = {"brute_force": bf, "timeline": tl}
        elif tool == "detect_brute_force":
            output = asyncio.run(detect_brute_force(hours=hours))
        elif tool == "build_timeline":
            output = asyncio.run(build_timeline(hours=hours))
        else:
            output = {"error": f"Unknown tool: {tool}"}

        json.dump({"output": output, "is_error": False}, sys.stdout, default=str)
    except Exception as e:
        json.dump({"output": {"error": str(e)}, "is_error": True}, sys.stdout)


if __name__ == "__main__":
    main()
