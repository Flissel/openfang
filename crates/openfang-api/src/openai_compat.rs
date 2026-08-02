//! OpenAI-compatible `/v1/chat/completions` API endpoint.
//!
//! Allows any OpenAI-compatible client library to talk to OpenFang agents.
//! The `model` field resolves to an agent (by name, UUID, or `openfang:<name>`),
//! and the messages are forwarded to the agent's LLM loop.
//!
//! Supports both streaming (SSE) and non-streaming responses.

use crate::routes::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use openfang_runtime::kernel_handle::KernelHandle;
use openfang_runtime::llm_driver::StreamEvent;
use openfang_types::agent::AgentId;
use openfang_types::message::{ContentBlock, Message, MessageContent, Role, StopReason};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::warn;

// ── Request types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<OaiMessage>,
    #[serde(default)]
    pub stream: bool,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub struct OaiMessage {
    pub role: String,
    #[serde(default)]
    pub content: OaiContent,
}

#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
pub enum OaiContent {
    Text(String),
    Parts(Vec<OaiContentPart>),
    #[default]
    Null,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum OaiContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: OaiImageUrlRef },
}

#[derive(Debug, Deserialize)]
pub struct OaiImageUrlRef {
    pub url: String,
}

/// Bounded OpenAI-compatible request body for `/v1/embeddings`.
///
/// The endpoint accepts either one text or a non-empty batch of texts. Other
/// OpenAI request fields are deliberately not accepted until OpenFang can
/// honour them through its embedding driver abstraction.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingsRequest {
    pub model: String,
    pub input: EmbeddingsInput,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingsInput {
    Text(String),
    Batch(Vec<String>),
}

impl EmbeddingsInput {
    fn texts(&self) -> Vec<&str> {
        match self {
            Self::Text(text) => vec![text],
            Self::Batch(texts) => texts.iter().map(String::as_str).collect(),
        }
    }
}

impl EmbeddingsRequest {
    fn validate(&self) -> Result<(), &'static str> {
        if self.model.trim().is_empty() {
            return Err("'model' must not be empty");
        }
        let texts = self.input.texts();
        if texts.is_empty() {
            return Err("'input' must contain at least one text");
        }
        if texts.iter().any(|text| text.trim().is_empty()) {
            return Err("'input' must not contain empty text");
        }
        Ok(())
    }
}

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatCompletionResponse {
    id: String,
    object: &'static str,
    created: u64,
    model: String,
    choices: Vec<Choice>,
    usage: UsageInfo,
}

#[derive(Serialize)]
struct Choice {
    index: u32,
    message: ChoiceMessage,
    finish_reason: &'static str,
}

#[derive(Serialize)]
struct ChoiceMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCall>>,
}

#[derive(Serialize)]
struct UsageInfo {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

#[derive(Serialize)]
struct ChatCompletionChunk {
    id: String,
    object: &'static str,
    created: u64,
    model: String,
    choices: Vec<ChunkChoice>,
}

#[derive(Serialize)]
struct ChunkChoice {
    index: u32,
    delta: ChunkDelta,
    finish_reason: Option<&'static str>,
}

#[derive(Serialize)]
struct ChunkDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCall>>,
}

#[derive(Serialize, Clone)]
struct OaiToolCall {
    index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    call_type: Option<&'static str>,
    function: OaiToolCallFunction,
}

#[derive(Serialize, Clone)]
struct OaiToolCallFunction {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
}

#[derive(Serialize)]
struct ModelObject {
    id: String,
    object: &'static str,
    created: u64,
    owned_by: String,
}

#[derive(Serialize)]
struct ModelListResponse {
    object: &'static str,
    data: Vec<ModelObject>,
}

#[derive(Serialize)]
struct EmbeddingsResponse {
    object: &'static str,
    data: Vec<EmbeddingObject>,
    model: String,
    usage: EmbeddingUsage,
}

#[derive(Serialize)]
struct EmbeddingObject {
    object: &'static str,
    embedding: Vec<f32>,
    index: usize,
}

#[derive(Serialize)]
struct EmbeddingUsage {
    prompt_tokens: u64,
    total_tokens: u64,
}

