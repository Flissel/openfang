//! Embedding driver for vector-based semantic memory.
//!
//! Provides an `EmbeddingDriver` trait and an OpenAI-compatible implementation
//! that works with any provider offering a `/v1/embeddings` endpoint (OpenAI,
//! Groq, Together, Fireworks, Ollama, etc.).

use crate::retry::{retry_async, RetryConfig, RetryOutcome};
use async_trait::async_trait;
use openfang_types::model_catalog::{
    FIREWORKS_BASE_URL, GROQ_BASE_URL, LMSTUDIO_BASE_URL, MISTRAL_BASE_URL, OLLAMA_BASE_URL,
    OPENAI_BASE_URL, TOGETHER_BASE_URL, VLLM_BASE_URL,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use zeroize::Zeroizing;

/// Error type for embedding operations.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("API error (status {status}): {message}")]
    Api { status: u16, message: String },
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Missing API key: {0}")]
    MissingApiKey(String),
}

/// Configuration for creating an embedding driver.
#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    /// Provider name (openai, groq, together, ollama, etc.).
    pub provider: String,
    /// Model name (e.g., "text-embedding-3-small", "all-MiniLM-L6-v2").
    pub model: String,
    /// API key (resolved from env var).
    pub api_key: String,
    /// Base URL for the API.
    pub base_url: String,
}

/// Trait for computing text embeddings.
#[async_trait]
pub trait EmbeddingDriver: Send + Sync {
    /// Compute embedding vectors for a batch of texts.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError>;

    /// Compute embedding for a single text.
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let results = self.embed(&[text]).await?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::Parse("Empty embedding response".to_string()))
    }

    /// Return the dimensionality of embeddings produced by this driver.
    fn dimensions(&self) -> usize;
}

/// OpenAI-compatible embedding driver.
///
/// Works with any provider that implements the `/v1/embeddings` endpoint:
/// OpenAI, Groq, Together, Fireworks, Ollama, vLLM, LM Studio, etc.
pub struct OpenAIEmbeddingDriver {
    api_key: Zeroizing<String>,
    base_url: String,
    model: String,
    client: reqwest::Client,
    dims: usize,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

#[derive(Debug)]
struct EmbeddingAttemptError {
    error: EmbeddingError,
    retryable: bool,
}

fn is_retryable_embedding_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
}

impl OpenAIEmbeddingDriver {
    /// Create a new OpenAI-compatible embedding driver.
    pub fn new(config: EmbeddingConfig) -> Result<Self, EmbeddingError> {
        // Infer dimensions from model name (common models)
        let dims = infer_dimensions(&config.model);

        Ok(Self {
            api_key: Zeroizing::new(config.api_key),
            base_url: config.base_url,
            model: config.model,
            client: reqwest::Client::new(),
            dims,
        })
    }
}

/// Infer embedding dimensions from model name.
fn infer_dimensions(model: &str) -> usize {
    match model {
        // OpenAI
        "text-embedding-3-small" => 1536,
        "text-embedding-3-large" => 3072,
        "text-embedding-ada-002" => 1536,
        // Sentence Transformers / local models
        "all-MiniLM-L6-v2" => 384,
        "all-MiniLM-L12-v2" => 384,
        "all-mpnet-base-v2" => 768,
        "nomic-embed-text" => 768,
        "mxbai-embed-large" => 1024,
        // Default to 1536 (most common)
        _ => 1536,
    }
}

#[async_trait]
impl EmbeddingDriver for OpenAIEmbeddingDriver {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let url = format!("{}/embeddings", self.base_url);
        let body = EmbedRequest {
            model: &self.model,
            input: texts,
        };

        let retry_config = RetryConfig::default();
        let response = retry_async(
            &retry_config,
            || {
                let mut req = self.client.post(&url).json(&body);
                if !self.api_key.as_str().is_empty() {
                    req = req.header("Authorization", format!("Bearer {}", self.api_key.as_str()));
                }

                async move {
                    let resp = req.send().await.map_err(|error| EmbeddingAttemptError {
                        retryable: error.is_timeout() || error.is_connect(),
                        error: EmbeddingError::Http(error.to_string()),
                    })?;
                    let status = resp.status().as_u16();

                    if status != 200 {
                        let body_text = resp.text().await.unwrap_or_default();
                        return Err(EmbeddingAttemptError {
                            error: EmbeddingError::Api {
                                status,
                                message: body_text,
                            },
                            retryable: is_retryable_embedding_status(status),
                        });
                    }

                    resp.bytes().await.map_err(|error| EmbeddingAttemptError {
                        retryable: error.is_timeout()
                            || error.is_connect()
                            || error.is_body()
                            || error.is_decode(),
                        error: EmbeddingError::Parse(error.to_string()),
                    })
                }
            },
            |error| error.retryable,
            |_| None,
        )
        .await;

