//! OpenAI Codex CLI backend driver (subscription mode).
//!
//! Spawns the `codex exec` CLI as a subprocess. Codex handles its own
//! authentication from `~/.codex/auth.json`. When that file is in
//! ChatGPT-subscription mode (`OPENAI_API_KEY` empty, `tokens.*` present),
//! Codex uses the flat-rate ChatGPT/Codex subscription instead of the metered
//! platform API — the same trick `claude_code.rs` uses for Claude Max.
//!
//! This driver **strips `OPENAI_API_KEY` from the subprocess env** so Codex can
//! never silently fall back to metered API billing: the flat-rate subscription
//! is the whole point. This is deliberately different from the `codex` /
//! `openai-codex` provider in `mod.rs`, which reuses the OpenAI HTTP driver and
//! only works with a real API key.
//!
//! Verified headless 2026-08-19: `cmd /c codex exec -s read-only -C <dir>
//! -o <file> --skip-git-repo-check` with `OPENAI_API_KEY=""` returns the exact
//! completion via the output-last-message file, using subscription auth.
//!
//! Intended for the heavy, bursty code/delivery fleet (Captain delivery,
//! coding agents) — NOT high-volume routing/CRUD, where the subscription's
//! usage cap and the per-call agent-loop overhead make the metered API better.

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmDriver, LlmError};
use async_trait::async_trait;
use openfang_types::message::{ContentBlock, Role, StopReason, TokenUsage};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

/// Provider API-key env vars stripped from the subprocess so Codex uses the
/// subscription OAuth rather than metered API billing.
const STRIPPED_ENV: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GROQ_API_KEY",
    "OPENROUTER_API_KEY",
];

/// LLM driver that delegates to the OpenAI Codex CLI in non-interactive
/// `exec` mode, authenticated via the user's ChatGPT subscription.
pub struct CodexExecDriver {
    cli_path: String,
    #[allow(dead_code)]
    skip_permissions: bool,
    message_timeout_secs: u64,
}

impl CodexExecDriver {
    /// `cli_path` overrides the CLI binary; defaults to `"codex"` on PATH.
    /// `skip_permissions` is accepted for interface symmetry; the driver always
    /// runs a `read-only` sandbox (a completion backend never mutates the FS),
    /// so there are no tool approvals to skip.
    pub fn new(cli_path: Option<String>, skip_permissions: bool) -> Self {
        Self {
            cli_path: cli_path
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "codex".to_string()),
            skip_permissions,
            message_timeout_secs: 300,
        }
    }

    /// Flatten the request into a single prompt (Codex reads it from stdin).
    fn build_prompt(request: &CompletionRequest) -> String {
        let mut parts = Vec::new();
        if let Some(ref sys) = request.system {
            parts.push(format!("[System]\n{sys}"));
        }
        for msg in &request.messages {
            let role_label = match msg.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::System => "System",
            };
            let text = msg.content.text_content();
            if !text.is_empty() {
                parts.push(format!("[{role_label}]\n{text}"));
            }
        }
        parts.join("\n\n")
    }

    /// Map a model id like `codex-cli/gpt-5-codex` or `codex-cli/terra` to the
    /// `-m` flag. A bare `codex-cli` (no model) omits `-m` so Codex uses the
    /// user's configured default.
    fn model_flag(model: &str) -> Option<String> {
        let stripped = model
            .strip_prefix("codex-cli/")
            .or_else(|| model.strip_prefix("codex/"))
            .unwrap_or(model);
        if stripped.is_empty() || stripped == "codex-cli" || stripped == "codex" || stripped == "default" {
            None
        } else {
            Some(stripped.to_string())
        }
    }
}

/// Build the base command, wrapping the npm shim through `cmd.exe /C` on
/// Windows (the `codex` shim is a `.cmd`/`.ps1`, not a Win32 exe).
fn base_command(cli_path: &str) -> tokio::process::Command {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = tokio::process::Command::new("cmd.exe");
        cmd.arg("/C").arg(cli_path);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        tokio::process::Command::new(cli_path)
    }
}

fn home_dir() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
}

#[async_trait]
impl LlmDriver for CodexExecDriver {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let prompt = Self::build_prompt(&request);

