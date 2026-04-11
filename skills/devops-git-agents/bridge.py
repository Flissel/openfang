"""Bridge: devops-git-agents skill for OpenFang."""
import json
import sys
import os
from pathlib import Path

# Load .env for API keys
_env_path = Path(__file__).resolve().parent.parent.parent.parent / "devops" / ".env"
if not _env_path.exists():
    _env_path = Path(__file__).resolve().parent.parent.parent.parent / "security" / ".env"
if _env_path.exists():
    try:
        from dotenv import load_dotenv
        load_dotenv(_env_path)
    except ImportError:
        pass

# Add PoC to path
_poc_dir = str(Path(__file__).resolve().parent.parent.parent.parent / "devops" / "poc_git_agents")
_devops_dir = str(Path(__file__).resolve().parent.parent.parent.parent / "devops")
sys.path.insert(0, _poc_dir)
sys.path.insert(0, _devops_dir)


def _run_agent(agent_name: str, repo: str, dry_run: bool) -> dict:
    """Run a single git agent or all agents."""
    from llm_client import get_client_sync, get_model
    from gh_client import GhClient
    from orchestrator import Orchestrator

    import yaml
    config_path = os.path.join(_poc_dir, "config.yml")
    with open(config_path, encoding="utf-8") as f:
        config = yaml.safe_load(f)

    safety_cfg = config.get("safety", {})
    scan_cfg = config.get("scan", {})

    gh = GhClient(
        owner="Flissel",
        skip_repos=scan_cfg.get("skip_repos", []),
        timeout=30,
    )

    llm_client = get_client_sync("default")
    llm_model = get_model("default")

    orch = Orchestrator(
        gh=gh,
        llm_client=llm_client,
        llm_model=llm_model,
        dry_run=dry_run,
        auto_approve=safety_cfg.get("auto_approve", False),
        delete_whitelist=safety_cfg.get("delete_whitelist", []),
    )

    # Suppress prints to stderr
    old_stdout = sys.stdout
    sys.stdout = sys.stderr

    try:
        if agent_name == "all":
            results = orch.run_all(repo_filter=repo)
        else:
            results = orch.run_agent(agent_name, repo_filter=repo)

        report = orch.format_report(results)
    finally:
        sys.stdout = old_stdout

    return {"results": results, "report": report}


def main():
    request = json.loads(sys.stdin.read())
    tool = request.get("tool", "")
    inp = request.get("input", {})
    repo = inp.get("repo", "")
    dry_run = inp.get("dry_run", True)

    try:
        agent_map = {
            "git_triage": "triage",
            "git_review": "review",
            "git_cicd": "cicd",
            "git_board": "board",
            "git_all": "all",
        }

        if tool in agent_map:
            output = _run_agent(agent_map[tool], repo, dry_run)
        else:
            output = {"error": f"Unknown tool: {tool}"}

        json.dump({"output": output, "is_error": False}, sys.stdout, default=str)
    except Exception as e:
        json.dump({"output": {"error": str(e)}, "is_error": True}, sys.stdout)


if __name__ == "__main__":
    main()
