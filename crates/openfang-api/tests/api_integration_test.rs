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
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Start the production router with runtime admission enabled. Unlike the
/// lightweight general-purpose test router, this retains the API-key middleware
/// so authority endpoints exercise the same authenticated boundary as daemon
/// traffic.
async fn start_runtime_admission_test_server(auto_approve: bool) -> TestServer {
    start_runtime_admission_test_server_with_enabled(true, auto_approve).await
}

/// Starts the real runtime-admission test server with a harmless test-only
/// dispatch probe at the real runtime tool-runner boundary.
async fn start_runtime_admission_test_server_with_dispatch_probe(
    auto_approve: bool,
    probe: Arc<RuntimeMcpDispatchProbe>,
) -> TestServer {
    start_runtime_admission_test_server_with_api_key_and_dispatch_probe(
        true,
        auto_approve,
        "runtime-admission-test-key",
        Some(probe),
    )
    .await
}

struct RuntimeMcpDispatchProbe {
    calls: AtomicUsize,
    gate: Option<Arc<tokio::sync::Barrier>>,
}

impl RuntimeMcpDispatchProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            gate: None,
        })
    }

    fn blocked() -> (Arc<Self>, Arc<tokio::sync::Barrier>) {
        let gate = Arc::new(tokio::sync::Barrier::new(2));
        (
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                gate: Some(gate.clone()),
            }),
            gate,
        )
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn observer(self: &Arc<Self>) -> routes::RuntimeMcpDispatchObserver {
        let probe = Arc::clone(self);
        Arc::new(move || {
            let probe = Arc::clone(&probe);
            Box::pin(async move {
                probe.calls.fetch_add(1, Ordering::SeqCst);
                if let Some(gate) = probe.gate.as_ref() {
                    gate.wait().await;
                }
            })
        })
    }

    async fn wait_for_call(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if self.calls() > 0 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dispatch probe should observe one invocation");
    }
}

async fn start_runtime_admission_test_server_with_enabled(
    runtime_admission_enabled: bool,
    auto_approve: bool,
) -> TestServer {
    start_runtime_admission_test_server_with_api_key(
        runtime_admission_enabled,
        auto_approve,
        "runtime-admission-test-key",
    )
    .await
}

async fn start_runtime_admission_test_server_with_api_key(
    runtime_admission_enabled: bool,
    auto_approve: bool,
    api_key: &str,
) -> TestServer {
    start_runtime_admission_test_server_with_api_key_and_dispatch_probe(
        runtime_admission_enabled,
        auto_approve,
        api_key,
        None,
    )
    .await
}

async fn start_runtime_admission_test_server_with_api_key_and_dispatch_probe(
    runtime_admission_enabled: bool,
    auto_approve: bool,
    api_key: &str,
    dispatch_probe: Option<Arc<RuntimeMcpDispatchProbe>>,
) -> TestServer {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    let mut config = KernelConfig {
        home_dir: tmp.path().to_path_buf(),
        data_dir: tmp.path().join("data"),
        api_key: api_key.to_string(),
        default_model: DefaultModelConfig {
            provider: "ollama".to_string(),
            model: "test-model".to_string(),
            api_key_env: "OLLAMA_API_KEY".to_string(),
            base_url: None,
            subprocess_timeout_secs: None,
        },
        ..KernelConfig::default()
    };
    config.runtime_admission.enabled = runtime_admission_enabled;
    config.approval.auto_approve = auto_approve;

    let kernel = Arc::new(OpenFangKernel::boot_with_config(config).expect("Kernel should boot"));
    kernel.set_self_handle();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind test server");
    let addr = listener.local_addr().expect("test listener address");
    let (app, state) = match dispatch_probe {
        Some(probe) => {
            openfang_api::server::build_router_with_runtime_mcp_dispatch_observer(
                kernel,
                addr,
                probe.observer(),
            )
            .await
        }
        None => openfang_api::server::build_router(kernel, addr).await,
    };

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("test server should serve");
    });

    TestServer {
        base_url: format!("http://{addr}"),
        state,
        _tmp: tmp,
    }
}

async fn spawn_runtime_test_agent(server: &TestServer) -> String {
    spawn_runtime_test_agent_with_manifest(server, TEST_MANIFEST).await
}

async fn spawn_runtime_test_agent_with_manifest(server: &TestServer, manifest: &str) -> String {
    let response = runtime_authorized_request(
        &reqwest::Client::new(),
        reqwest::Method::POST,
        format!("{}/api/agents", server.base_url),
    )
    .json(&serde_json::json!({"manifest_toml": manifest}))
    .send()
    .await
    .expect("spawn runtime test agent");
    assert_eq!(response.status(), 201);
    response
        .json::<serde_json::Value>()
        .await
        .expect("spawn response JSON")["agent_id"]
        .as_str()
        .expect("spawned agent id")
        .to_string()
}

fn runtime_authorized_request(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
) -> reqwest::RequestBuilder {
    client
        .request(method, url)
        .bearer_auth("runtime-admission-test-key")
}

fn runtime_approval_request(agent_id: &str) -> serde_json::Value {
    serde_json::json!({
        "contract_version": "v1",
        "correlation_id": "correlation-runtime-admission",
        "plan_id": "plan-runtime-admission",
        "plan_revision": 1,
        "space_id": "space-runtime-admission",
        "agent_id": agent_id,
        "max_plan_cost_microusd": 0,
        "ttl_seconds": 60
    })
}

async fn runtime_approved_refs(
    server: &TestServer,
    client: &reqwest::Client,
    agent_id: &str,
    invocation_id: &str,
) -> (String, String) {
    let approval = runtime_authorized_request(
        client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/admit", server.base_url),
    )
    .json(&runtime_approval_request(agent_id))
    .send()
    .await
    .expect("approval route responds");
    assert_eq!(approval.status(), 200);
    let approval: serde_json::Value = approval.json().await.expect("approval JSON");
    let approval_ref = approval["approval_ref"]
        .as_str()
        .expect("approved response supplies approval ref")
        .to_string();

    let reservation = runtime_authorized_request(
        client,
        reqwest::Method::POST,
        format!("{}/api/runtime/cost-reservations", server.base_url),
    )
    .json(&serde_json::json!({
        "contract_version": "v1",
        "correlation_id": "correlation-runtime-admission",
        "plan_id": "plan-runtime-admission",
        "plan_revision": 1,
        "space_id": "space-runtime-admission",
        "agent_id": agent_id,
        "approval_ref": approval_ref,
        "invocation_id": invocation_id,
        "max_cost_microusd": 0,
        "ttl_seconds": 60
    }))
    .send()
    .await
    .expect("cost route responds");
    assert_eq!(reservation.status(), 200);
    let reservation: serde_json::Value = reservation.json().await.expect("reservation JSON");
    let cost_ref = reservation["reservation"]["cost_ref"]
        .as_str()
        .expect("reservation supplies cost ref")
        .to_string();
    (approval_ref, cost_ref)
}

fn runtime_mcp_call(
    client: &reqwest::Client,
    server: &TestServer,
    agent_id: &str,
    invocation_id: serde_json::Value,
    approval_ref: Option<&str>,
    cost_ref: Option<&str>,
    tool_name: &str,
    arguments: serde_json::Value,
) -> reqwest::RequestBuilder {
    let mut request = runtime_authorized_request(
        client,
        reqwest::Method::POST,
        format!("{}/mcp", server.base_url),
    )
    .header("X-OpenFang-Agent-Id", agent_id);
    if let Some(approval_ref) = approval_ref {
        request = request.header("X-OpenFang-Approval-Ref", approval_ref);
    }
    if let Some(cost_ref) = cost_ref {
        request = request.header("X-OpenFang-Cost-Ref", cost_ref);
    }
    request.json(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": invocation_id,
        "method": "tools/call",
        "params": {"name": tool_name, "arguments": arguments}
    }))
}

