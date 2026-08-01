#!/usr/bin/env python3
"""Fail when a versioned OpenFang agent can bypass the OpenAI provider."""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path
from typing import Any


EXPECTED_PROVIDER = "openai"
EXPECTED_MODEL = "gpt-4o-mini"
EXPECTED_API_KEY_ENV = "OPENAI_API_KEY"
MINIMUM_AGENT_COUNT = 47


def _validate_model(
    path: Path,
    label: str,
    model_config: dict[str, Any],
) -> list[str]:
    errors: list[str] = []
    if model_config.get("provider") != EXPECTED_PROVIDER:
        errors.append(
            f"{path.as_posix()}:{label}: provider must be {EXPECTED_PROVIDER!r}"
        )
    if model_config.get("model") != EXPECTED_MODEL:
        errors.append(
            f"{path.as_posix()}:{label}: model must be {EXPECTED_MODEL!r}"
        )
    api_key_env = model_config.get("api_key_env")
    if api_key_env is not None and api_key_env != EXPECTED_API_KEY_ENV:
        errors.append(
            f"{path.as_posix()}:{label}: api_key_env must be "
            f"{EXPECTED_API_KEY_ENV!r}"
        )
    return errors


def validate_agent(path: Path) -> list[str]:
    with path.open("rb") as handle:
        document = tomllib.load(handle)

    errors: list[str] = []
    primary = document.get("model")
    if not isinstance(primary, dict):
        errors.append(f"{path.as_posix()}: missing [model] table")
    else:
        errors.extend(_validate_model(path, "model", primary))

    fallbacks = document.get("fallback_models", [])
    if not isinstance(fallbacks, list):
        errors.append(f"{path.as_posix()}: fallback_models must be an array of tables")
    else:
        for index, fallback in enumerate(fallbacks):
            if not isinstance(fallback, dict):
                errors.append(
                    f"{path.as_posix()}:fallback_models[{index}]: must be a table"
                )
                continue
            errors.extend(
                _validate_model(path, f"fallback_models[{index}]", fallback)
            )
    return errors


def main() -> int:
    repo_root = Path(__file__).resolve().parents[1]
    agent_files = sorted((repo_root / "agents").glob("*/agent.toml"))
    if len(agent_files) < MINIMUM_AGENT_COUNT:
        print(
            f"agent-provider-check: expected at least {MINIMUM_AGENT_COUNT} agent "
            f"manifests, found {len(agent_files)}",
            file=sys.stderr,
        )
        return 1

    errors = [error for path in agent_files for error in validate_agent(path)]
    if errors:
        print("agent-provider-check: FAIL", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1

    print(
        "agent-provider-check: PASS "
        f"({len(agent_files)} manifests, provider={EXPECTED_PROVIDER}, "
        f"model={EXPECTED_MODEL})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