        let response_bytes = match response {
            RetryOutcome::Success { result, .. } => result,
            RetryOutcome::Exhausted { last_error, .. } => return Err(last_error.error),
        };

        let data: EmbedResponse = serde_json::from_slice(&response_bytes)
            .map_err(|e| EmbeddingError::Parse(e.to_string()))?;

        // Update dimensions from actual response if available
        let embeddings: Vec<Vec<f32>> = data.data.into_iter().map(|d| d.embedding).collect();

        debug!(
            "Embedded {} texts (dims={})",
            embeddings.len(),
            embeddings.first().map(|e| e.len()).unwrap_or(0)
        );

        Ok(embeddings)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}

/// Create an embedding driver from kernel config.
pub fn create_embedding_driver(
    provider: &str,
    model: &str,
    api_key_env: &str,
    custom_base_url: Option<&str>,
) -> Result<Box<dyn EmbeddingDriver + Send + Sync>, EmbeddingError> {
    let api_key = if api_key_env.is_empty() {
        String::new()
    } else {
        std::env::var(api_key_env).unwrap_or_default()
    };

    let base_url = custom_base_url
        .filter(|u| !u.is_empty())
        .map(|u| {
            let trimmed = u.trim_end_matches('/');
            // All OpenAI-compatible embedding providers need /v1 in the path.
            // If the user supplied a bare host URL (e.g. "http://192.168.0.1:11434"),
            // append /v1 so the final request hits {base}/v1/embeddings.
            let needs_v1 = matches!(
                provider,
                "openai"
                    | "groq"
                    | "together"
                    | "fireworks"
                    | "mistral"
                    | "ollama"
                    | "vllm"
                    | "lmstudio"
            );
            if needs_v1 && !trimmed.ends_with("/v1") {
                format!("{trimmed}/v1")
            } else {
                trimmed.to_string()
            }
        })
        .unwrap_or_else(|| match provider {
            "openai" => OPENAI_BASE_URL.to_string(),
            "groq" => GROQ_BASE_URL.to_string(),
            "together" => TOGETHER_BASE_URL.to_string(),
            "fireworks" => FIREWORKS_BASE_URL.to_string(),
            "mistral" => MISTRAL_BASE_URL.to_string(),
            "ollama" => OLLAMA_BASE_URL.to_string(),
            "vllm" => VLLM_BASE_URL.to_string(),
            "lmstudio" => LMSTUDIO_BASE_URL.to_string(),
            other => {
                warn!("Unknown embedding provider '{other}', using OpenAI-compatible format");
                format!("https://{other}/v1")
            }
        });

    // SECURITY: Warn when embedding requests will be sent to an external API
    let is_local = base_url.contains("localhost")
        || base_url.contains("127.0.0.1")
        || base_url.contains("[::1]");
    if !is_local {
        warn!(
            provider = %provider,
            base_url = %base_url,
            "Embedding driver configured to send data to external API — text content will leave this machine"
        );
    }

    let config = EmbeddingConfig {
        provider: provider.to_string(),
        model: model.to_string(),
        api_key,
        base_url,
    };

    let driver = OpenAIEmbeddingDriver::new(config)?;
    Ok(Box::new(driver))
}

/// Compute cosine similarity between two vectors.
///
/// Returns a value in [-1.0, 1.0] where 1.0 = identical direction.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

/// Serialize an embedding vector to bytes (for SQLite BLOB storage).
pub fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(embedding.len() * 4);
    for &val in embedding {
        bytes.extend_from_slice(&val.to_le_bytes());
    }
    bytes
}

