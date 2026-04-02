"""Bridge: sec-forensics skill for OpenFang."""
import json
import sys
import asyncio
from pathlib import Path

_poc_dir = str(Path(__file__).resolve().parent.parent.parent.parent / "security" / "poc_forensics")
sys.path.insert(0, _poc_dir)


def main():
    request = json.loads(sys.stdin.read())
    tool = request.get("tool", "")

    try:
        from main import (parse_prefetch, parse_browser_history,
                          parse_powershell_history, parse_usb_history,
                          parse_recent_files)

        if tool == "forensics_timeline":
            output = {
                "prefetch": asyncio.run(parse_prefetch()),
                "browser_history": asyncio.run(parse_browser_history()),
                "powershell_history": asyncio.run(parse_powershell_history()),
                "usb_devices": asyncio.run(parse_usb_history()),
                "recent_files": asyncio.run(parse_recent_files()),
            }
        elif tool == "browser_history":
            output = {"entries": asyncio.run(parse_browser_history())}
        elif tool == "usb_history":
            output = {"devices": asyncio.run(parse_usb_history())}
        elif tool == "powershell_history":
            output = {"commands": asyncio.run(parse_powershell_history())}
        else:
            output = {"error": f"Unknown tool: {tool}"}

        json.dump({"output": output, "is_error": False}, sys.stdout)
    except Exception as e:
        json.dump({"output": {"error": str(e)}, "is_error": True}, sys.stdout)


if __name__ == "__main__":
    main()