fn runtime_authority_row_counts(server: &TestServer) -> (i64, i64) {
    let connection = server.state.kernel.memory.usage_conn();
    let connection = connection.lock().expect("runtime authority database lock");
    let cost_rows = connection
        .query_row("SELECT COUNT(*) FROM cost_reservations", [], |row| {
            row.get(0)
        })
        .expect("count cost rows");
    let receipt_rows = connection
        .query_row("SELECT COUNT(*) FROM execution_receipts", [], |row| {
            row.get(0)
        })
        .expect("count receipt rows");
    (cost_rows, receipt_rows)
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
            subprocess_timeout_secs: None,
        },
        ..KernelConfig::default()
    };

    let kernel = OpenFangKernel::boot_with_config(config).expect("Kernel should boot");
    let kernel = Arc::new(kernel);
    kernel.set_self_handle();

    let state = Arc::new(AppState {
        kernel,
        started_at: Instant::now(),
        peer_registry: None,
        bridge_manager: tokio::sync::Mutex::new(None),
        channels_config: tokio::sync::RwLock::new(Default::default()),
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        clawhub_cache: dashmap::DashMap::new(),
        provider_probe_cache: openfang_runtime::provider_health::ProbeCache::new(),
        budget_config: Arc::new(tokio::sync::RwLock::new(Default::default())),
    });

    let app = Router::new()
        .route("/api/health", axum::routing::get(routes::health))
        .route("/api/status", axum::routing::get(routes::status))
        .route(
            "/v1/embeddings",
            axum::routing::post(openfang_api::openai_compat::embeddings),
        )
        .route("/mcp", axum::routing::post(routes::mcp_http))
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
            "/api/agents/{id}/clone",
            axum::routing::post(routes::clone_agent),
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
        .route("/api/commands", axum::routing::get(routes::list_commands))
        .route(
            "/api/schedules",
            axum::routing::get(routes::list_schedules).post(routes::create_schedule),
        )
        .route(
            "/api/schedules/{id}",
            axum::routing::delete(routes::delete_schedule).put(routes::update_schedule),
        )
        .route(
            "/api/schedules/{id}/delivery-log",
            axum::routing::get(routes::schedule_delivery_log),
        )
        .route(
            "/api/cron/jobs",
            axum::routing::get(routes::list_cron_jobs).post(routes::create_cron_job),
        )
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

const RUNTIME_MULTI_TOOL_MANIFEST: &str = r#"
name = "runtime-test-agent"
version = "0.1.0"
description = "Runtime admission integration test agent"
author = "test"
module = "builtin:chat"

[model]
provider = "ollama"
model = "test-model"
system_prompt = "You are a test agent. Reply concisely."

[capabilities]
tools = ["file_read", "file_list"]
memory_read = ["*"]
memory_write = ["self.*"]
"#;

const RUNTIME_AGENT_LIST_MANIFEST: &str = r#"
name = "runtime-agent-list-test-agent"
version = "0.1.0"
description = "Runtime admission replay envelope integration test agent"
author = "test"
module = "builtin:chat"

[model]
provider = "ollama"
model = "test-model"
system_prompt = "You are a test agent. Reply concisely."

[capabilities]
tools = ["agent_find"]
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
async fn test_embeddings_rejects_unconfigured_non_openai_provider() {
    let server = start_test_server().await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/embeddings", server.base_url))
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": "fail closed"
        }))
        .send()
        .await
        .expect("the local test server should respond");

    assert_eq!(response.status(), 503);
    let body: serde_json::Value = response.json().await.expect("error body should be JSON");
    assert_eq!(body["error"]["type"], "service_unavailable_error");
    assert_eq!(body["error"]["code"], "embedding_provider_not_configured");
}

/// MCP tool execution must not accept an agent identity from the JSON-RPC
/// body. The caller identity is an authenticated transport concern and is
/// required before the route reaches the tool runner.
#[tokio::test]
async fn test_mcp_tools_call_rejects_body_claimed_caller_without_bound_header() {
    let server = start_test_server().await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "file_read",
                "arguments": {"path": "ignored"},
                "caller_agent_id": "00000000-0000-0000-0000-000000000000"
            }
        }))
        .send()
        .await
        .expect("the local test server should respond");

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("response is JSON");
    assert_eq!(body["error"]["code"], -32001);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("X-OpenFang-Agent-Id"));
}

/// A client that has reached the MCP route still cannot claim an unregistered
/// caller identity. The route must resolve the ID against the kernel registry.
#[tokio::test]
async fn test_mcp_tools_call_rejects_unknown_bound_caller() {
    let server = start_test_server().await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .header(
            "X-OpenFang-Agent-Id",
            "00000000-0000-0000-0000-000000000000",
        )
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "file_read", "arguments": {"path": "ignored"}}
        }))
        .send()
        .await
        .expect("the local test server should respond");

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("response is JSON");
    assert_eq!(body["error"]["code"], -32002);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("not registered"));
}

/// A registered caller receives only the tool set resolved from its manifest;
/// a globally-known tool outside that scope must fail closed.
#[tokio::test]
async fn test_mcp_tools_call_rejects_tool_outside_caller_scope() {
    let server = start_test_server().await;
    let caller_id = spawn_test_agent(&server).await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .header("X-OpenFang-Agent-Id", caller_id)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "file_write", "arguments": {"path": "ignored", "content": "x"}}
        }))
        .send()
        .await
        .expect("the local test server should respond");

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("response is JSON");
    assert_eq!(body["error"]["code"], -32602);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("not permitted"));
}

/// Tool discovery carries the same authority as execution: a client must not
/// discover the global catalog merely by omitting the caller binding.
#[tokio::test]
async fn test_mcp_tools_list_rejects_missing_caller_binding() {
    let server = start_test_server().await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        }))
        .send()
        .await
        .expect("the local test server should respond");

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("response is JSON");
    assert_eq!(body["error"]["code"], -32001);
}

/// Tool discovery is a projection of the caller's effective allowlist, not a
/// global catalog. The test agent is limited to `file_read` by its manifest.
#[tokio::test]
async fn test_mcp_tools_list_filters_catalog_to_caller_scope() {
    let server = start_test_server().await;
    let caller_id = spawn_test_agent(&server).await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .header("X-OpenFang-Agent-Id", caller_id)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        }))
        .send()
        .await
        .expect("the local test server should respond");

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("response is JSON");
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("tools/list result has tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"file_read"));
    assert!(!names.contains(&"file_write"));
}

#[tokio::test]
async fn runtime_control_plane_empty_configured_key_is_denied_on_loopback() {
    let server = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        start_runtime_admission_test_server_with_api_key(true, true, ""),
    )
    .await
    .expect("runtime test server startup timed out");
    let client = reqwest::Client::new();
    let caller_id = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        spawn_runtime_test_agent(&server),
    )
    .await
    .expect("registered agent spawn timed out");

    let approval = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client
            .post(format!("{}/api/runtime/approvals/admit", server.base_url))
            .json(&runtime_approval_request(&caller_id))
            .send(),
    )
    .await
    .expect("runtime approval request timed out")
    .expect("runtime approval route responds");
    assert_eq!(approval.status(), 401);
}

