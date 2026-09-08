//! Real HTTP integration tests for the OpenFang API.
//!
//! These tests boot a real kernel, start a real axum HTTP server on a random
//! port, and hit actual endpoints with reqwest.  No mocking.
//!
//! Tests that require an LLM API call are gated behind GROQ_API_KEY.
//!
//! Run: cargo test -p openfang-api --test api_integration_test -- --nocapture

use axum::Router;
use openfang_api::middleware;
use openfang_api::routes::{self, AppState};
use openfang_api::ws;
use openfang_kernel::OpenFangKernel;
use openfang_types::config::{DefaultModelConfig, KernelConfig};
use std::sync::Arc;
use std::time::Instant;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

struct TestServer {
    base_url: String,
    state: Arc<AppState>,
    _tmp: tempfile::TempDir,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.state.kernel.shutdown();
    }
}

/// Start a test server using ollama as default provider (no API key needed).
/// This lets the kernel boot without any real LLM credentials.
/// Tests that need actual LLM calls should use `start_test_server_with_llm()`.
async fn start_test_server() -> TestServer {
    start_test_server_with_provider("ollama", "test-model", "OLLAMA_API_KEY").await
}

/// Start a test server with Groq as the LLM provider (requires GROQ_API_KEY).
async fn start_test_server_with_llm() -> TestServer {
    start_test_server_with_provider("groq", "llama-3.3-70b-versatile", "GROQ_API_KEY").await
}

async fn start_test_server_with_provider(
    provider: &str,
    model: &str,
    api_key_env: &str,
) -> TestServer {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");

    let config = KernelConfig {
        home_dir: tmp.path().to_path_buf(),
        data_dir: tmp.path().join("data"),
        default_model: DefaultModelConfig {
            provider: provider.to_string(),
            model: model.to_string(),
            api_key_env: api_key_env.to_string(),
            base_url: None,
        },
        ..KernelConfig::default()
    };

    let kernel = OpenFangKernel::boot_with_config(config).expect("Kernel should boot");
    let kernel = Arc::new(kernel);
    kernel.set_self_handle();

    let budget = kernel.config.budget.clone();
    let state = Arc::new(AppState {
        kernel,
        started_at: Instant::now(),
        peer_registry: None,
        bridge_manager: tokio::sync::Mutex::new(None),
        channels_config: tokio::sync::RwLock::new(Default::default()),
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        clawhub_cache: dashmap::DashMap::new(),
        provider_probe_cache: openfang_runtime::provider_health::ProbeCache::new(),
        budget_config: Arc::new(tokio::sync::RwLock::new(budget)),
        issuable_credentials: Default::default(),
    });

    let app = Router::new()
        .route("/api/health", axum::routing::get(routes::health))
        .route("/api/status", axum::routing::get(routes::status))
        .route(
            "/api/agents",
            axum::routing::get(routes::list_agents).post(routes::spawn_agent),
        )
        .route(
            "/api/agents/{id}/message",
            axum::routing::post(routes::send_message),
        )
        .route(
            "/api/agents/{id}/session",
            axum::routing::get(routes::get_agent_session),
        )
        .route("/api/agents/{id}/ws", axum::routing::get(ws::agent_ws))
        .route(
            "/api/agents/{id}",
            axum::routing::delete(routes::kill_agent),
        )
        .route(
            "/api/triggers",
            axum::routing::get(routes::list_triggers).post(routes::create_trigger),
        )
        .route(
            "/api/triggers/{id}",
            axum::routing::delete(routes::delete_trigger),
        )
        .route(
            "/api/workflows",
            axum::routing::get(routes::list_workflows).post(routes::create_workflow),
        )
        .route(
            "/api/workflows/{id}/run",
            axum::routing::post(routes::run_workflow),
        )
        .route(
            "/api/workflows/{id}/runs",
            axum::routing::get(routes::list_workflow_runs),
        )
        .route("/api/shutdown", axum::routing::post(routes::shutdown))
        .layer(axum::middleware::from_fn(middleware::request_logging))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind test server");
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestServer {
        base_url: format!("http://{}", addr),
        state,
        _tmp: tmp,
    }
}

/// Manifest that uses ollama (no API key required, won't make real LLM calls).
const TEST_MANIFEST: &str = r#"
name = "test-agent"
version = "0.1.0"
description = "Integration test agent"
author = "test"
module = "builtin:chat"

[model]
provider = "ollama"
model = "test-model"
system_prompt = "You are a test agent. Reply concisely."

[capabilities]
tools = ["file_read"]
memory_read = ["*"]
memory_write = ["self.*"]
"#;

/// Manifest that uses Groq for real LLM tests.
const LLM_MANIFEST: &str = r#"
name = "test-agent"
version = "0.1.0"
description = "Integration test agent"
author = "test"
module = "builtin:chat"

[model]
provider = "groq"
model = "llama-3.3-70b-versatile"
system_prompt = "You are a test agent. Reply concisely."

[capabilities]
tools = ["file_read"]
memory_read = ["*"]
memory_write = ["self.*"]
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_endpoint() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/health", server.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    // Middleware injects x-request-id
    assert!(resp.headers().contains_key("x-request-id"));

    let body: serde_json::Value = resp.json().await.unwrap();
    // Public health endpoint returns minimal info (redacted for security)
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());
    // Detailed fields should NOT appear in public health endpoint
    assert!(body["database"].is_null());
    assert!(body["agent_count"].is_null());
}

