"""Bridge: sec-vuln-scanner skill for OpenFang."""
import json
import sys
import asyncio
from pathlib import Path

_poc_dir = str(Path(__file__).resolve().parent.parent.parent.parent / "security" / "poc_vuln_scanner")
sys.path.insert(0, _poc_dir)


def main():
    request = json.loads(sys.stdin.read())
    tool = request.get("tool", "")
    inp = request.get("input", {})

    try:
        from main import inventory_installed_software, check_windows_updates, lookup_cves

        if tool == "scan_vulnerabilities":
            software = asyncio.run(inventory_installed_software())
            updates = asyncio.run(check_windows_updates())
            # Lookup CVEs for top 10 most common software
            output = {
                "installed_software_count": len(software),
                "sample_software": software[:20],
                "windows_updates": updates,
            }
        elif tool == "lookup_cve":
            name = inp.get("software_name", "")
            version = inp.get("version", "")
            cves = asyncio.run(lookup_cves(name, version))
            output = {"software": name, "version": version, "cves": cves}
        else:
            output = {"error": f"Unknown tool: {tool}"}

        json.dump({"output": output, "is_error": False}, sys.stdout)
    except Exception as e:
        json.dump({"output": {"error": str(e)}, "is_error": True}, sys.stdout)


if __name__ == "__main__":
    main()