#[tokio::test]
async fn registered_agent_header_without_control_plane_key_is_denied() {
    let server = start_runtime_admission_test_server_with_api_key(true, true, "").await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;

    let response = client
        .post(format!("{}/mcp", server.base_url))
        .header("X-OpenFang-Agent-Id", caller_id)
        .header("X-OpenFang-Approval-Ref", "not-authority")
        .header("X-OpenFang-Cost-Ref", "not-authority")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "spoofed-agent-header",
            "method": "tools/call",
            "params": {"name": "file_read", "arguments": {"path": "Cargo.toml"}}
        }))
        .send()
        .await
        .expect("MCP route responds");
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn runtime_admission_mcp_protocol_methods_preserve_global_api_auth() {
    let server = start_runtime_admission_test_server(true).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;

    for (method, params, requires_caller) in [
        (
            "initialize",
            serde_json::json!({"protocolVersion": "2024-11-05"}),
            false,
        ),
        ("tools/list", serde_json::json!({}), true),
    ] {
        for credential in [None, Some("wrong-control-plane-key")] {
            let request = client.post(format!("{}/mcp", server.base_url));
            let request = if let Some(credential) = credential {
                request.bearer_auth(credential)
            } else {
                request
            };
            let request = if requires_caller {
                request.header("X-OpenFang-Agent-Id", &caller_id)
            } else {
                request
            };
            let response = request
                .json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": format!("{method}-missing-or-wrong-global-key"),
                    "method": method,
                    "params": params,
                }))
                .send()
                .await
                .expect("MCP protocol route responds");
            assert_eq!(response.status(), 401, "{method} must retain global auth");
        }

        let request = client
            .post(format!("{}/mcp", server.base_url))
            .bearer_auth("runtime-admission-test-key");
        let request = if requires_caller {
            request.header("X-OpenFang-Agent-Id", &caller_id)
        } else {
            request
        };
        let response = request
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": format!("{method}-valid-global-key"),
                "method": method,
                "params": params,
            }))
            .send()
            .await
            .expect("MCP protocol route responds with valid global API key");
        assert_eq!(
            response.status(),
            200,
            "{method} accepts the valid global key"
        );
        let body: serde_json::Value = response.json().await.expect("MCP protocol JSON");
        if method == "initialize" {
            assert_eq!(body["result"]["serverInfo"]["name"], "openfang");
        } else {
            assert!(body["result"]["tools"]
                .as_array()
                .expect("tools/list exposes the caller-scoped catalog")
                .iter()
                .any(|tool| tool["name"] == "file_read"));
        }
    }
}

#[tokio::test]
async fn runtime_control_plane_missing_or_wrong_key_denied_before_registered_agent_authority() {
    let server = start_runtime_admission_test_server(true).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;
    for credential in [None, Some("wrong-control-plane-key")] {
        let mut request = client
            .post(format!("{}/mcp", server.base_url))
            .header("X-OpenFang-Agent-Id", &caller_id)
            .header("X-OpenFang-Approval-Ref", "not-authority")
            .header("X-OpenFang-Cost-Ref", "not-authority");
        if let Some(credential) = credential {
            request = request.bearer_auth(credential);
        }
        let response = request
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": "missing-or-wrong-key",
                "method": "tools/call",
                "params": {"name": "file_read", "arguments": {"path": "Cargo.toml"}}
            }))
            .send()
            .await
            .expect("MCP route responds");
        assert_eq!(response.status(), 401);
    }
}

#[tokio::test]
async fn runtime_control_plane_valid_api_key_allows_registered_agent_authority() {
    let server = start_runtime_admission_test_server(true).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;
    let response = runtime_authorized_request(
        &client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/admit", server.base_url),
    )
    .json(&runtime_approval_request(&caller_id))
    .send()
    .await
    .expect("runtime approval route responds");
    assert_eq!(response.status(), 200);

    let x_api_key = client
        .post(format!("{}/api/runtime/approvals/admit", server.base_url))
        .header("X-API-Key", "runtime-admission-test-key")
        .json(&runtime_approval_request(&caller_id))
        .send()
        .await
        .expect("runtime approval route responds");
    assert_eq!(x_api_key.status(), 200);
}

#[tokio::test]
async fn runtime_admission_rejects_body_authority_and_non_string_invocation_ids() {
    let probe = RuntimeMcpDispatchProbe::new();
    let server = start_runtime_admission_test_server_with_dispatch_probe(true, probe.clone()).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;

    let body_authority = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!("body-refs-only"),
        None,
        None,
        "file_read",
        serde_json::json!({
            "path": "Cargo.toml",
            "approval_ref": "body-approval",
            "cost_ref": "body-cost"
        }),
    )
    .send()
    .await
    .expect("MCP route responds")
    .json::<serde_json::Value>()
    .await
    .expect("MCP response JSON");
    assert_eq!(
        body_authority["error"]["data"]["authority_status"],
        "denied"
    );
    assert!(body_authority.get("result").is_none());

    for invalid_id in [
        serde_json::json!(7),
        serde_json::json!("  "),
        serde_json::Value::Null,
    ] {
        let body = runtime_mcp_call(
            &client,
            &server,
            &caller_id,
            invalid_id,
            Some("approval-header"),
            Some("cost-header"),
            "file_read",
            serde_json::json!({"path": "Cargo.toml"}),
        )
        .send()
        .await
        .expect("MCP route responds")
        .json::<serde_json::Value>()
        .await
        .expect("MCP response JSON");
        assert_eq!(body["error"]["data"]["authority_status"], "denied");
    }
    assert_eq!(probe.calls(), 0, "denied calls must not dispatch");
}

#[tokio::test]
async fn runtime_admission_pending_approval_never_creates_a_receipt_or_dispatches() {
    let probe = RuntimeMcpDispatchProbe::new();
    let server =
        start_runtime_admission_test_server_with_dispatch_probe(false, probe.clone()).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;

    let pending = runtime_authorized_request(
        &client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/admit", server.base_url),
    )
    .json(&runtime_approval_request(&caller_id))
    .send()
    .await
    .expect("approval route responds");
    assert_eq!(pending.status(), 200);
    let pending: serde_json::Value = pending.json().await.expect("pending JSON");
    let approval_ref = pending["approval_ref"]
        .as_str()
        .expect("pending approval ref");

    let response = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!("pending-invocation"),
        Some(approval_ref),
        Some("unusable-cost-ref"),
        "file_read",
        serde_json::json!({"path": "Cargo.toml"}),
    )
    .send()
    .await
    .expect("MCP route responds");
    let body: serde_json::Value = response.json().await.expect("MCP JSON");
    assert_eq!(
        body["error"]["data"]["authority_status"],
        "pending_approval"
    );

    let receipt = runtime_authorized_request(
        &client,
        reqwest::Method::GET,
        format!(
            "{}/api/runtime/receipts/pending-invocation",
            server.base_url
        ),
    )
    .send()
    .await
    .expect("receipt route responds");
    assert_eq!(receipt.status(), 404);
    assert_eq!(runtime_authority_row_counts(&server), (0, 0));
    assert_eq!(probe.calls(), 0, "pending approval must not dispatch");
}