#[tokio::test]
async fn test_status_endpoint() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/status", server.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "running");
    assert_eq!(body["agent_count"], 1); // default assistant auto-spawned
    assert!(body["uptime_seconds"].is_number());
    assert_eq!(body["default_provider"], "ollama");
    assert_eq!(body["agents"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_spawn_list_kill_agent() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // --- Spawn ---
    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": TEST_MANIFEST}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "test-agent");
    let agent_id = body["agent_id"].as_str().unwrap().to_string();
    assert!(!agent_id.is_empty());

    // --- List (2 agents: default assistant + test-agent) ---
    let resp = client
        .get(format!("{}/api/agents", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let agents: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(agents.len(), 2);
    let test_agent = agents.iter().find(|a| a["name"] == "test-agent").unwrap();
    assert_eq!(test_agent["id"], agent_id);
    assert_eq!(test_agent["model_provider"], "ollama");

    // --- Kill ---
    let resp = client
        .delete(format!("{}/api/agents/{}", server.base_url, agent_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "killed");

    // --- List (only default assistant remains) ---
    let resp = client
        .get(format!("{}/api/agents", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let agents: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["name"], "assistant");
}

#[tokio::test]
async fn test_agent_session_empty() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Spawn agent
    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": TEST_MANIFEST}))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let agent_id = body["agent_id"].as_str().unwrap();

    // Session should be empty — no messages sent yet
    let resp = client
        .get(format!(
            "{}/api/agents/{}/session",
            server.base_url, agent_id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["message_count"], 0);
    assert_eq!(body["messages"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_send_message_with_llm() {
    if std::env::var("GROQ_API_KEY").is_err() {
        eprintln!("GROQ_API_KEY not set, skipping LLM integration test");
        return;
    }

    let server = start_test_server_with_llm().await;
    let client = reqwest::Client::new();

    // Spawn
    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": LLM_MANIFEST}))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let agent_id = body["agent_id"].as_str().unwrap().to_string();

    // Send message through the real HTTP endpoint → kernel → Groq LLM
    let resp = client
        .post(format!(
            "{}/api/agents/{}/message",
            server.base_url, agent_id
        ))
        .json(&serde_json::json!({"message": "Say hello in exactly 3 words."}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let response_text = body["response"].as_str().unwrap();
    assert!(
        !response_text.is_empty(),
        "LLM response should not be empty"
    );
    assert!(body["input_tokens"].as_u64().unwrap() > 0);
    assert!(body["output_tokens"].as_u64().unwrap() > 0);

    // Session should now have messages
    let resp = client
        .get(format!(
            "{}/api/agents/{}/session",
            server.base_url, agent_id
        ))
        .send()
        .await
        .unwrap();
    let session: serde_json::Value = resp.json().await.unwrap();
    assert!(session["message_count"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_workflow_crud() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Spawn agent for workflow
    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": TEST_MANIFEST}))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let agent_name = body["name"].as_str().unwrap().to_string();

    // Create workflow
    let resp = client
        .post(format!("{}/api/workflows", server.base_url))
        .json(&serde_json::json!({
            "name": "test-workflow",
            "description": "Integration test workflow",
            "steps": [
                {
                    "name": "step1",
                    "agent_name": agent_name,
                    "prompt": "Echo: {{input}}",
                    "mode": "sequential",
                    "timeout_secs": 30
                }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let workflow_id = body["workflow_id"].as_str().unwrap().to_string();
    assert!(!workflow_id.is_empty());

    // List workflows
    let resp = client
        .get(format!("{}/api/workflows", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let workflows: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0]["name"], "test-workflow");
    assert_eq!(workflows[0]["steps"], 1);
}

#[tokio::test]
async fn test_trigger_crud() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Spawn agent for trigger
    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": TEST_MANIFEST}))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let agent_id = body["agent_id"].as_str().unwrap().to_string();

    // Create trigger (Lifecycle pattern — simplest variant)
    let resp = client
        .post(format!("{}/api/triggers", server.base_url))
        .json(&serde_json::json!({
            "agent_id": agent_id,
            "pattern": "lifecycle",
            "prompt_template": "Handle: {{event}}",
            "max_fires": 5
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let trigger_id = body["trigger_id"].as_str().unwrap().to_string();
    assert_eq!(body["agent_id"], agent_id);

    // List triggers (unfiltered)
    let resp = client
        .get(format!("{}/api/triggers", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let triggers: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0]["agent_id"], agent_id);
    assert_eq!(triggers[0]["enabled"], true);
    assert_eq!(triggers[0]["max_fires"], 5);

    // List triggers (filtered by agent_id)
    let resp = client
        .get(format!(
            "{}/api/triggers?agent_id={}",
            server.base_url, agent_id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let triggers: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(triggers.len(), 1);

    // Delete trigger
    let resp = client
        .delete(format!("{}/api/triggers/{}", server.base_url, trigger_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // List triggers (should be empty)
    let resp = client
        .get(format!("{}/api/triggers", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let triggers: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(triggers.len(), 0);
}

#[tokio::test]
async fn test_invalid_agent_id_returns_400() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Send message to invalid ID
    let resp = client
        .post(format!("{}/api/agents/not-a-uuid/message", server.base_url))
        .json(&serde_json::json!({"message": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("Invalid"));

    // Kill invalid ID
    let resp = client
        .delete(format!("{}/api/agents/not-a-uuid", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Session for invalid ID
    let resp = client
        .get(format!("{}/api/agents/not-a-uuid/session", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn test_kill_nonexistent_agent_returns_404() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let fake_id = uuid::Uuid::new_v4();
    let resp = client
        .delete(format!("{}/api/agents/{}", server.base_url, fake_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn test_spawn_invalid_manifest_returns_400() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": "this is {{ not valid toml"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("Invalid manifest"));
}

#[tokio::test]
async fn test_request_id_header_is_uuid() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/health", server.base_url))
        .send()
        .await
        .unwrap();

    let request_id = resp
        .headers()
        .get("x-request-id")
        .expect("x-request-id header should be present");
    let id_str = request_id.to_str().unwrap();
    assert!(
        uuid::Uuid::parse_str(id_str).is_ok(),
        "x-request-id should be a valid UUID, got: {}",
        id_str
    );
}

#[tokio::test]
async fn test_multiple_agents_lifecycle() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Spawn 3 agents
    let mut ids = Vec::new();
    for i in 0..3 {
        let manifest = format!(
            r#"
name = "agent-{i}"
version = "0.1.0"
description = "Multi-agent test {i}"
author = "test"
module = "builtin:chat"

[model]
provider = "ollama"
model = "test-model"
system_prompt = "Agent {i}."

[capabilities]
memory_read = ["*"]
memory_write = ["self.*"]
"#
        );

        let resp = client
            .post(format!("{}/api/agents", server.base_url))
            .json(&serde_json::json!({"manifest_toml": manifest}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let body: serde_json::Value = resp.json().await.unwrap();
        ids.push(body["agent_id"].as_str().unwrap().to_string());
    }

    // List should show 4 (3 spawned + default assistant)
    let resp = client
        .get(format!("{}/api/agents", server.base_url))
        .send()
        .await
        .unwrap();
    let agents: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(agents.len(), 4);

    // Status should agree
    let resp = client
        .get(format!("{}/api/status", server.base_url))
        .send()
        .await
        .unwrap();
    let status: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status["agent_count"], 4);

    // Kill one
    let resp = client
        .delete(format!("{}/api/agents/{}", server.base_url, ids[1]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // List should show 3 (2 spawned + default assistant)
    let resp = client
        .get(format!("{}/api/agents", server.base_url))
        .send()
        .await
        .unwrap();
    let agents: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(agents.len(), 3);

    // Kill the rest
    for id in [&ids[0], &ids[2]] {
        client
            .delete(format!("{}/api/agents/{}", server.base_url, id))
            .send()
            .await
            .unwrap();
    }

    // List should have only default assistant
    let resp = client
        .get(format!("{}/api/agents", server.base_url))
        .send()
        .await
        .unwrap();
    let agents: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(agents.len(), 1);
}

// ---------------------------------------------------------------------------
// Auth integration tests
// ---------------------------------------------------------------------------

/// Start a test server with Bearer-token authentication enabled.
async fn start_test_server_with_auth(api_key: &str) -> TestServer {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");

    let config = KernelConfig {
        home_dir: tmp.path().to_path_buf(),
        data_dir: tmp.path().join("data"),
        api_key: api_key.to_string(),
        default_model: DefaultModelConfig {
            provider: "ollama".to_string(),
            model: "test-model".to_string(),
            api_key_env: "OLLAMA_API_KEY".to_string(),
            base_url: None,
        },
        ..KernelConfig::default()
    };

    let kernel = OpenFangKernel::boot_with_config(config).expect("Kernel should boot");
    let kernel = Arc::new(kernel);
    kernel.set_self_handle();

    let budget = kernel.config.budget.clone();
    let state = Arc::new(AppState {
        kernel,
        started_at: Instant::now(),
        peer_registry: None,
        bridge_manager: tokio::sync::Mutex::new(None),
        channels_config: tokio::sync::RwLock::new(Default::default()),
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        clawhub_cache: dashmap::DashMap::new(),
        provider_probe_cache: openfang_runtime::provider_health::ProbeCache::new(),
        budget_config: Arc::new(tokio::sync::RwLock::new(budget)),
        issuable_credentials: Default::default(),
    });

    let api_key = state.kernel.config.api_key.trim().to_string();
    let auth_state = middleware::AuthState {
        api_key: api_key.clone(),
        auth_enabled: state.kernel.config.auth.enabled,
        session_secret: if !api_key.is_empty() {
            api_key.clone()
        } else if state.kernel.config.auth.enabled {
            state.kernel.config.auth.password_hash.clone()
        } else {
            String::new()
        },
    };

    let app = Router::new()
        .route("/api/health", axum::routing::get(routes::health))
        .route("/api/status", axum::routing::get(routes::status))
        .route(
            "/api/agents",
            axum::routing::get(routes::list_agents).post(routes::spawn_agent),
        )
        .route(
            "/api/agents/{id}/message",
            axum::routing::post(routes::send_message),
        )
        .route(
            "/api/agents/{id}/session",
            axum::routing::get(routes::get_agent_session),
        )
        .route("/api/agents/{id}/ws", axum::routing::get(ws::agent_ws))
        .route(
            "/api/agents/{id}",
            axum::routing::delete(routes::kill_agent),
        )
        .route(
            "/api/triggers",
            axum::routing::get(routes::list_triggers).post(routes::create_trigger),
        )
        .route(
            "/api/triggers/{id}",
            axum::routing::delete(routes::delete_trigger),
        )
        .route(
            "/api/workflows",
            axum::routing::get(routes::list_workflows).post(routes::create_workflow),
        )
        .route(
            "/api/workflows/{id}/run",
            axum::routing::post(routes::run_workflow),
        )
        .route(
            "/api/workflows/{id}/runs",
            axum::routing::get(routes::list_workflow_runs),
        )
        .route("/api/shutdown", axum::routing::post(routes::shutdown))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            middleware::auth,
        ))
        .layer(axum::middleware::from_fn(middleware::request_logging))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind test server");
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestServer {
        base_url: format!("http://{}", addr),
        state,
        _tmp: tmp,
    }
}

#[tokio::test]
async fn test_auth_health_is_public() {
    let server = start_test_server_with_auth("secret-key-123").await;
    let client = reqwest::Client::new();

    // /api/health should be accessible without auth
    let resp = client
        .get(format!("{}/api/health", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_auth_rejects_no_token() {
    let server = start_test_server_with_auth("secret-key-123").await;
    let client = reqwest::Client::new();

    // Protected endpoint without auth header → 401
    // Note: /api/status is public (dashboard needs it), so use a protected endpoint
    let resp = client
        .get(format!("{}/api/commands", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("Missing"));
}

#[tokio::test]
async fn test_auth_rejects_wrong_token() {
    let server = start_test_server_with_auth("secret-key-123").await;
    let client = reqwest::Client::new();

    // Wrong bearer token → 401
    // Note: /api/status is public (dashboard needs it), so use a protected endpoint
    let resp = client
        .get(format!("{}/api/commands", server.base_url))
        .header("authorization", "Bearer wrong-key")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("Invalid"));
}

#[tokio::test]
async fn test_auth_accepts_correct_token() {
    let server = start_test_server_with_auth("secret-key-123").await;
    let client = reqwest::Client::new();

    // Correct bearer token → 200
    let resp = client
        .get(format!("{}/api/status", server.base_url))
        .header("authorization", "Bearer secret-key-123")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "running");
}

#[tokio::test]
async fn test_auth_disabled_when_no_key() {
    // Empty API key = auth disabled
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Protected endpoint accessible without auth when no key is configured
    let resp = client
        .get(format!("{}/api/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------------
// Credential issuance tests -- POST /api/credentials/issue
// ---------------------------------------------------------------------------

/// Bearer token every credential-issuance test server is configured with.
const CRED_API_KEY: &str = "cred-test-key-123";

/// Start a test server that mounts `POST /api/credentials/issue` behind the
/// real auth middleware.
///
/// `issuable` is the server-side allowlist. It is threaded through `AppState`
/// rather than read from `std::env` at request time, so tests running in
/// parallel cannot race each other over one process-wide variable.
///
/// `credentials` are written to `<home>/.env` *before* the kernel boots -- that
/// dotenv file is the credential resolver's second lookup source, so it seeds
/// `Kernel::resolve_credential` per test without touching the process
/// environment either.
async fn start_credential_test_server(
    issuable: &[&str],
    credentials: &[(&str, &str)],
) -> TestServer {
    start_credential_test_server_with_key(CRED_API_KEY, issuable, credentials).await
}

/// Same, but with the daemon's `api_key` under the test's control.
///
/// Passing `""` reproduces the fail-open configuration: `middleware::auth`
/// waves every request through, exactly as a local daemon with no `api_key`
/// and no dashboard auth would.
async fn start_credential_test_server_with_key(
    api_key: &str,
    issuable: &[&str],
    credentials: &[(&str, &str)],
) -> TestServer {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");

    if !credentials.is_empty() {
        let dotenv: String = credentials
            .iter()
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect();
        std::fs::write(tmp.path().join(".env"), dotenv).expect("Failed to write test .env");
    }

    let config = KernelConfig {
        home_dir: tmp.path().to_path_buf(),
        data_dir: tmp.path().join("data"),
        api_key: api_key.to_string(),
        default_model: DefaultModelConfig {
            provider: "ollama".to_string(),
            model: "test-model".to_string(),
            api_key_env: "OLLAMA_API_KEY".to_string(),
            base_url: None,
        },
        ..KernelConfig::default()
    };

    let kernel = OpenFangKernel::boot_with_config(config).expect("Kernel should boot");
    let kernel = Arc::new(kernel);
    kernel.set_self_handle();

    let budget = kernel.config.budget.clone();
    let state = Arc::new(AppState {
        kernel,
        started_at: Instant::now(),
        peer_registry: None,
        bridge_manager: tokio::sync::Mutex::new(None),
        channels_config: tokio::sync::RwLock::new(Default::default()),
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        clawhub_cache: dashmap::DashMap::new(),
        provider_probe_cache: openfang_runtime::provider_health::ProbeCache::new(),
        budget_config: Arc::new(tokio::sync::RwLock::new(budget)),
        issuable_credentials: tokio::sync::RwLock::new(
            issuable.iter().map(|s| s.to_string()).collect(),
        ),
    });

    let api_key = state.kernel.config.api_key.trim().to_string();
    let auth_state = middleware::AuthState {
        api_key: api_key.clone(),
        auth_enabled: state.kernel.config.auth.enabled,
        session_secret: api_key.clone(),
    };

    let app = Router::new()
        .route(
            "/api/credentials/issue",
            axum::routing::post(routes::issue_credential),
        )
        .route(
            "/api/credentials/store",
            axum::routing::post(routes::store_credential),
        )
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            middleware::auth,
        ))
        .layer(axum::middleware::from_fn(middleware::request_logging))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind test server");
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestServer {
        base_url: format!("http://{}", addr),
        state,
        _tmp: tmp,
    }
}

/// The endpoint is a write route and is deliberately NOT in the middleware's
/// public-path allowlist, so an unauthenticated call is rejected before the
/// handler ever sees the reference.
#[tokio::test]
async fn test_credential_issue_requires_auth() {
    let server =
        start_credential_test_server(&["ROWBOAT_TEST_TOKEN"], &[("ROWBOAT_TEST_TOKEN", "seeded")])
            .await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/api/credentials/issue", server.base_url))
        .json(&serde_json::json!({"reference": "ROWBOAT_TEST_TOKEN"}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    // The middleware's own error shape, not anything credential-shaped.
    assert!(body["error"].as_str().unwrap().contains("Missing"));
    assert!(
        body["value"].is_null(),
        "unauthenticated 401 body must not carry a credential field"
    );
}

/// An allowlisted, resolvable reference is issued, and the response is marked
/// uncacheable.
#[tokio::test]
async fn test_credential_issue_returns_value_with_no_store() {
    const SEEDED: &str = "test-value-not-a-real-secret";
    let server =
        start_credential_test_server(&["ROWBOAT_TEST_TOKEN"], &[("ROWBOAT_TEST_TOKEN", SEEDED)])
            .await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/api/credentials/issue", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_TEST_TOKEN"}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    // The handler sets exactly `no-store`. In the real daemon the
    // `security_headers` middleware (crates/openfang-api/src/middleware.rs)
    // overwrites this header with the wider `no-store, no-cache,
    // must-revalidate`, and that layer is not mounted on this test router.
    // Assert containment so the test holds in both worlds rather than
    // describing something production never returns.
    let cache_control = resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .expect("credential response must carry a cache-control header");
    assert!(
        cache_control.contains("no-store"),
        "credential response must forbid storage, got: {cache_control}"
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["reference"], "ROWBOAT_TEST_TOKEN");
    // Compared without printing either side into the failure output.
    assert!(
        body["value"].as_str() == Some(SEEDED),
        "issued value did not match the credential the kernel resolves"
    );
    // Nothing that pretends a scoped, expiring token was minted.
    assert!(body["expires_at"].is_null());
}

/// Allowlisted, but the kernel cannot resolve it.
#[tokio::test]
async fn test_credential_issue_allowlisted_but_unresolvable() {
    let server = start_credential_test_server(&["ROWBOAT_ALLOWED_MISSING"], &[]).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/api/credentials/issue", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_ALLOWED_MISSING"}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "credential_unavailable");
}

/// The load-bearing one: a reference the kernel CAN resolve but that is not on
/// the allowlist must be refused, and that refusal must be byte-identical to
/// the refusal for an allowlisted-but-unresolvable reference. Anything less
/// turns the endpoint into an oracle that enumerates the daemon's secrets.
#[tokio::test]
async fn test_credential_issue_refusals_are_indistinguishable() {
    const SECRET: &str = "must-not-be-issued";
    let server = start_credential_test_server(
        &["ROWBOAT_ALLOWED_MISSING"],
        &[("ROWBOAT_SECRET_NOT_ALLOWED", SECRET)],
    )
    .await;
    let client = reqwest::Client::new();

    // Resolvable by the kernel, but not allowlisted.
    let not_allowlisted = client
        .post(format!("{}/api/credentials/issue", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_SECRET_NOT_ALLOWED"}))
        .send()
        .await
        .unwrap();

    // Allowlisted, but unresolvable.
    let unresolvable = client
        .post(format!("{}/api/credentials/issue", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_ALLOWED_MISSING"}))
        .send()
        .await
        .unwrap();

    assert_eq!(not_allowlisted.status(), 404);
    assert_eq!(not_allowlisted.status(), unresolvable.status());
    assert_eq!(
        not_allowlisted
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        unresolvable
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
    );

    let not_allowlisted_body = not_allowlisted.text().await.unwrap();
    let unresolvable_body = unresolvable.text().await.unwrap();
    assert_eq!(
        not_allowlisted_body,
        "{\"error\":\"credential_unavailable\"}"
    );
    assert_eq!(not_allowlisted_body, unresolvable_body);
    assert!(
        !not_allowlisted_body.contains(SECRET),
        "refusal body leaked the credential value"
    );
}

/// An empty allowlist means the feature is off: even a well-formed, resolvable
/// reference is refused.
#[tokio::test]
async fn test_credential_issue_empty_allowlist_refuses_everything() {
    let server =
        start_credential_test_server(&[], &[("ROWBOAT_TEST_TOKEN", "seeded-but-unreachable")])
            .await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/api/credentials/issue", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_TEST_TOKEN"}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "{\"error\":\"credential_unavailable\"}");
    assert!(!body.contains("seeded-but-unreachable"));
}

/// A reference that could never be an env-var name is a caller bug, not a fact
/// about which secrets exist -- so it gets its own status.
#[tokio::test]
async fn test_credential_issue_rejects_malformed_reference() {
    let server =
        start_credential_test_server(&["ROWBOAT_TEST_TOKEN"], &[("ROWBOAT_TEST_TOKEN", "seeded")])
            .await;
    let client = reqwest::Client::new();

    let too_long = "A".repeat(200);
    for reference in ["a b", "", too_long.as_str(), "../../etc/passwd"] {
        let resp = client
            .post(format!("{}/api/credentials/issue", server.base_url))
            .bearer_auth(CRED_API_KEY)
            .json(&serde_json::json!({"reference": reference}))
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            400,
            "malformed reference of length {} should be rejected",
            reference.len()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "reference_invalid");
    }
}

/// A daemon with no `api_key` and no dashboard auth runs `middleware::auth` in
/// pass-through mode: nothing authenticates the caller. An endpoint that
/// dispenses stored secrets must not operate in that configuration — and its
/// refusal must be the ordinary one, or the refusal itself would advertise
/// how the daemon is configured.
#[tokio::test]
async fn test_credential_issue_refused_when_daemon_has_no_auth() {
    const SEEDED: &str = "must-not-be-issued-without-auth";

    // Fail-open daemon: empty api_key, and the reference IS allowlisted and IS
    // resolvable — so only the guard can be what refuses it.
    let open = start_credential_test_server_with_key(
        "",
        &["ROWBOAT_TEST_TOKEN"],
        &[("ROWBOAT_TEST_TOKEN", SEEDED)],
    )
    .await;
    let client = reqwest::Client::new();

    let refused_by_guard = client
        .post(format!("{}/api/credentials/issue", open.base_url))
        .json(&serde_json::json!({"reference": "ROWBOAT_TEST_TOKEN"}))
        .send()
        .await
        .unwrap();

    assert_eq!(refused_by_guard.status(), 404);

    // The same refusal an authenticated caller gets for an unresolvable
    // reference, from a normally configured daemon.
    let guarded = start_credential_test_server(&["ROWBOAT_ALLOWED_MISSING"], &[]).await;
    let ordinary_refusal = client
        .post(format!("{}/api/credentials/issue", guarded.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_ALLOWED_MISSING"}))
        .send()
        .await
        .unwrap();

    assert_eq!(refused_by_guard.status(), ordinary_refusal.status());
    assert_eq!(
        refused_by_guard
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        ordinary_refusal
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
    );

    let guard_body = refused_by_guard.text().await.unwrap();
    let ordinary_body = ordinary_refusal.text().await.unwrap();
    assert_eq!(guard_body, r#"{"error":"credential_unavailable"}"#);
    assert_eq!(
        guard_body, ordinary_body,
        "the fail-open guard must be indistinguishable from an ordinary refusal"
    );
    assert!(
        !guard_body.contains(SEEDED),
        "guard refusal leaked the credential value"
    );
}

/// The endpoint documents exactly two error bodies. A `reference` that is
/// null, a number, or missing, and a body that is not JSON at all, must all
/// produce `400 reference_invalid` — not axum's own text/plain 415/422.
#[tokio::test]
async fn test_credential_issue_rejects_non_string_and_unparseable_bodies() {
    let server =
        start_credential_test_server(&["ROWBOAT_TEST_TOKEN"], &[("ROWBOAT_TEST_TOKEN", "seeded")])
            .await;
    let client = reqwest::Client::new();

    for body in [
        serde_json::json!({"reference": null}),
        serde_json::json!({"reference": 42}),
        serde_json::json!({"reference": ["ROWBOAT_TEST_TOKEN"]}),
        serde_json::json!({}),
        serde_json::json!("ROWBOAT_TEST_TOKEN"),
    ] {
        let resp = client
            .post(format!("{}/api/credentials/issue", server.base_url))
            .bearer_auth(CRED_API_KEY)
            .json(&body)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 400, "body {body} should be rejected");
        let parsed: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(parsed["error"], "reference_invalid");
    }

    // No Content-Type: application/json, and not JSON either.
    let resp = client
        .post(format!("{}/api/credentials/issue", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .header("content-type", "text/plain")
        .body("reference=ROWBOAT_TEST_TOKEN")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let parsed: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(parsed["error"], "reference_invalid");
}

// ---------------------------------------------------------------------------
// Credential store tests -- POST /api/credentials/store
// ---------------------------------------------------------------------------
//
// `start_credential_test_server` above already mounts `/api/credentials/store`
// alongside `/issue` behind the same auth middleware, so the four rejection
// paths (Schritt 1) -- none of which ever reach the vault -- reuse it as-is.
//
// The other two tests need a REAL, decryptable vault, because the store
// handler refuses (503) when there is nowhere durable to put the value. A
// fixed test-only master key plus `OPENFANG_VAULT_KEY` is this codebase's own
// documented headless/CI escape hatch (see the module doc on
// `crates/openfang-extensions/src/vault.rs`) -- not a new mechanism invented
// for this test.

/// Fixed 32-byte vault master key used only by these tests. Never derived
/// from anything secret, never written to the real OS keyring/keyring-file
/// fallback (that path is only consulted if `OPENFANG_VAULT_KEY` is unset,
/// which it never is here).
const TEST_VAULT_KEY: [u8; 32] = [7u8; 32];

fn test_vault_key_b64() -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, TEST_VAULT_KEY)
}

/// Guards every test that sets the process-wide `OPENFANG_VAULT_KEY` env var.
///
/// `CredentialVault::unlock()` (called once, synchronously, inside
/// `OpenFangKernel::boot_with_config`) reads this var exactly once per boot.
/// Holding this lock across "set the var, boot, unset the var" stops two
/// vault-backed tests running concurrently on different threads from one
/// reading the other's key mid-boot -- the same class of hazard the existing
/// `start_credential_test_server` doc comment calls out for `issuable`.
static VAULT_KEY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Create a vault at `<home>/vault.enc`, seeded with `entries`, using the
/// fixed test key. Must run before `OpenFangKernel::boot_with_config` --
/// the kernel only auto-unlocks a vault that already exists on disk.
fn seed_vault(home: &std::path::Path, entries: &[(&str, &str)]) {
    let mut vault = openfang_extensions::vault::CredentialVault::new(home.join("vault.enc"));
    vault
        .init_with_key(zeroize::Zeroizing::new(TEST_VAULT_KEY))
        .expect("test vault should init with an explicit key");
    for (k, v) in entries {
        vault
            .set((*k).to_string(), zeroize::Zeroizing::new((*v).to_string()))
            .expect("test vault should accept a seeded entry");
    }
}

/// Boot a credential-capable daemon (mounting both `/issue` and `/store`)
/// against `home`, without creating a temp dir of its own -- the caller owns
/// `home`'s lifetime. This is what lets the restart test boot a second,
/// independent kernel against the very same `OPENFANG_HOME` the first one
/// used.
///
/// `issuable` seeds the allowlist the same way `OPENFANG_ISSUABLE_CREDENTIALS`
/// would in production; it is unioned with whatever
/// `routes::seed_issuable_credentials` finds already persisted under `home`
/// -- the exact same production code path `server.rs` calls at boot, so this
/// helper cannot pass by exercising different logic than the real daemon.
async fn boot_credential_server(
    home: &std::path::Path,
    issuable: &[&str],
) -> (String, Arc<AppState>) {
    let config = KernelConfig {
        home_dir: home.to_path_buf(),
        data_dir: home.join("data"),
        api_key: CRED_API_KEY.to_string(),
        default_model: DefaultModelConfig {
            provider: "ollama".to_string(),
            model: "test-model".to_string(),
            api_key_env: "OLLAMA_API_KEY".to_string(),
            base_url: None,
        },
        ..KernelConfig::default()
    };

    let kernel = {
        let _guard = VAULT_KEY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded w.r.t. this var while `_guard` is held --
        // every other reader/writer of OPENFANG_VAULT_KEY in this test binary
        // takes the same lock first.
        unsafe {
            std::env::set_var("OPENFANG_VAULT_KEY", test_vault_key_b64());
        }
        let kernel = OpenFangKernel::boot_with_config(config).expect("Kernel should boot");
        unsafe {
            std::env::remove_var("OPENFANG_VAULT_KEY");
        }
        kernel
    };
    let kernel = Arc::new(kernel);
    kernel.set_self_handle();

    let mut seeded: std::collections::HashSet<String> =
        issuable.iter().map(|s| s.to_string()).collect();
    seeded.extend(routes::seed_issuable_credentials(&kernel.config.home_dir));

    let budget = kernel.config.budget.clone();
    let state = Arc::new(AppState {
        kernel,
        started_at: Instant::now(),
        peer_registry: None,
        bridge_manager: tokio::sync::Mutex::new(None),
        channels_config: tokio::sync::RwLock::new(Default::default()),
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        clawhub_cache: dashmap::DashMap::new(),
        provider_probe_cache: openfang_runtime::provider_health::ProbeCache::new(),
        budget_config: Arc::new(tokio::sync::RwLock::new(budget)),
        issuable_credentials: tokio::sync::RwLock::new(seeded),
    });

    let api_key = state.kernel.config.api_key.trim().to_string();
    let auth_state = middleware::AuthState {
        api_key: api_key.clone(),
        auth_enabled: state.kernel.config.auth.enabled,
        session_secret: api_key.clone(),
    };

    let app = Router::new()
        .route(
            "/api/credentials/issue",
            axum::routing::post(routes::issue_credential),
        )
        .route(
            "/api/credentials/store",
            axum::routing::post(routes::store_credential),
        )
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            middleware::auth,
        ))
        .layer(axum::middleware::from_fn(middleware::request_logging))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind test server");
    let addr = listener.local_addr().unwrap();

    let server_state = state.clone();
    tokio::spawn(async move {
        let _keep_alive = server_state;
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{}", addr), state)
}

/// Schritt 1: the four ways `/api/credentials/store` refuses a request.
/// Grouped into one test against one server, because none of the four paths
/// ever reach the vault -- they are all refused earlier, so there is nothing
/// vault-specific to isolate per case.
#[tokio::test]
async fn test_credential_store_rejects_bad_requests() {
    const EXISTING: &str = "already-there-not-a-real-secret";
    let server = start_credential_test_server(
        &["ROWBOAT_STORE_TOKEN", "ROWBOAT_EXISTING_TOKEN"],
        &[("ROWBOAT_EXISTING_TOKEN", EXISTING)],
    )
    .await;
    let client = reqwest::Client::new();

    // 1. No bearer token.
    let resp = client
        .post(format!("{}/api/credentials/store", server.base_url))
        .json(&serde_json::json!({
            "reference": "ROWBOAT_STORE_TOKEN",
            "value": "irrelevant-not-a-real-secret"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // 2. Invalid reference name.
    let resp = client
        .post(format!("{}/api/credentials/store", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({
            "reference": "not a valid name",
            "value": "irrelevant-not-a-real-secret"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "reference_invalid");

    // 3a. Empty value.
    let resp = client
        .post(format!("{}/api/credentials/store", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_STORE_TOKEN", "value": ""}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "reference_invalid");

    // 3b. NUL byte in value.
    let resp = client
        .post(format!("{}/api/credentials/store", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({
            "reference": "ROWBOAT_STORE_TOKEN",
            "value": "bad\u{0000}value"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "reference_invalid");

    // 4. Reference already resolvable (seeded via `.env`), no `overwrite` --
    // refused, and the existing value is unchanged afterward. Verified via
    // `/issue` (the same resolution path a real caller would exercise)
    // rather than by reading `.env` back, so this proves the API's view of
    // the world, not just the file on disk.
    let resp = client
        .post(format!("{}/api/credentials/store", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({
            "reference": "ROWBOAT_EXISTING_TOKEN",
            "value": "attempted-overwrite-not-a-real-secret"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "reference_exists");
    assert!(body.get("value").is_none() || body["value"].is_null());

    let issue_resp = client
        .post(format!("{}/api/credentials/issue", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_EXISTING_TOKEN"}))
        .send()
        .await
        .unwrap();
    assert_eq!(issue_resp.status(), 200);
    let issue_body: serde_json::Value = issue_resp.json().await.unwrap();
    assert_eq!(
        issue_body["value"].as_str(),
        Some(EXISTING),
        "the existing value must be unchanged after a refused overwrite"
    );
}

/// Schritt 2 (the core test): a value stored via `/api/credentials/store` is
/// issuable through `/api/credentials/issue` immediately -- same process, no
/// restart -- and the issued value is exactly what was stored.
#[tokio::test]
async fn test_credential_store_then_issue_same_process() {
    const STORED: &str = "stored-value-not-a-real-secret";
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    seed_vault(tmp.path(), &[]);
    let (base_url, state) = boot_credential_server(tmp.path(), &["ROWBOAT_STORED_TOKEN"]).await;
    let client = reqwest::Client::new();

    let store_resp = client
        .post(format!("{}/api/credentials/store", base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_STORED_TOKEN", "value": STORED}))
        .send()
        .await
        .unwrap();

    assert_eq!(store_resp.status(), 200);
    let cache_control = store_resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .expect("store response must carry a cache-control header")
        .to_string();
    assert!(cache_control.contains("no-store"));
    let store_body: serde_json::Value = store_resp.json().await.unwrap();
    assert_eq!(store_body["reference"], "ROWBOAT_STORED_TOKEN");
    assert_eq!(store_body["issuable"], true);
    assert!(
        store_body.get("value").is_none() || store_body["value"].is_null(),
        "the store response must never carry the value"
    );

    let issue_resp = client
        .post(format!("{}/api/credentials/issue", base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_STORED_TOKEN"}))
        .send()
        .await
        .unwrap();
    assert_eq!(issue_resp.status(), 200);
    let issue_body: serde_json::Value = issue_resp.json().await.unwrap();
    assert_eq!(issue_body["value"].as_str(), Some(STORED));

    state.kernel.shutdown();
}

/// Schritt 3 (the restart test): store a value, then boot an entirely new
/// kernel + AppState against the SAME `OPENFANG_HOME` -- simulating a daemon
/// restart -- and confirm issuance still works, with the second boot's
/// `issuable` seed list deliberately empty. Nothing here relies on a process
/// env var carrying the value or the allowlist membership across the
/// "restart": the vault entry comes back by decrypting `vault.enc` fresh off
/// disk, and the allowlist membership comes back by
/// `routes::seed_issuable_credentials` re-reading
/// `issuable_credentials.list` off disk -- both genuine disk round-trips, not
/// something that would pass merely because both "boots" share one OS
/// process.
#[tokio::test]
async fn test_credential_store_survives_restart() {
    const STORED: &str = "restart-value-not-a-real-secret";
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    seed_vault(tmp.path(), &[]);

    let (base_url_1, state_1) = boot_credential_server(tmp.path(), &[]).await;
    let client = reqwest::Client::new();

    let store_resp = client
        .post(format!("{}/api/credentials/store", base_url_1))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_RESTART_TOKEN", "value": STORED}))
        .send()
        .await
        .unwrap();
    assert_eq!(store_resp.status(), 200);

    // "Restart": shut the first kernel down, then boot a second one against
    // the same home_dir. The second boot is NOT told about
    // ROWBOAT_RESTART_TOKEN via `issuable`, so the only way the assertion
    // below can pass is if the store handler's own persistence worked.
    state_1.kernel.shutdown();
    let (base_url_2, state_2) = boot_credential_server(tmp.path(), &[]).await;

    let issue_resp = client
        .post(format!("{}/api/credentials/issue", base_url_2))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_RESTART_TOKEN"}))
        .send()
        .await
        .unwrap();

    assert_eq!(issue_resp.status(), 200);
    let issue_body: serde_json::Value = issue_resp.json().await.unwrap();
    assert_eq!(issue_body["value"].as_str(), Some(STORED));

    state_2.kernel.shutdown();
}