        // Unique output-last-message file per call (concurrent-call safe).
        let uniq = format!(
            "openfang-codex-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let out_path = std::env::temp_dir().join(uniq);
        let scratch = std::env::temp_dir();

        let mut cmd = base_command(&self.cli_path);
        cmd.arg("exec")
            .arg("-s")
            .arg("read-only")
            .arg("-C")
            .arg(&scratch)
            .arg("-o")
            .arg(&out_path)
            .arg("--skip-git-repo-check");

        if let Some(model) = Self::model_flag(&request.model) {
            cmd.arg("-m").arg(model);
        }

        // Force subscription auth: strip provider API keys so Codex cannot fall
        // back to metered billing.
        for key in STRIPPED_ENV {
            cmd.env_remove(key);
        }
        // Ensure the CLI finds its credentials when OpenFang runs as a service.
        if let Some(home) = home_dir() {
            cmd.env("USERPROFILE", &home);
            cmd.env("HOME", &home);
        }

        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        debug!(cli = %self.cli_path, prompt_bytes = prompt.len(), "Spawning Codex exec (subscription)");

        let mut child = cmd.spawn().map_err(|e| {
            LlmError::Http(format!(
                "Codex CLI not found or failed to start ({e}). \
                 Install: npm install -g @openai/codex && codex login"
            ))
        })?;

        // Codex reads the prompt from stdin when no prompt arg is given.
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
                warn!(error = %e, "Failed to write prompt to Codex stdin");
            }
            drop(stdin);
        }

        let timeout = std::time::Duration::from_secs(self.message_timeout_secs);
        let wait = tokio::time::timeout(timeout, child.wait_with_output()).await;

        let output = match wait {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                let _ = std::fs::remove_file(&out_path);
                return Err(LlmError::Http(format!("Codex subprocess failed: {e}")));
            }
            Err(_) => {
                let _ = std::fs::remove_file(&out_path);
                return Err(LlmError::Http(format!(
                    "Codex exec timed out after {}s",
                    self.message_timeout_secs
                )));
            }
        };

        // The completion is the output-last-message file. Read then remove it.
        let text = std::fs::read_to_string(&out_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let _ = std::fs::remove_file(&out_path);

        if text.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = if !stderr.trim().is_empty() {
                stderr.trim()
            } else {
                stdout.trim()
            };
            let code = output.status.code().unwrap_or(1);
            let message = if detail.contains("login")
                || detail.contains("auth")
                || detail.contains("credentials")
                || detail.contains("sign in")
            {
                format!("Codex CLI is not authenticated. Run: codex login\nDetail: {detail}")
            } else {
                format!("Codex exec produced no output (exit {code}): {detail}")
            };
            return Err(LlmError::Api {
                status: code as u16,
                message,
            });
        }

        // Subscription usage is flat-rate — no per-token receipt is authoritative,
        // so usage is reported as zero (honest for a non-metered provider).
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text,
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: Vec::new(),
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
        })
    }

    // `stream` uses the trait default (runs `complete`, emits one TextDelta).
    // Codex exec is a batch agent loop, so token-streaming is not applicable.
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::message::{Message, MessageContent, Role};

    #[test]
    fn model_flag_strips_prefix_and_defaults() {
        assert_eq!(CodexExecDriver::model_flag("codex-cli"), None);
        assert_eq!(CodexExecDriver::model_flag("codex-cli/"), None);
        assert_eq!(
            CodexExecDriver::model_flag("codex-cli/gpt-5-codex"),
            Some("gpt-5-codex".to_string())
        );
        assert_eq!(
            CodexExecDriver::model_flag("codex/terra"),
            Some("terra".to_string())
        );
    }

    #[test]
    fn build_prompt_includes_system_and_messages() {
        let req = CompletionRequest {
            model: "codex-cli".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                ..Default::default()
            }],
            tools: vec![],
            max_tokens: 64,
            temperature: 0.0,
            system: Some("be terse".into()),
            thinking: None,
        };
        let p = CodexExecDriver::build_prompt(&req);
        assert!(p.contains("[System]"));
        assert!(p.contains("be terse"));
        assert!(p.contains("hello"));
    }

    /// Live driver proof against the real Codex CLI + ChatGPT subscription.
    /// Ignored by default (needs `codex` installed and `codex login`).
    /// Run: `cargo test -p openfang-runtime --lib codex_exec::tests::live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_completion_uses_subscription() {
        let driver = CodexExecDriver::new(None, false);
        let req = CompletionRequest {
            model: "codex-cli".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("Reply with exactly: CODEX-DRIVER-OK".into()),
                ..Default::default()
            }],
            tools: vec![],
            max_tokens: 32,
            temperature: 0.0,
            system: None,
            thinking: None,
        };
        let resp = driver.complete(req).await.expect("codex driver completion");
        let text = match resp.content.first() {
            Some(ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        };
        println!("codex driver returned: {text:?}");
        assert!(
            text.contains("CODEX-DRIVER-OK"),
            "expected marker in response, got: {text:?}"
        );
    }
}