#[tokio::test]
async fn runtime_admission_replays_exact_string_invocation_without_a_second_dispatch() {
    let probe = RuntimeMcpDispatchProbe::new();
    let server = start_runtime_admission_test_server_with_dispatch_probe(true, probe.clone()).await;
    let client = reqwest::Client::new();
    let caller_id =
        spawn_runtime_test_agent_with_manifest(&server, RUNTIME_AGENT_LIST_MANIFEST).await;
    let invocation_id = "replay-exact-string-id";
    let (approval_ref, cost_ref) =
        runtime_approved_refs(&server, &client, &caller_id, invocation_id).await;

    let first = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!(invocation_id),
        Some(&approval_ref),
        Some(&cost_ref),
        "agent_find",
        serde_json::json!({"query": "runtime-agent-list-test-agent"}),
    )
    .send()
    .await
    .expect("first MCP route responds")
    .json::<serde_json::Value>()
    .await
    .expect("first MCP JSON");
    assert_eq!(
        first["result"]["_meta"]["runtime_admission"]["replayed"],
        false
    );

    let replay = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!(invocation_id),
        Some(&approval_ref),
        Some(&cost_ref),
        "agent_find",
        serde_json::json!({"query": "runtime-agent-list-test-agent"}),
    )
    .send()
    .await
    .expect("replay MCP route responds")
    .json::<serde_json::Value>()
    .await
    .expect("replay MCP JSON");
    assert_eq!(
        replay["result"]["_meta"]["runtime_admission"]["replayed"],
        true
    );
    let content = replay["result"]["content"]
        .as_array()
        .expect("redacted envelope replay content is an array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
    let envelope = content[0]["text"]
        .as_str()
        .expect("replayed redacted envelope is text");
    assert!(serde_json::from_str::<serde_json::Value>(envelope)
        .expect("replayed redacted envelope remains valid JSON")
        .is_array());
    assert!(
        replay["result"]["_meta"]["runtime_admission"]["receipt"]["result_sha256"]
            .as_str()
            .is_some()
    );
    assert_eq!(
        replay["result"]["_meta"]["runtime_admission"]["receipt"]["result_storage_mode"],
        "redacted_envelope"
    );

    let receipt = runtime_authorized_request(
        &client,
        reqwest::Method::GET,
        format!("{}/api/runtime/receipts/{invocation_id}", server.base_url),
    )
    .send()
    .await
    .expect("receipt route responds");
    assert_eq!(receipt.status(), 200);
    let receipt: serde_json::Value = receipt.json().await.expect("receipt JSON");
    assert_eq!(receipt["invocation_id"], invocation_id);
    assert_eq!(receipt["status"], "succeeded");
    assert_eq!(
        probe.calls(),
        1,
        "success and replay must dispatch exactly once"
    );
}

#[tokio::test]
async fn runtime_admission_digest_only_replay_exposes_hash_metadata_without_content() {
    let probe = RuntimeMcpDispatchProbe::new();
    let server = start_runtime_admission_test_server_with_dispatch_probe(true, probe.clone()).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;
    let invocation_id = "digest-only-replay-invocation";
    let (approval_ref, cost_ref) =
        runtime_approved_refs(&server, &client, &caller_id, invocation_id).await;

    let first = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!(invocation_id),
        Some(&approval_ref),
        Some(&cost_ref),
        "file_read",
        serde_json::json!({"path": "Cargo.toml"}),
    )
    .send()
    .await
    .expect("first MCP route responds")
    .json::<serde_json::Value>()
    .await
    .expect("first MCP JSON");
    assert_eq!(first["result"]["isError"], false);

    let replay = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!(invocation_id),
        Some(&approval_ref),
        Some(&cost_ref),
        "file_read",
        serde_json::json!({"path": "Cargo.toml"}),
    )
    .send()
    .await
    .expect("replay MCP route responds")
    .json::<serde_json::Value>()
    .await
    .expect("replay MCP JSON");
    assert_eq!(
        replay["result"]["_meta"]["runtime_admission"]["replayed"],
        true
    );
    assert!(replay["result"]["content"]
        .as_array()
        .expect("digest-only replay content is an array")
        .is_empty());
    assert_eq!(
        replay["result"]["_meta"]["runtime_admission"]["receipt"]["result_storage_mode"],
        "digest_only"
    );
    assert!(
        replay["result"]["_meta"]["runtime_admission"]["receipt"]["result_sha256"]
            .as_str()
            .is_some()
    );
    assert_eq!(probe.calls(), 1, "digest-only replay must not redispatch");
}

#[tokio::test]
async fn runtime_admission_failed_tool_replay_preserves_error_without_second_dispatch() {
    let probe = RuntimeMcpDispatchProbe::new();
    let server = start_runtime_admission_test_server_with_dispatch_probe(true, probe.clone()).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;
    let invocation_id = "failed-replay-invocation";
    let arguments = serde_json::json!({"path": "definitely-missing-runtime-admission-file"});
    let (approval_ref, cost_ref) =
        runtime_approved_refs(&server, &client, &caller_id, invocation_id).await;

    let first = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!(invocation_id),
        Some(&approval_ref),
        Some(&cost_ref),
        "file_read",
        arguments.clone(),
    )
    .send()
    .await
    .expect("failed tool call responds")
    .json::<serde_json::Value>()
    .await
    .expect("failed tool JSON");
    assert_eq!(first["result"]["isError"], true);
    assert_eq!(
        first["result"]["_meta"]["runtime_admission"]["replayed"],
        false
    );

    let replay = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!(invocation_id),
        Some(&approval_ref),
        Some(&cost_ref),
        "file_read",
        arguments,
    )
    .send()
    .await
    .expect("failed replay responds")
    .json::<serde_json::Value>()
    .await
    .expect("failed replay JSON");
    assert_eq!(replay["result"]["isError"], true);
    assert_eq!(
        replay["result"]["_meta"]["runtime_admission"]["replayed"],
        true
    );
    assert_eq!(
        replay["result"]["_meta"]["runtime_admission"]["receipt"]["status"],
        "failed"
    );

    let receipt = runtime_authorized_request(
        &client,
        reqwest::Method::GET,
        format!("{}/api/runtime/receipts/{invocation_id}", server.base_url),
    )
    .send()
    .await
    .expect("receipt route responds")
    .json::<serde_json::Value>()
    .await
    .expect("receipt JSON");
    assert_eq!(receipt["status"], "failed");
    assert_eq!(
        probe.calls(),
        1,
        "failed call and replay must dispatch exactly once"
    );
}

#[tokio::test]
async fn runtime_admission_routes_require_auth_reject_unknown_fields_and_fail_closed_when_disabled()
{
    let disabled = start_runtime_admission_test_server_with_enabled(false, true).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&disabled).await;
    let request = runtime_approval_request(&caller_id);

    let unauthenticated = client
        .post(format!("{}/api/runtime/approvals/admit", disabled.base_url))
        .json(&request)
        .send()
        .await
        .expect("route responds");
    assert_eq!(unauthenticated.status(), 401);

    let mut unknown = request.clone();
    unknown["unexpected"] = serde_json::json!(true);
    let unknown = runtime_authorized_request(
        &client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/admit", disabled.base_url),
    )
    .json(&unknown)
    .send()
    .await
    .expect("route responds");
    assert_eq!(unknown.status(), 400);

    let closed = runtime_authorized_request(
        &client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/admit", disabled.base_url),
    )
    .json(&request)
    .send()
    .await
    .expect("route responds");
    assert_eq!(closed.status(), 503);

    let invalid_decision = runtime_authorized_request(
        &client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/resolve", disabled.base_url),
    )
    .json(&serde_json::json!({"approval_ref": "approval-test", "decision": "unknown"}))
    .send()
    .await
    .expect("resolve route responds");
    assert_eq!(invalid_decision.status(), 400);

    let legacy = runtime_mcp_call(
        &client,
        &disabled,
        &caller_id,
        serde_json::json!("legacy-disabled-admission"),
        None,
        None,
        "file_read",
        serde_json::json!({"path": "Cargo.toml"}),
    )
    .send()
    .await
    .expect("legacy MCP route responds")
    .json::<serde_json::Value>()
    .await
    .expect("legacy MCP JSON");
    assert!(legacy["result"]["content"].is_array());
    assert!(legacy["result"]["_meta"].is_null());
}