impl EmbeddingsResponse {
    fn from_vectors(model: String, vectors: Vec<Vec<f32>>) -> Self {
        Self {
            object: "list",
            data: vectors
                .into_iter()
                .enumerate()
                .map(|(index, embedding)| EmbeddingObject {
                    object: "embedding",
                    embedding,
                    index,
                })
                .collect(),
            model,
            // The shared EmbeddingDriver does not expose token accounting.
            usage: EmbeddingUsage {
                prompt_tokens: 0,
                total_tokens: 0,
            },
        }
    }
}

// ── Agent resolution ────────────────────────────────────────────────────────

fn resolve_agent(state: &AppState, model: &str) -> Option<(AgentId, String)> {
    // 1. "openfang:<name>" → find agent by name
    if let Some(name) = model.strip_prefix("openfang:") {
        if let Some(entry) = state.kernel.registry.find_by_name(name) {
            return Some((entry.id, entry.name.clone()));
        }
    }

    // 2. Valid UUID → find agent by ID
    if let Ok(id) = model.parse::<AgentId>() {
        if let Some(entry) = state.kernel.registry.get(id) {
            return Some((entry.id, entry.name.clone()));
        }
    }

    // 3. Plain string → try as agent name
    if let Some(entry) = state.kernel.registry.find_by_name(model) {
        return Some((entry.id, entry.name.clone()));
    }

    // No match — return None so the caller returns a proper 404
    None
}

// ── Message conversion ──────────────────────────────────────────────────────

fn convert_messages(oai_messages: &[OaiMessage]) -> Vec<Message> {
    oai_messages
        .iter()
        .filter_map(|m| {
            let role = match m.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "system" => Role::System,
                _ => Role::User,
            };

            let content = match &m.content {
                OaiContent::Text(text) => MessageContent::Text(text.clone()),
                OaiContent::Parts(parts) => {
                    let blocks: Vec<ContentBlock> = parts
                        .iter()
                        .filter_map(|part| match part {
                            OaiContentPart::Text { text } => Some(ContentBlock::Text {
                                text: text.clone(),
                                provider_metadata: None,
                            }),
                            OaiContentPart::ImageUrl { image_url } => {
                                // Parse data URI: data:{media_type};base64,{data}
                                if let Some(rest) = image_url.url.strip_prefix("data:") {
                                    let parts: Vec<&str> = rest.splitn(2, ',').collect();
                                    if parts.len() == 2 {
                                        let media_type = parts[0]
                                            .strip_suffix(";base64")
                                            .unwrap_or(parts[0])
                                            .to_string();
                                        let data = parts[1].to_string();
                                        Some(ContentBlock::Image { media_type, data })
                                    } else {
                                        None
                                    }
                                } else {
                                    // URL-based images not supported (would require fetching)
                                    None
                                }
                            }
                        })
                        .collect();
                    if blocks.is_empty() {
                        return None;
                    }
                    MessageContent::Blocks(blocks)
                }
                OaiContent::Null => return None,
            };

            Some(Message {
                msg_id: uuid::Uuid::new_v4().to_string(),
                provider_msg_id: None,
                role,
                content,
            })
        })
        .collect()
}

// ── Handlers ────────────────────────────────────────────────────────────────

