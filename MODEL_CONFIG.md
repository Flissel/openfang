# OpenFang — LLM Model Configuration

## Rust Runtime (37 Agent TOMLs)

The OpenFang Rust runtime reads agent model configuration from TOML files:

```
agents/{agent-name}/agent.toml
```

Agents with `provider = "default"` and `model = "default"` delegate to OpenFang's central config:

```toml
# ~/.openfang/config.toml
[default_model]
provider = "groq"
model = "llama-3.3-70b-versatile"
```

Changing the default model in `config.toml` applies to all agents using `"default"`.

### Brain Agents (claude-code provider)

These 7 agents use `provider = "claude-code"` and are invoked via Claude Code CLI:

- brain-coder, brain-planner, brain-orchestrator, brain-fallback
- brain-devops, brain-researcher, brain-writer

Their model is controlled by the Claude Code CLI `--model` flag or `CLAUDE_MODEL` env var.

### Fallback Models

Many agent TOMLs define `[[fallback_models]]` with `gemini-2.0-flash` as a hardcoded fallback.
This is managed by OpenFang's Rust runtime — not the Python `llm_config.yml` system.

## Python Agents

Python-based agents (e.g., `langchain-code-reviewer`) use `vibemind_shared` with their own
`llm_config.yml` — see `agents/langchain-code-reviewer/llm_config.yml`.