#[tokio::test]
async fn runtime_admission_disabled_legacy_mcp_tools_call_preserves_global_api_auth() {
    let server = start_runtime_admission_test_server_with_enabled(false, true).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;

    for credential in [None, Some("wrong-control-plane-key")] {
        let request = client
            .post(format!("{}/mcp", server.base_url))
            .header("X-OpenFang-Agent-Id", &caller_id);
        let request = if let Some(credential) = credential {
            request.bearer_auth(credential)
        } else {
            request
        };
        let response = request
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": "legacy-missing-or-wrong-global-key",
                "method": "tools/call",
                "params": {"name": "file_read", "arguments": {"path": "Cargo.toml"}}
            }))
            .send()
            .await
            .expect("legacy MCP route responds");
        assert_eq!(
            response.status(),
            401,
            "legacy tools/call retains global auth"
        );
    }

    let valid = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!("legacy-valid-global-key"),
        None,
        None,
        "file_read",
        serde_json::json!({"path": "Cargo.toml"}),
    )
    .send()
    .await
    .expect("legacy MCP route responds with valid global key");
    assert_eq!(valid.status(), 200);
    let valid: serde_json::Value = valid.json().await.expect("legacy MCP JSON");
    assert!(valid["result"]["content"].is_array());
    assert!(valid["result"]["_meta"].is_null());
}

#[tokio::test]
async fn runtime_authority_invalid_contract_is_400_without_retry_hint() {
    let server = start_runtime_admission_test_server(true).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;
    let mut request = runtime_approval_request(&caller_id);
    request["correlation_id"] = serde_json::json!("   ");

    let response = runtime_authorized_request(
        &client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/admit", server.base_url),
    )
    .json(&request)
    .send()
    .await
    .expect("runtime approval route responds");
    assert_eq!(response.status(), 400);
    assert!(!response.headers().contains_key("retry-after"));
    let body: serde_json::Value = response.json().await.expect("invalid request JSON");
    assert_eq!(body["error"], "invalid_runtime_authority_request");
    assert!(body.get("retry_after").is_none());
}

#[tokio::test]
async fn runtime_authority_unknown_expired_and_already_resolved_are_stable_conflicts() {
    let server = start_runtime_admission_test_server(false).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;

    let unknown = runtime_authorized_request(
        &client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/resolve", server.base_url),
    )
    .json(&serde_json::json!({
        "approval_ref": "approval-unknown",
        "decision": "approved"
    }))
    .send()
    .await
    .expect("unknown resolve responds");
    assert_eq!(unknown.status(), 409);
    assert_eq!(
        unknown
            .json::<serde_json::Value>()
            .await
            .expect("unknown conflict JSON")["error"],
        "runtime_authority_conflict"
    );

    let pending = runtime_authorized_request(
        &client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/admit", server.base_url),
    )
    .json(&runtime_approval_request(&caller_id))
    .send()
    .await
    .expect("pending approval responds")
    .json::<serde_json::Value>()
    .await
    .expect("pending approval JSON");
    let approval_ref = pending["approval_ref"]
        .as_str()
        .expect("pending approval ref");
    for expected_status in [200, 409] {
        let response = runtime_authorized_request(
            &client,
            reqwest::Method::POST,
            format!("{}/api/runtime/approvals/resolve", server.base_url),
        )
        .json(&serde_json::json!({
            "approval_ref": approval_ref,
            "decision": "approved"
        }))
        .send()
        .await
        .expect("resolve responds");
        assert_eq!(response.status().as_u16(), expected_status);
        if expected_status == 409 {
            assert_eq!(
                response
                    .json::<serde_json::Value>()
                    .await
                    .expect("already resolved conflict JSON")["error"],
                "runtime_authority_conflict"
            );
        }
    }

    let mut expiring_request = runtime_approval_request(&caller_id);
    expiring_request["correlation_id"] = serde_json::json!("correlation-expiring");
    expiring_request["plan_id"] = serde_json::json!("plan-expiring");
    expiring_request["ttl_seconds"] = serde_json::json!(1);
    let expiring = runtime_authorized_request(
        &client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/admit", server.base_url),
    )
    .json(&expiring_request)
    .send()
    .await
    .expect("expiring approval responds")
    .json::<serde_json::Value>()
    .await
    .expect("expiring approval JSON");
    let expiring_ref = expiring["approval_ref"]
        .as_str()
        .expect("expiring approval ref");
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let expired = runtime_authorized_request(
        &client,
        reqwest::Method::POST,
        format!("{}/api/runtime/approvals/resolve", server.base_url),
    )
    .json(&serde_json::json!({
        "approval_ref": expiring_ref,
        "decision": "approved"
    }))
    .send()
    .await
    .expect("expired resolve responds");
    assert_eq!(expired.status(), 409);
    assert_eq!(
        expired
            .json::<serde_json::Value>()
            .await
            .expect("expired conflict JSON")["error"],
        "runtime_authority_conflict"
    );
}

#[tokio::test]
async fn runtime_admission_rejects_changed_bindings_without_replacing_the_first_receipt() {
    let server = start_runtime_admission_test_server(true).await;
    let client = reqwest::Client::new();
    let caller_id =
        spawn_runtime_test_agent_with_manifest(&server, RUNTIME_MULTI_TOOL_MANIFEST).await;
    let other_caller_id = spawn_runtime_test_agent(&server).await;
    let invocation_id = "binding-guard-invocation";
    let (approval_ref, cost_ref) =
        runtime_approved_refs(&server, &client, &caller_id, invocation_id).await;

    let first = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!(invocation_id),
        Some(&approval_ref),
        Some(&cost_ref),
        "file_read",
        serde_json::json!({"path": "Cargo.toml"}),
    )
    .send()
    .await
    .expect("first call responds");
    assert_eq!(first.status(), 200);

    for (agent_id, tool_name, arguments) in [
        (
            caller_id.as_str(),
            "file_read",
            serde_json::json!({"path": "crates/openfang-api/Cargo.toml"}),
        ),
        (
            caller_id.as_str(),
            "file_list",
            serde_json::json!({"path": "."}),
        ),
        (
            other_caller_id.as_str(),
            "file_read",
            serde_json::json!({"path": "Cargo.toml"}),
        ),
    ] {
        let body = runtime_mcp_call(
            &client,
            &server,
            agent_id,
            serde_json::json!(invocation_id),
            Some(&approval_ref),
            Some(&cost_ref),
            tool_name,
            arguments,
        )
        .send()
        .await
        .expect("changed binding route responds")
        .json::<serde_json::Value>()
        .await
        .expect("changed binding JSON");
        assert_eq!(body["error"]["data"]["authority_status"], "denied");
    }

    let receipt = runtime_authorized_request(
        &client,
        reqwest::Method::GET,
        format!("{}/api/runtime/receipts/{invocation_id}", server.base_url),
    )
    .send()
    .await
    .expect("receipt route responds")
    .json::<serde_json::Value>()
    .await
    .expect("receipt JSON");
    assert_eq!(receipt["agent_id"], caller_id);
    assert_eq!(receipt["tool_name"], "file_read");
    assert_eq!(
        receipt["arguments_sha256"],
        openfang_runtime::result_receipt::canonical_json_sha256(&serde_json::json!({
            "path": "Cargo.toml"
        }))
        .expect("digest")
    );
}

