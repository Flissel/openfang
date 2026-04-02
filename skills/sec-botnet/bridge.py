"""Bridge: sec-botnet skill for OpenFang."""
import json
import sys
import asyncio
from pathlib import Path

_poc_dir = str(Path(__file__).resolve().parent.parent.parent.parent / "security" / "poc_botnet_detector")
sys.path.insert(0, _poc_dir)


def main():
    request = json.loads(sys.stdin.read())
    tool = request.get("tool", "")
    inp = request.get("input", {})

    try:
        from detector import DGADetector, BeaconDetector, BotnetDetector

        if tool == "botnet_local_check":
            bd = BotnetDetector()
            result = asyncio.run(bd.check_local())
            output = result
        elif tool == "dga_check":
            dga = DGADetector()
            domains = inp.get("domains", [])
            batch = dga.analyze_batch(domains)
            output = batch
        elif tool == "beacon_check":
            bd = BotnetDetector()
            result = asyncio.run(bd.analyze_beacons(duration=15))
            output = result
        else:
            output = {"error": f"Unknown tool: {tool}"}

        json.dump({"output": output, "is_error": False}, sys.stdout, default=str)
    except Exception as e:
        json.dump({"output": {"error": str(e)}, "is_error": True}, sys.stdout)


if __name__ == "__main__":
    main()