/// Deserialize an embedding vector from bytes.
pub fn embedding_from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_embedding_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, Arc<AtomicUsize>) {
        let responses = responses
            .into_iter()
            .map(|(status, body)| (status, body, body.len()))
            .collect();
        spawn_embedding_server_with_content_lengths(responses).await
    }

    async fn spawn_embedding_server_with_content_lengths(
        responses: Vec<(u16, &'static str, usize)>,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let request_attempts = attempts.clone();

        tokio::spawn(async move {
            for (status, body, content_length) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                stream.read(&mut request).await.unwrap();
                request_attempts.fetch_add(1, Ordering::SeqCst);

                let response = format!(
                    "HTTP/1.1 {status} test\r\nContent-Length: {content_length}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        (format!("http://{address}"), attempts)
    }

    fn test_driver(base_url: String) -> OpenAIEmbeddingDriver {
        OpenAIEmbeddingDriver::new(EmbeddingConfig {
            provider: "openai".to_string(),
            model: "text-embedding-3-small".to_string(),
            api_key: "test-key".to_string(),
            base_url,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn retries_transient_upstream_failure_before_returning_embedding() {
        let (base_url, attempts) = spawn_embedding_server(vec![
            (503, r#"{"error":"temporarily unavailable"}"#),
            (200, r#"{"data":[{"embedding":[0.25,0.5]}]}"#),
        ])
        .await;
        let driver = test_driver(base_url);

        let embeddings = tokio::time::timeout(
            Duration::from_secs(2),
            driver.embed(&["retry this embedding"]),
        )
        .await
        .expect("bounded retry window elapsed")
        .expect("transient failure should recover");

        assert_eq!(embeddings, vec![vec![0.25, 0.5]]);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retries_truncated_successful_embedding_response_before_decoding_json() {
        let valid_response = r#"{"data":[{"embedding":[0.25,0.5]}]}"#;
        let (base_url, attempts) = spawn_embedding_server_with_content_lengths(vec![
            (200, r#"{"data":["#, valid_response.len()),
            (200, valid_response, valid_response.len()),
        ])
        .await;
        let driver = test_driver(base_url);

        let embeddings = tokio::time::timeout(
            Duration::from_secs(2),
            driver.embed(&["retry truncated embedding response"]),
        )
        .await
        .expect("bounded retry window elapsed")
        .expect("truncated response should recover");

        assert_eq!(embeddings, vec![vec![0.25, 0.5]]);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn does_not_retry_permanent_upstream_failure_or_fallback() {
        let (base_url, attempts) = spawn_embedding_server(vec![
            (400, r#"{"error":"invalid embedding request"}"#),
            (200, r#"{"data":[{"embedding":[0.25,0.5]}]}"#),
        ])
        .await;
        let driver = test_driver(base_url);

        let error = driver
            .embed(&["invalid embedding request"])
            .await
            .expect_err("permanent upstream failure must fail closed");

        assert!(matches!(error, EmbeddingError::Api { status: 400, .. }));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn does_not_retry_malformed_successful_embedding_response() {
        let (base_url, attempts) = spawn_embedding_server(vec![
            (200, "not valid embedding json"),
            (200, r#"{"data":[{"embedding":[0.25,0.5]}]}"#),
        ])
        .await;
        let driver = test_driver(base_url);

        let error = driver
            .embed(&["malformed embedding response"])
            .await
            .expect_err("malformed successful response must fail closed");

        assert!(matches!(error, EmbeddingError::Parse(_)));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_real_vectors() {
        let a = vec![0.1, 0.2, 0.3, 0.4];
        let b = vec![0.1, 0.2, 0.3, 0.4];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-5);

        let c = vec![0.4, 0.3, 0.2, 0.1];
        let sim2 = cosine_similarity(&a, &c);
        assert!(sim2 > 0.0 && sim2 < 1.0); // Similar but not identical
    }

    #[test]
    fn test_cosine_similarity_empty() {
        let sim = cosine_similarity(&[], &[]);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_cosine_similarity_length_mismatch() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_embedding_roundtrip() {
        let embedding = vec![0.1, -0.5, 1.23456, 0.0, -1e10, 1e10];
        let bytes = embedding_to_bytes(&embedding);
        let recovered = embedding_from_bytes(&bytes);
        assert_eq!(embedding.len(), recovered.len());
        for (a, b) in embedding.iter().zip(recovered.iter()) {
            assert!((a - b).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn test_embedding_bytes_empty() {
        let bytes = embedding_to_bytes(&[]);
        assert!(bytes.is_empty());
        let recovered = embedding_from_bytes(&bytes);
        assert!(recovered.is_empty());
    }

    #[test]
    fn test_infer_dimensions() {
        assert_eq!(infer_dimensions("text-embedding-3-small"), 1536);
        assert_eq!(infer_dimensions("all-MiniLM-L6-v2"), 384);
        assert_eq!(infer_dimensions("nomic-embed-text"), 768);
        assert_eq!(infer_dimensions("unknown-model"), 1536); // default
    }

    #[test]
    fn test_create_embedding_driver_ollama() {
        // Should succeed even without API key (ollama is local)
        let driver = create_embedding_driver("ollama", "all-MiniLM-L6-v2", "", None);
        assert!(driver.is_ok());
        assert_eq!(driver.unwrap().dimensions(), 384);
    }

    #[test]
    fn test_create_embedding_driver_custom_url_with_v1() {
        // Custom URL already containing /v1 should be used as-is
        let driver = create_embedding_driver(
            "ollama",
            "nomic-embed-text",
            "",
            Some("http://192.168.0.1:11434/v1"),
        );
        assert!(driver.is_ok());
    }

    #[test]
    fn test_create_embedding_driver_custom_url_without_v1() {
        // Custom URL missing /v1 should get it appended for known providers
        let driver = create_embedding_driver(
            "ollama",
            "nomic-embed-text",
            "",
            Some("http://192.168.0.1:11434"),
        );
        assert!(driver.is_ok());
    }

    #[test]
    fn test_create_embedding_driver_custom_url_trailing_slash() {
        // Trailing slash should be trimmed before appending /v1
        let driver = create_embedding_driver(
            "ollama",
            "nomic-embed-text",
            "",
            Some("http://192.168.0.1:11434/"),
        );
        assert!(driver.is_ok());
    }
}