#[tokio::test]
async fn runtime_admission_concurrent_duplicate_has_one_durable_dispatch_receipt() {
    let (probe, dispatch_gate) = RuntimeMcpDispatchProbe::blocked();
    let server = start_runtime_admission_test_server_with_dispatch_probe(true, probe.clone()).await;
    let client = reqwest::Client::new();
    let caller_id = spawn_runtime_test_agent(&server).await;
    let invocation_id = "concurrent-duplicate-invocation";
    let (approval_ref, cost_ref) =
        runtime_approved_refs(&server, &client, &caller_id, invocation_id).await;

    let first = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!(invocation_id),
        Some(&approval_ref),
        Some(&cost_ref),
        "file_read",
        serde_json::json!({"path": "Cargo.toml"}),
    )
    .send();
    let first = tokio::spawn(first);
    probe.wait_for_call().await;
    let second = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!(invocation_id),
        Some(&approval_ref),
        Some(&cost_ref),
        "file_read",
        serde_json::json!({"path": "Cargo.toml"}),
    )
    .send();
    let second = tokio::spawn(second);
    tokio::task::yield_now().await;
    dispatch_gate.wait().await;
    let (first, second) = tokio::join!(first, second);
    let bodies = [
        first
            .expect("first concurrent task")
            .expect("first concurrent response")
            .json::<serde_json::Value>()
            .await
            .expect("first concurrent JSON"),
        second
            .expect("second concurrent task")
            .expect("second concurrent response")
            .json::<serde_json::Value>()
            .await
            .expect("second concurrent JSON"),
    ];
    let completed = bodies
        .iter()
        .filter(|body| body["result"]["_meta"]["runtime_admission"]["replayed"] == false)
        .count();
    let replay_or_progress = bodies
        .iter()
        .filter(|body| {
            body["result"]["_meta"]["runtime_admission"]["replayed"] == true
                || body["error"]["data"]["authority_status"] == "in_progress"
        })
        .count();
    assert_eq!(completed, 1, "only one request may dispatch");
    assert_eq!(
        replay_or_progress, 1,
        "duplicate returns replay or in-progress evidence"
    );

    let receipt = runtime_authorized_request(
        &client,
        reqwest::Method::GET,
        format!("{}/api/runtime/receipts/{invocation_id}", server.base_url),
    )
    .send()
    .await
    .expect("receipt route responds")
    .json::<serde_json::Value>()
    .await
    .expect("receipt JSON");
    assert_eq!(receipt["invocation_id"], invocation_id);
    assert_eq!(receipt["status"], "succeeded");
    assert_eq!(
        probe.calls(),
        1,
        "concurrent duplicates must dispatch exactly once"
    );
}

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