/// POST /v1/chat/completions
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatCompletionRequest>,
) -> impl IntoResponse {
    let (agent_id, agent_name) = match resolve_agent(&state, &req.model) {
        Some(pair) => pair,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": {
                        "message": format!("No agent found for model '{}'", req.model),
                        "type": "invalid_request_error",
                        "code": "model_not_found"
                    }
                })),
            )
                .into_response();
        }
    };

    // Extract the last user message as the input
    let messages = convert_messages(&req.messages);
    let last_user_msg = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.text_content())
        .unwrap_or_default();

    if last_user_msg.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": {
                    "message": "No user message found in request",
                    "type": "invalid_request_error",
                    "code": "missing_message"
                }
            })),
        )
            .into_response();
    }

    let request_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if req.stream {
        // Streaming response
        return match stream_response(
            state,
            agent_id,
            agent_name,
            &last_user_msg,
            request_id,
            created,
        )
        .await
        {
            Ok(sse) => sse.into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": {
                        "message": format!("{e}"),
                        "type": "server_error"
                    }
                })),
            )
                .into_response(),
        };
    }

    // Non-streaming response
    let kernel_handle: Arc<dyn KernelHandle> = state.kernel.clone() as Arc<dyn KernelHandle>;
    match state
        .kernel
        .send_message_with_handle(agent_id, &last_user_msg, Some(kernel_handle), None, None)
        .await
    {
        Ok(result) => {
            let response = ChatCompletionResponse {
                id: request_id,
                object: "chat.completion",
                created,
                model: agent_name,
                choices: vec![Choice {
                    index: 0,
                    message: ChoiceMessage {
                        role: "assistant",
                        content: Some(crate::ws::strip_think_tags(&result.response)),
                        tool_calls: None,
                    },
                    finish_reason: "stop",
                }],
                usage: UsageInfo {
                    prompt_tokens: result.total_usage.input_tokens,
                    completion_tokens: result.total_usage.output_tokens,
                    total_tokens: result.total_usage.input_tokens
                        + result.total_usage.output_tokens,
                },
            };
            Json(serde_json::to_value(&response).unwrap_or_default()).into_response()
        }
        Err(e) => {
            warn!("OpenAI compat: agent error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": {
                        "message": "Agent processing failed",
                        "type": "server_error"
                    }
                })),
            )
                .into_response()
        }
    }
}

/// POST /v1/embeddings
///
/// Computes embeddings through the already-booted OpenFang OpenAI embedding
/// driver. The endpoint refuses any implicit provider selection so a request
/// can never fall through to a local model or a different provider.
pub async fn embeddings(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EmbeddingsRequest>,
) -> axum::response::Response {
    if let Err(message) = req.validate() {
        return embeddings_error(
            StatusCode::BAD_REQUEST,
            message,
            "invalid_request_error",
            "invalid_embedding_input",
        );
    }

    let memory_config = &state.kernel.config.memory;
    if memory_config.embedding_provider.as_deref() != Some("openai") {
        return embeddings_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Embeddings require an explicit OpenAI memory provider configuration",
            "service_unavailable_error",
            "embedding_provider_not_configured",
        );
    }

    let configured_model = memory_config.embedding_model.trim();
    if !is_openai_embedding_model(configured_model) || req.model != configured_model {
        return embeddings_error(
            StatusCode::BAD_REQUEST,
            "Requested model is not the configured OpenFang OpenAI embedding model",
            "invalid_request_error",
            "embedding_model_not_configured",
        );
    }

    let api_key_env = memory_config
        .embedding_api_key_env
        .as_deref()
        .unwrap_or("OPENAI_API_KEY");
    let has_api_key = std::env::var(api_key_env)
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    if !has_api_key {
        return embeddings_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Configured OpenAI embedding credentials are unavailable",
            "service_unavailable_error",
            "embedding_credentials_unavailable",
        );
    }

    let Some(driver) = state.kernel.embedding_driver.as_ref() else {
        return embeddings_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "OpenAI embedding driver is unavailable",
            "service_unavailable_error",
            "embedding_driver_unavailable",
        );
    };

    let texts = req.input.texts();
    match driver.embed(&texts).await {
        Ok(vectors) => Json(EmbeddingsResponse::from_vectors(req.model, vectors)).into_response(),
        Err(error) => {
            warn!(error = %error, "OpenAI embeddings request failed");
            embeddings_error(
                StatusCode::BAD_GATEWAY,
                "OpenAI embeddings request failed",
                "server_error",
                "embedding_provider_error",
            )
        }
    }
}

fn is_openai_embedding_model(model: &str) -> bool {
    matches!(
        model,
        "text-embedding-3-small" | "text-embedding-3-large" | "text-embedding-ada-002"
    )
}

fn embeddings_error(
    status: StatusCode,
    message: &str,
    error_type: &str,
    code: &str,
) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "message": message,
                "type": error_type,
                "code": code,
            }
        })),
    )
        .into_response()
}

