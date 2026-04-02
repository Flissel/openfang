"""Bridge: sec-canary skill for OpenFang."""
import json
import sys
from pathlib import Path

_env_path = Path(__file__).resolve().parent.parent.parent.parent / "security" / ".env"
if _env_path.exists():
    try:
        from dotenv import load_dotenv
        load_dotenv(_env_path)
    except ImportError:
        pass

_poc_dir = str(Path(__file__).resolve().parent.parent.parent.parent / "security" / "poc_canary")
sys.path.insert(0, _poc_dir)


def main():
    request = json.loads(sys.stdin.read())
    tool = request.get("tool", "")

    try:
        from canary import deploy_canaries, cleanup_canaries, show_status

        if tool == "deploy_canaries":
            deployed = deploy_canaries()
            output = {"deployed": deployed}
        elif tool == "canary_status":
            # Capture print output
            import io
            buf = io.StringIO()
            old_stdout = sys.stdout
            sys.stdout = buf
            show_status()
            sys.stdout = old_stdout
            output = {"status": buf.getvalue()}
        elif tool == "cleanup_canaries":
            cleanup_canaries()
            output = {"cleaned": True}
        else:
            output = {"error": f"Unknown tool: {tool}"}

        json.dump({"output": output, "is_error": False}, sys.stdout)
    except Exception as e:
        json.dump({"output": {"error": str(e)}, "is_error": True}, sys.stdout)


if __name__ == "__main__":
    main()