/// Regression test for issue #1026: GET /api/agents returns `is_inferencing`
/// reflecting whether the agent has an in-flight LLM task. This drives the
/// live dashboard indicator that shows which agents are calling the LLM.
#[tokio::test]
async fn test_list_agents_includes_inferencing_flag() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Spawn a test agent.
    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": TEST_MANIFEST}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let agent_id_str = body["agent_id"].as_str().unwrap().to_string();
    let agent_id: openfang_types::agent::AgentId = agent_id_str.parse().unwrap();

    // Baseline: idle agent must report is_inferencing = false.
    let resp = client
        .get(format!("{}/api/agents", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let agents: Vec<serde_json::Value> = resp.json().await.unwrap();
    let test_agent = agents
        .iter()
        .find(|a| a["id"] == agent_id_str)
        .expect("spawned agent should appear in list");
    assert_eq!(
        test_agent["is_inferencing"], false,
        "freshly spawned agent should not be inferencing"
    );

    // Simulate an in-flight LLM call by inserting a real AbortHandle into
    // the kernel's running_tasks map. This is exactly what the agent loop
    // does when it starts processing a message.
    let handle = tokio::spawn(async {
        // Long-lived task we will abort at end of test.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    });
    server
        .state
        .kernel
        .running_tasks
        .insert(agent_id, handle.abort_handle());

    // Now list_agents should report is_inferencing = true for that agent.
    let resp = client
        .get(format!("{}/api/agents", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let agents: Vec<serde_json::Value> = resp.json().await.unwrap();
    let test_agent = agents
        .iter()
        .find(|a| a["id"] == agent_id_str)
        .expect("spawned agent should still appear in list");
    assert_eq!(
        test_agent["is_inferencing"], true,
        "agent with an entry in running_tasks must be flagged is_inferencing"
    );

    // Other agents (the default assistant) must NOT be flagged.
    if let Some(other) = agents.iter().find(|a| a["id"] != agent_id_str) {
        assert_eq!(
            other["is_inferencing"], false,
            "agents without a running task must not be flagged"
        );
    }

    // Cleanup so the spawned future does not outlive the test.
    server.state.kernel.running_tasks.remove(&agent_id);
    handle.abort();
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

/// Regression test for #935: the GET /api/agents/:id/session endpoint
/// must NOT expose internal system-prompt messages to the Web UI.
///
/// We construct a session containing a System message + a User message + an
/// Assistant message, persist it via the kernel's memory store, then call the
/// HTTP endpoint and assert:
///   1. The default response excludes the system message entirely.
///   2. `message_count` reflects only the visible (user + assistant) messages.
///   3. `raw_message_count` exposes the underlying total.
///   4. With `?include_system=true`, the system message IS returned (debug
///      mode opt-in).
#[tokio::test]
async fn test_agent_session_filters_system_messages() {
    use openfang_types::message::{Message, Role};

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
    let agent_id_str = body["agent_id"].as_str().unwrap().to_string();

    // Look up the agent's session id and inject a forged history that
    // contains a system-role message (simulating what an OpenAI-compat
    // client could push, or what a future regression might persist).
    let agent_id: openfang_types::agent::AgentId = agent_id_str.parse().unwrap();
    let entry = server.state.kernel.registry.get(agent_id).unwrap();
    let session_id = entry.session_id;
    let mut session = server
        .state
        .kernel
        .memory
        .get_session(session_id)
        .unwrap()
        .expect("session should exist after spawn");

    session.messages = vec![
        Message {
            role: Role::System,
            content: openfang_types::message::MessageContent::Text(
                "INTERNAL SYSTEM PROMPT — must not leak to UI".to_string(),
            ),
            ..Default::default()
        },
        Message::user("hello"),
        Message::assistant("hi there"),
    ];
    server.state.kernel.memory.save_session(&session).unwrap();

    // --- Default request: system message must be filtered out ---
    let resp = client
        .get(format!(
            "{}/api/agents/{}/session",
            server.base_url, agent_id_str
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "should only see user + assistant");
    assert_eq!(body["message_count"], 2);
    assert_eq!(body["raw_message_count"], 3);

    // No message in the response should carry the System role label, and
    // the system prompt text MUST NOT appear anywhere in the payload.
    for m in messages {
        let role = m["role"].as_str().unwrap_or("");
        assert_ne!(role, "System", "system role leaked into UI history");
        assert_ne!(role, "system", "system role leaked into UI history");
    }
    let body_str = serde_json::to_string(&body).unwrap();
    assert!(
        !body_str.contains("INTERNAL SYSTEM PROMPT"),
        "system prompt content leaked into session response: {body_str}"
    );

    // Verify the visible roles are exactly what we expect.
    assert_eq!(messages[0]["role"], "User");
    assert_eq!(messages[0]["content"], "hello");
    assert_eq!(messages[1]["role"], "Assistant");
    assert_eq!(messages[1]["content"], "hi there");

    // --- Opt-in debug mode: ?include_system=true returns it ---
    let resp = client
        .get(format!(
            "{}/api/agents/{}/session?include_system=true",
            server.base_url, agent_id_str
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "include_system=true should return all 3");
    assert_eq!(messages[0]["role"], "System");
    assert_eq!(
        messages[0]["content"],
        "INTERNAL SYSTEM PROMPT — must not leak to UI"
    );
    assert_eq!(body["message_count"], 3);
    assert_eq!(body["raw_message_count"], 3);
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
            subprocess_timeout_secs: None,
        },
        ..KernelConfig::default()
    };

    let kernel = OpenFangKernel::boot_with_config(config).expect("Kernel should boot");
    let kernel = Arc::new(kernel);
    kernel.set_self_handle();

    let state = Arc::new(AppState {
        kernel,
        started_at: Instant::now(),
        peer_registry: None,
        bridge_manager: tokio::sync::Mutex::new(None),
        channels_config: tokio::sync::RwLock::new(Default::default()),
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        clawhub_cache: dashmap::DashMap::new(),
        provider_probe_cache: openfang_runtime::provider_health::ProbeCache::new(),
        budget_config: Arc::new(tokio::sync::RwLock::new(Default::default())),
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
        allow_no_auth: true,
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
            "/api/agents/{id}/clone",
            axum::routing::post(routes::clone_agent),
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
// /api/commands — unified command registry endpoint
// ---------------------------------------------------------------------------

/// Default (no surface query) returns web-surface commands.
#[tokio::test]
async fn test_commands_default_returns_web() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/commands", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["surface"], "web");

    let commands = body["commands"].as_array().expect("commands is array");
    assert!(!commands.is_empty(), "web surface should have commands");

    // Every entry has the documented shape.
    for c in commands {
        assert!(c["name"].is_string());
        assert!(c["aliases"].is_array());
        assert!(c["description"].is_string());
        assert!(c["category"].is_string());
        assert!(c["requires_agent"].is_boolean());
    }

    // Sanity: web surface must include `/help` and `/verbose` and must NOT
    // include CLI-only `/kill`.
    let names: Vec<&str> = commands
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"help"));
    assert!(names.contains(&"verbose"));
    assert!(!names.contains(&"kill"));
}

/// `?surface=cli` returns CLI-only commands and includes the alias array.
#[tokio::test]
async fn test_commands_cli_surface() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/commands?surface=cli", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["surface"], "cli");

    let commands = body["commands"].as_array().unwrap();
    let names: Vec<&str> = commands
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"kill"));
    assert!(names.contains(&"clear"));
    assert!(names.contains(&"exit"));
    // `start` is channel-only — must not appear on CLI.
    assert!(!names.contains(&"start"));

    // `/exit` carries the `quit` alias.
    let exit = commands
        .iter()
        .find(|c| c["name"] == "exit")
        .expect("exit command must be present on CLI");
    let aliases = exit["aliases"].as_array().unwrap();
    assert!(
        aliases.iter().any(|a| a == "quit"),
        "quit alias should be attached to /exit"
    );
}

/// `?surface=all` includes commands from every surface.
#[tokio::test]
async fn test_commands_all_surface() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/commands?surface=all", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["surface"], "all");

    let names: Vec<&str> = body["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();

    // Surface-specific probes: all three unique-per-surface commands appear.
    assert!(names.contains(&"kill"), "CLI-only /kill missing from /all");
    assert!(
        names.contains(&"start"),
        "channel-only /start missing from /all"
    );
    assert!(
        names.contains(&"verbose"),
        "web-only /verbose missing from /all"
    );
}

/// `?surface=channel` returns channel commands only.
#[tokio::test]
async fn test_commands_channel_surface() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/commands?surface=channel", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["surface"], "channel");

    let names: Vec<&str> = body["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"start"));
    // CLI-only must not appear here.
    assert!(!names.contains(&"kill"));
}

/// Unknown surface returns 400 with a JSON error body.
#[tokio::test]
async fn test_commands_invalid_surface_400() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/commands?surface=bogus", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    let err = body["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("bogus"),
        "error should mention the bad value: {err}"
    );
}

// ---------------------------------------------------------------------------
// Schedule delivery_targets round-trip tests
// ---------------------------------------------------------------------------
//
// These exercise the `/api/schedules` and `/api/cron/jobs` endpoints to
// confirm `CronDeliveryTarget` variants round-trip cleanly through create /
// list / update / delivery-log, and that bad input is rejected at the API
// layer rather than silently dropped.

async fn spawn_test_agent(server: &TestServer) -> String {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": TEST_MANIFEST}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    body["agent_id"].as_str().unwrap().to_string()
}

/// POST /api/schedules with all four `CronDeliveryTarget` variants should
/// store them and return them on GET /api/schedules.
#[tokio::test]
async fn test_schedules_delivery_targets_roundtrip() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let agent_id = spawn_test_agent(&server).await;

    let delivery_targets = serde_json::json!([
        { "type": "channel", "channel_type": "telegram", "recipient": "chat_12345" },
        { "type": "webhook", "url": "https://example.com/hook", "auth_header": "Bearer abc" },
        { "type": "local_file", "path": "/tmp/openfang-test.log", "append": true },
        { "type": "email", "to": "alice@example.com", "subject_template": "Cron: {job}" },
    ]);

    let resp = client
        .post(format!("{}/api/schedules", server.base_url))
        .json(&serde_json::json!({
            "name": "multi-destination-test",
            "cron": "0 9 * * 1-5",
            "agent_id": agent_id,
            "message": "Generate the daily brief.",
            "enabled": true,
            "delivery_targets": delivery_targets,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sched_id = body["id"]
        .as_str()
        .expect("created schedule id")
        .to_string();
    let got = body["delivery_targets"]
        .as_array()
        .expect("response must include delivery_targets");
    assert_eq!(got.len(), 4, "all four targets should round-trip");
    assert_eq!(got[0]["type"], "channel");
    assert_eq!(got[0]["channel_type"], "telegram");
    assert_eq!(got[0]["recipient"], "chat_12345");
    assert_eq!(got[1]["type"], "webhook");
    assert_eq!(got[1]["url"], "https://example.com/hook");
    assert_eq!(got[1]["auth_header"], "Bearer abc");
    assert_eq!(got[2]["type"], "local_file");
    assert_eq!(got[2]["append"], true);
    assert_eq!(got[3]["type"], "email");
    assert_eq!(got[3]["subject_template"], "Cron: {job}");

    let resp = client
        .get(format!("{}/api/schedules", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let schedules = body["schedules"].as_array().unwrap();
    let created = schedules
        .iter()
        .find(|s| s["id"] == sched_id)
        .expect("created schedule must appear in list");
    let listed = created["delivery_targets"].as_array().unwrap();
    assert_eq!(listed.len(), 4);
    assert_eq!(listed[0]["channel_type"], "telegram");

    let _ = client
        .delete(format!("{}/api/schedules/{}", server.base_url, sched_id))
        .send()
        .await;
}

/// PUT /api/schedules/{id} with `delivery_targets` should fully replace the
/// target list.
#[tokio::test]
async fn test_schedules_delivery_targets_update() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let agent_id = spawn_test_agent(&server).await;

    let resp = client
        .post(format!("{}/api/schedules", server.base_url))
        .json(&serde_json::json!({
            "name": "update-target-test",
            "cron": "*/15 * * * *",
            "agent_id": agent_id,
            "message": "hi",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sched_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(
        body["delivery_targets"].as_array().map(|a| a.len()),
        Some(0)
    );

    let resp = client
        .put(format!("{}/api/schedules/{}", server.base_url, sched_id))
        .json(&serde_json::json!({
            "delivery_targets": [
                { "type": "webhook", "url": "https://new.example.com/hook" },
                { "type": "local_file", "path": "/tmp/new.log" },
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "updated");
    let echoed = &body["schedule"]["delivery_targets"];
    let arr = echoed
        .as_array()
        .expect("schedule.delivery_targets must be array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["type"], "webhook");
    assert_eq!(arr[1]["type"], "local_file");

    let resp = client
        .get(format!("{}/api/schedules", server.base_url))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let created = body["schedules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == sched_id)
        .unwrap();
    let listed = created["delivery_targets"].as_array().unwrap();
    assert_eq!(listed.len(), 2);

    let resp = client
        .put(format!("{}/api/schedules/{}", server.base_url, sched_id))
        .json(&serde_json::json!({"delivery_targets": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client
        .get(format!("{}/api/schedules", server.base_url))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let created = body["schedules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == sched_id)
        .unwrap();
    let listed = created["delivery_targets"].as_array().unwrap();
    assert_eq!(listed.len(), 0);

    let _ = client
        .delete(format!("{}/api/schedules/{}", server.base_url, sched_id))
        .send()
        .await;
}

/// Malformed `delivery_targets` should return 400, not silently succeed.
#[tokio::test]
async fn test_schedules_rejects_bad_delivery_target() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let agent_id = spawn_test_agent(&server).await;

    let resp = client
        .post(format!("{}/api/schedules", server.base_url))
        .json(&serde_json::json!({
            "name": "bad-target-test",
            "cron": "*/10 * * * *",
            "agent_id": agent_id,
            "message": "hi",
            "delivery_targets": [
                { "type": "channel" /* missing channel_type + recipient */ }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    let err = body["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("delivery_targets"),
        "error should mention delivery_targets, got: {err}"
    );

    let resp = client
        .post(format!("{}/api/schedules", server.base_url))
        .json(&serde_json::json!({
            "name": "bad-array-test",
            "cron": "*/10 * * * *",
            "agent_id": agent_id,
            "message": "hi",
            "delivery_targets": "not-an-array",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// GET /api/schedules/{id}/delivery-log returns the configured targets and an
/// empty entries array for a known schedule, and 404 for a random UUID.
#[tokio::test]
async fn test_schedules_delivery_log_endpoint() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let agent_id = spawn_test_agent(&server).await;

    let resp = client
        .post(format!("{}/api/schedules", server.base_url))
        .json(&serde_json::json!({
            "name": "log-test",
            "cron": "0 * * * *",
            "agent_id": agent_id,
            "message": "x",
            "delivery_targets": [
                { "type": "webhook", "url": "https://example.com/h" }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let sched_id = resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = client
        .get(format!(
            "{}/api/schedules/{}/delivery-log",
            server.base_url, sched_id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["schedule_id"], sched_id);
    let targets = body["targets"].as_array().expect("targets array");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["type"], "webhook");
    let entries = body["entries"].as_array().expect("entries array");
    assert!(
        entries.is_empty(),
        "delivery history is not persisted yet — entries must be empty"
    );

    let random = "550e8400-e29b-41d4-a716-446655440000";
    let resp = client
        .get(format!(
            "{}/api/schedules/{}/delivery-log",
            server.base_url, random
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let resp = client
        .get(format!(
            "{}/api/schedules/not-a-uuid/delivery-log",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let _ = client
        .delete(format!("{}/api/schedules/{}", server.base_url, sched_id))
        .send()
        .await;
}

/// POST /api/cron/jobs with `delivery_targets` should persist them and they
/// should appear on the subsequent GET.
#[tokio::test]
async fn test_cron_jobs_delivery_targets_roundtrip() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let agent_id = spawn_test_agent(&server).await;

    let resp = client
        .post(format!("{}/api/cron/jobs", server.base_url))
        .json(&serde_json::json!({
            "agent_id": agent_id,
            "name": "cron-fanout",
            "schedule": { "kind": "cron", "expr": "*/20 * * * *" },
            "action": { "kind": "agent_turn", "message": "pulse" },
            "delivery": { "kind": "none" },
            "delivery_targets": [
                { "type": "local_file", "path": "/tmp/pulse.log", "append": true },
                { "type": "webhook", "url": "http://example.com/pulse" }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    let resp = client
        .get(format!(
            "{}/api/cron/jobs?agent_id={}",
            server.base_url, agent_id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let jobs = body["jobs"].as_array().unwrap();
    let job = jobs
        .iter()
        .find(|j| j["name"] == "cron-fanout")
        .expect("created job must be listed");
    let targets = job["delivery_targets"].as_array().expect("targets array");
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0]["type"], "local_file");
    assert_eq!(targets[0]["path"], "/tmp/pulse.log");
    assert_eq!(targets[0]["append"], true);
    assert_eq!(targets[1]["type"], "webhook");
    assert_eq!(targets[1]["url"], "http://example.com/pulse");
}

// ---------------------------------------------------------------------------
// Clone agent endpoint tests (issue #868)
// ---------------------------------------------------------------------------

/// Happy path: clone an existing template agent into a new agent with a
/// distinct name. The clone must get a fresh ID, fresh workspace path, and
/// inherit non-name manifest fields from the template.
#[tokio::test]
async fn test_clone_agent_happy_path() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Spawn a template agent.
    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": TEST_MANIFEST}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let template_id = body["agent_id"].as_str().unwrap().to_string();

    // Clone it.
    let resp = client
        .post(format!(
            "{}/api/agents/{}/clone",
            server.base_url, template_id
        ))
        .json(&serde_json::json!({
            "new_name": "cloned-user-1",
            "overrides": {
                "description": "Cloned for user 1",
                "tags": ["clone", "user-1"]
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "clone should succeed");
    let body: serde_json::Value = resp.json().await.unwrap();

    let new_id = body["agent_id"].as_str().unwrap();
    assert_ne!(new_id, template_id, "clone must have a fresh agent ID");
    assert_eq!(body["name"], "cloned-user-1");

    // The full manifest should be returned and reflect the new name + overrides.
    let manifest = &body["manifest"];
    assert!(manifest.is_object(), "manifest must be returned");
    assert_eq!(manifest["name"], "cloned-user-1");
    assert_eq!(manifest["description"], "Cloned for user 1");
    assert_eq!(
        manifest["tags"].as_array().unwrap(),
        &vec![serde_json::json!("clone"), serde_json::json!("user-1"),]
    );
    // Inherited from template — the system_prompt should match.
    assert_eq!(
        manifest["model"]["system_prompt"],
        "You are a test agent. Reply concisely."
    );

    // The agent list should now contain both template and clone.
    let resp = client
        .get(format!("{}/api/agents", server.base_url))
        .send()
        .await
        .unwrap();
    let agents: Vec<serde_json::Value> = resp.json().await.unwrap();
    let names: Vec<&str> = agents.iter().map(|a| a["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"test-agent"));
    assert!(names.contains(&"cloned-user-1"));
}

/// Cloning into a name that's already taken must fail with 409 Conflict.
#[tokio::test]
async fn test_clone_agent_name_collision() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Spawn a template agent named "test-agent".
    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": TEST_MANIFEST}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let template_id = body["agent_id"].as_str().unwrap().to_string();

    // First clone — succeeds.
    let resp = client
        .post(format!(
            "{}/api/agents/{}/clone",
            server.base_url, template_id
        ))
        .json(&serde_json::json!({"new_name": "duplicate-name"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Second clone with the same name — must be rejected.
    let resp = client
        .post(format!(
            "{}/api/agents/{}/clone",
            server.base_url, template_id
        ))
        .json(&serde_json::json!({"new_name": "duplicate-name"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        409,
        "duplicate name must return 409 Conflict"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("already exists"));

    // Cloning into the template's own name must also be rejected.
    let resp = client
        .post(format!(
            "{}/api/agents/{}/clone",
            server.base_url, template_id
        ))
        .json(&serde_json::json!({"new_name": "test-agent"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

/// Cloning a non-existent template must return 404.
#[tokio::test]
async fn test_clone_agent_template_not_found() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Random valid UUID that does not match any agent.
    let bogus_id = "00000000-0000-0000-0000-000000000000";
    let resp = client
        .post(format!("{}/api/agents/{}/clone", server.base_url, bogus_id))
        .json(&serde_json::json!({"new_name": "ghost-clone"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("Template agent not found"));

    // Malformed agent id → 400.
    let resp = client
        .post(format!("{}/api/agents/not-a-uuid/clone", server.base_url))
        .json(&serde_json::json!({"new_name": "ghost-clone"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// Empty new_name must be rejected with 400.
#[tokio::test]
async fn test_clone_agent_empty_name_rejected() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // Spawn a template agent.
    let resp = client
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": TEST_MANIFEST}))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let template_id = body["agent_id"].as_str().unwrap().to_string();

    let resp = client
        .post(format!(
            "{}/api/agents/{}/clone",
            server.base_url, template_id
        ))
        .json(&serde_json::json!({"new_name": "   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}