/// Build an SSE stream response for streaming completions.
async fn stream_response(
    state: Arc<AppState>,
    agent_id: AgentId,
    agent_name: String,
    message: &str,
    request_id: String,
    created: u64,
) -> Result<axum::response::Response, String> {
    let kernel_handle: Arc<dyn KernelHandle> = state.kernel.clone() as Arc<dyn KernelHandle>;

    let (mut rx, _handle) = state
        .kernel
        .send_message_streaming(agent_id, message, Some(kernel_handle), None, None, None)
        .map_err(|e| format!("Streaming setup failed: {e}"))?;

    let (tx, stream_rx) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(64);

    // Send initial role delta
    let first_chunk = ChatCompletionChunk {
        id: request_id.clone(),
        object: "chat.completion.chunk",
        created,
        model: agent_name.clone(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: Some("assistant"),
                content: None,
                tool_calls: None,
            },
            finish_reason: None,
        }],
    };
    let _ = tx
        .send(Ok(SseEvent::default().data(
            serde_json::to_string(&first_chunk).unwrap_or_default(),
        )))
        .await;

    // Helper to build a chunk with a delta and optional finish_reason.
    fn make_chunk(
        id: &str,
        created: u64,
        model: &str,
        delta: ChunkDelta,
        finish_reason: Option<&'static str>,
    ) -> String {
        let chunk = ChatCompletionChunk {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason,
            }],
        };
        serde_json::to_string(&chunk).unwrap_or_default()
    }

    // Spawn forwarder task — streams ALL iterations until the agent loop channel closes.
    let req_id = request_id.clone();
    tokio::spawn(async move {
        // Tracks current tool_call index within each LLM iteration.
        let mut tool_index: u32 = 0;

        while let Some(event) = rx.recv().await {
            let json = match event {
                StreamEvent::TextDelta { text } => make_chunk(
                    &req_id,
                    created,
                    &agent_name,
                    ChunkDelta {
                        role: None,
                        content: Some(text),
                        tool_calls: None,
                    },
                    None,
                ),
                StreamEvent::ToolUseStart { id, name } => {
                    let idx = tool_index;
                    tool_index += 1;
                    make_chunk(
                        &req_id,
                        created,
                        &agent_name,
                        ChunkDelta {
                            role: None,
                            content: None,
                            tool_calls: Some(vec![OaiToolCall {
                                index: idx,
                                id: Some(id),
                                call_type: Some("function"),
                                function: OaiToolCallFunction {
                                    name: Some(name),
                                    arguments: Some(String::new()),
                                },
                            }]),
                        },
                        None,
                    )
                }
                StreamEvent::ToolInputDelta { text } => {
                    // tool_index already incremented past current tool, so current = index - 1
                    let idx = tool_index.saturating_sub(1);
                    make_chunk(
                        &req_id,
                        created,
                        &agent_name,
                        ChunkDelta {
                            role: None,
                            content: None,
                            tool_calls: Some(vec![OaiToolCall {
                                index: idx,
                                id: None,
                                call_type: None,
                                function: OaiToolCallFunction {
                                    name: None,
                                    arguments: Some(text),
                                },
                            }]),
                        },
                        None,
                    )
                }
                StreamEvent::ContentComplete { stop_reason, .. } => {
                    // ToolUse → reset tool index for next iteration, do NOT finish.
                    // EndTurn/MaxTokens/StopSequence → continue, wait for channel close.
                    if matches!(stop_reason, StopReason::ToolUse) {
                        tool_index = 0;
                    }
                    continue;
                }
                // ToolUseEnd, ToolExecutionResult, ThinkingDelta, PhaseChange — skip
                _ => continue,
            };
            if tx.send(Ok(SseEvent::default().data(json))).await.is_err() {
                break;
            }
        }

        // Channel closed — agent loop is fully done. Send finish + [DONE].
        let final_json = make_chunk(
            &req_id,
            created,
            &agent_name,
            ChunkDelta {
                role: None,
                content: None,
                tool_calls: None,
            },
            Some("stop"),
        );
        let _ = tx.send(Ok(SseEvent::default().data(final_json))).await;
        let _ = tx.send(Ok(SseEvent::default().data("[DONE]"))).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// GET /v1/models — List available agents as OpenAI model objects.
pub async fn list_models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let agents = state.kernel.registry.list();
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let models: Vec<ModelObject> = agents
        .iter()
        .map(|e| ModelObject {
            id: format!("openfang:{}", e.name),
            object: "model",
            created,
            owned_by: "openfang".to_string(),
        })
        .collect();

    Json(
        serde_json::to_value(&ModelListResponse {
            object: "list",
            data: models,
        })
        .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeddings_request_accepts_text_or_non_empty_batch_only() {
        let one: EmbeddingsRequest =
            serde_json::from_str(r#"{"model":"text-embedding-3-small","input":"hello"}"#)
                .expect("a single text input should be accepted");
        assert_eq!(one.input.texts(), vec!["hello"]);

        let batch: EmbeddingsRequest =
            serde_json::from_str(r#"{"model":"text-embedding-3-small","input":["hello","world"]}"#)
                .expect("a text batch should be accepted");
        assert_eq!(batch.input.texts(), vec!["hello", "world"]);

        let empty: EmbeddingsRequest =
            serde_json::from_str(r#"{"model":"text-embedding-3-small","input":[]}"#)
                .expect("the syntactic request is valid");
        assert!(empty.validate().is_err());
    }

    #[test]
    fn embeddings_request_rejects_empty_or_whitespace_texts() {
        for input in ["\"\"", "\"   \""] {
            let request: EmbeddingsRequest = serde_json::from_str(&format!(
                r#"{{"model":"text-embedding-3-small","input":{input}}}"#
            ))
            .expect("the syntactic request is valid");
            assert!(
                request.validate().is_err(),
                "input {input:?} must fail closed"
            );
        }

        for input in [r#"["valid",""]"#, r#"["valid","   "]"#] {
            let request: EmbeddingsRequest = serde_json::from_str(&format!(
                r#"{{"model":"text-embedding-3-small","input":{input}}}"#
            ))
            .expect("the syntactic request is valid");
            assert!(
                request.validate().is_err(),
                "input {input:?} must fail closed"
            );
        }
    }

    #[test]
    fn embeddings_response_has_openai_shape_and_preserves_batch_order() {
        let response = EmbeddingsResponse::from_vectors(
            "text-embedding-3-small".to_string(),
            vec![vec![0.25, -0.5], vec![1.0, 0.0]],
        );

        let json = serde_json::to_value(response).expect("response should serialize");
        assert_eq!(json["object"], "list");
        assert_eq!(json["model"], "text-embedding-3-small");
        assert_eq!(json["data"][0]["object"], "embedding");
        assert_eq!(json["data"][0]["index"], 0);
        assert_eq!(
            json["data"][0]["embedding"],
            serde_json::json!([0.25, -0.5])
        );
        assert_eq!(json["data"][1]["index"], 1);
        assert_eq!(json["usage"]["prompt_tokens"], 0);
        assert_eq!(json["usage"]["total_tokens"], 0);
    }

    #[test]
    fn test_oai_content_deserialize_string() {
        let json = r#"{"role":"user","content":"hello"}"#;
        let msg: OaiMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg.content, OaiContent::Text(ref t) if t == "hello"));
    }

    #[test]
    fn test_oai_content_deserialize_parts() {
        let json = r#"{"role":"user","content":[{"type":"text","text":"what is this?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,abc123"}}]}"#;
        let msg: OaiMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg.content, OaiContent::Parts(ref p) if p.len() == 2));
    }

    #[test]
    fn test_convert_messages_text() {
        let oai = vec![
            OaiMessage {
                role: "system".to_string(),
                content: OaiContent::Text("You are helpful.".to_string()),
            },
            OaiMessage {
                role: "user".to_string(),
                content: OaiContent::Text("Hello!".to_string()),
            },
        ];
        let msgs = convert_messages(&oai);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[1].role, Role::User);
    }

    #[test]
    fn test_convert_messages_with_image() {
        let oai = vec![OaiMessage {
            role: "user".to_string(),
            content: OaiContent::Parts(vec![
                OaiContentPart::Text {
                    text: "What is this?".to_string(),
                },
                OaiContentPart::ImageUrl {
                    image_url: OaiImageUrlRef {
                        url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
                    },
                },
            ]),
        }];
        let msgs = convert_messages(&oai);
        assert_eq!(msgs.len(), 1);
        match &msgs[0].content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                assert!(matches!(&blocks[0], ContentBlock::Text { .. }));
                assert!(matches!(&blocks[1], ContentBlock::Image { .. }));
            }
            _ => panic!("Expected Blocks"),
        }
    }

    #[test]
    fn test_response_serialization() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion",
            created: 1234567890,
            model: "test-agent".to_string(),
            choices: vec![Choice {
                index: 0,
                message: ChoiceMessage {
                    role: "assistant",
                    content: Some("Hello!".to_string()),
                    tool_calls: None,
                },
                finish_reason: "stop",
            }],
            usage: UsageInfo {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["choices"][0]["message"]["content"], "Hello!");
        assert_eq!(json["usage"]["total_tokens"], 15);
        // tool_calls should be omitted when None
        assert!(json["choices"][0]["message"].get("tool_calls").is_none());
    }

    #[test]
    fn test_chunk_serialization() {
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion.chunk",
            created: 1234567890,
            model: "test-agent".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: Some("Hello".to_string()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
        };
        let json = serde_json::to_value(&chunk).unwrap();
        assert_eq!(json["object"], "chat.completion.chunk");
        assert_eq!(json["choices"][0]["delta"]["content"], "Hello");
        assert!(json["choices"][0]["delta"]["role"].is_null());
        // tool_calls should be omitted when None
        assert!(json["choices"][0]["delta"].get("tool_calls").is_none());
    }

    #[test]
    fn test_tool_call_serialization() {
        let tc = OaiToolCall {
            index: 0,
            id: Some("call_abc123".to_string()),
            call_type: Some("function"),
            function: OaiToolCallFunction {
                name: Some("get_weather".to_string()),
                arguments: Some(r#"{"location":"NYC"}"#.to_string()),
            },
        };
        let json = serde_json::to_value(&tc).unwrap();
        assert_eq!(json["index"], 0);
        assert_eq!(json["id"], "call_abc123");
        assert_eq!(json["type"], "function");
        assert_eq!(json["function"]["name"], "get_weather");
        assert_eq!(json["function"]["arguments"], r#"{"location":"NYC"}"#);
    }

    #[test]
    fn test_chunk_delta_with_tool_calls() {
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion.chunk",
            created: 1234567890,
            model: "test-agent".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![OaiToolCall {
                        index: 0,
                        id: Some("call_1".to_string()),
                        call_type: Some("function"),
                        function: OaiToolCallFunction {
                            name: Some("search".to_string()),
                            arguments: Some(String::new()),
                        },
                    }]),
                },
                finish_reason: None,
            }],
        };
        let json = serde_json::to_value(&chunk).unwrap();
        let tc = &json["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 0);
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "search");
        // content should be omitted
        assert!(json["choices"][0]["delta"].get("content").is_none());
    }

    #[test]
    fn test_tool_input_delta_chunk() {
        // Incremental arguments chunk — no id, no type, no name
        let tc = OaiToolCall {
            index: 2,
            id: None,
            call_type: None,
            function: OaiToolCallFunction {
                name: None,
                arguments: Some(r#"{"q":"rust"}"#.to_string()),
            },
        };
        let json = serde_json::to_value(&tc).unwrap();
        assert_eq!(json["index"], 2);
        // id and type should be omitted
        assert!(json.get("id").is_none());
        assert!(json.get("type").is_none());
        assert!(json["function"].get("name").is_none());
        assert_eq!(json["function"]["arguments"], r#"{"q":"rust"}"#);
    }

    #[test]
    fn test_backward_compat_no_tool_calls() {
        // When tool_calls is None, it should not appear in JSON at all (backward compat)
        let msg = ChoiceMessage {
            role: "assistant",
            content: Some("Hello".to_string()),
            tool_calls: None,
        };
        let json_str = serde_json::to_string(&msg).unwrap();
        assert!(!json_str.contains("tool_calls"));

        let delta = ChunkDelta {
            role: Some("assistant"),
            content: Some("Hi".to_string()),
            tool_calls: None,
        };
        let json_str = serde_json::to_string(&delta).unwrap();
        assert!(!json_str.contains("tool_calls"));
    }
}
