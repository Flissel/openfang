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

/// Start the real MCP HTTP route with the model-free runtime profile.  The
/// default model configuration remains default-valued so profile validation
/// permits boot while the kernel never constructs a model runtime.
async fn start_tool_only_test_server() -> TestServer {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    let mut config = KernelConfig {
        home_dir: tmp.path().to_path_buf(),
        data_dir: tmp.path().join("data"),
        ..KernelConfig::default()
    };
    config.runtime.tool_only = true;
    let kernel =
        Arc::new(OpenFangKernel::boot_with_config(config).expect("tool-only kernel boots"));
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
        issuable_credentials: Default::default(),
        store_credential_lock: tokio::sync::Mutex::new(()),
    });
    let app = Router::new()
        .route("/mcp", axum::routing::post(routes::mcp_http))
        .route(
            "/api/agents",
            axum::routing::get(routes::list_agents).post(routes::spawn_agent),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind test server");
    let addr = listener.local_addr().expect("test listener address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("test server should serve");
    });
    TestServer {
        base_url: format!("http://{addr}"),
        state,
        _tmp: tmp,
    }
}

/// Start the production router with both the tool-only profile and runtime
/// admission enabled. The existing lifecycle-owned dispatch observer proves
/// that rejected model tools stop before the real tool runner.
async fn start_tool_only_runtime_admission_test_server_with_dispatch_probe(
    probe: Arc<RuntimeMcpDispatchProbe>,
) -> TestServer {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    let mut config = KernelConfig {
        home_dir: tmp.path().to_path_buf(),
        data_dir: tmp.path().join("data"),
        api_key: "runtime-admission-test-key".to_string(),
        ..KernelConfig::default()
    };
    config.runtime.tool_only = true;
    config.runtime_admission.enabled = true;
    config.approval.auto_approve = true;

    let kernel =
        Arc::new(OpenFangKernel::boot_with_config(config).expect("tool-only kernel boots"));
    kernel.set_self_handle();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind test server");
    let addr = listener.local_addr().expect("test listener address");
    let (app, state) = openfang_api::server::build_router_with_runtime_mcp_dispatch_observer(
        kernel,
        addr,
        probe.observer(),
    )
    .await;
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
        store_credential_lock: tokio::sync::Mutex::new(()),
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

/// Model-backed builtins are not discoverable from an MCP caller connected to
/// a tool-only kernel, even when the caller declares unrestricted tools.
#[tokio::test]
async fn tool_only_mcp_tools_list_excludes_model_inference_tools() {
    let server = start_tool_only_test_server().await;
    let caller_id = spawn_test_agent_with_manifest(
        &server,
        TEST_MANIFEST
            .replace("tools = [\"file_read\"]", "tools = [\"*\"]")
            .as_str(),
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .header("X-OpenFang-Agent-Id", caller_id)
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .send()
        .await
        .expect("MCP route responds");
    let body: serde_json::Value = response.json().await.expect("MCP response JSON");
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("tools/list result")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"file_read"));
    for model_tool in [
        "media_describe",
        "media_transcribe",
        "image_generate",
        "text_to_speech",
        "speech_to_text",
    ] {
        assert!(!names.contains(&model_tool), "{model_tool} must be hidden");
    }
}

/// The same filter rejects an attempted model-tool invocation before it reaches
/// runtime admission or the runtime tool runner.  There is no model fallback.
#[tokio::test]
async fn tool_only_mcp_tools_call_rejects_model_inference_before_dispatch() {
    let probe = RuntimeMcpDispatchProbe::new();
    let server =
        start_tool_only_runtime_admission_test_server_with_dispatch_probe(probe.clone()).await;
    let caller_id = spawn_runtime_test_agent_with_manifest(
        &server,
        TEST_MANIFEST
            .replace("tools = [\"file_read\"]", "tools = [\"*\"]")
            .as_str(),
    )
    .await;

    let client = reqwest::Client::new();

    // Positive control: the lifecycle-owned observer sees an admitted
    // deterministic invocation. If the tool-only filter were removed, the
    // model-tool request below would reach this same observer.
    let (allowed_approval, allowed_cost) =
        runtime_approved_refs(&server, &client, &caller_id, "tool-only-allowed").await;
    let allowed = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!("tool-only-allowed"),
        Some(&allowed_approval),
        Some(&allowed_cost),
        "file_read",
        serde_json::json!({"path": "does-not-exist"}),
    )
    .send()
    .await
    .expect("admitted deterministic MCP request responds");
    assert_eq!(allowed.status(), 200);
    probe.wait_for_call().await;
    assert_eq!(probe.calls(), 1, "positive control reaches tool runner");

    let (rejected_approval, rejected_cost) =
        runtime_approved_refs(&server, &client, &caller_id, "tool-only-model").await;
    let before = runtime_authority_row_counts(&server);
    let response = runtime_mcp_call(
        &client,
        &server,
        &caller_id,
        serde_json::json!("tool-only-model"),
        Some(&rejected_approval),
        Some(&rejected_cost),
        "media_describe",
        serde_json::json!({"path": "never-read.png"}),
    )
    .send()
    .await
    .expect("MCP route responds");
    let body: serde_json::Value = response.json().await.expect("MCP response JSON");
    assert_eq!(
        runtime_authority_row_counts(&server),
        before,
        "model-tool rejection must not enter runtime admission"
    );
    assert_eq!(
        probe.calls(),
        1,
        "model-tool rejection must not enter the runtime tool runner"
    );
    assert_eq!(body["error"]["code"], -32602);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("not permitted"));
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
        store_credential_lock: tokio::sync::Mutex::new(()),
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

async fn spawn_test_agent_with_manifest(server: &TestServer, manifest: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{}/api/agents", server.base_url))
        .json(&serde_json::json!({"manifest_toml": manifest}))
        .send()
        .await
        .expect("spawn test agent");
    assert_eq!(response.status(), 201);
    response
        .json::<serde_json::Value>()
        .await
        .expect("spawn response JSON")["agent_id"]
        .as_str()
        .expect("spawned agent id")
        .to_string()
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

// ---------------------------------------------------------------------------
// Credential issuance tests -- POST /api/credentials/issue
// ---------------------------------------------------------------------------

/// Bearer token every credential-issuance test server is configured with.
const CRED_API_KEY: &str = "cred-test-key-123";
/// Eigener Schluessel fuer `/api/credentials/issue` (OPENFANG_ISSUE_KEY).
const CRED_ISSUE_KEY: &str = "cred-issue-key-456";

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
    start_credential_test_server_with_keys(api_key, CRED_ISSUE_KEY, issuable, credentials).await
}

/// Same, with the daemon's issue key (OPENFANG_ISSUE_KEY) under the test's
/// control too. `""` reproduces a daemon booted without one: `/issue` is off.
async fn start_credential_test_server_with_keys(
    api_key: &str,
    issue_key: &str,
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
        issue_key: issue_key.to_string(),
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
        store_credential_lock: tokio::sync::Mutex::new(()),
    });

    let api_key = state.kernel.config.api_key.trim().to_string();
    let auth_state = middleware::AuthState {
        api_key: api_key.clone(),
        auth_enabled: state.kernel.config.auth.enabled,
        session_secret: api_key.clone(),
        // Test harness only. This router is not built with
        // `into_make_service_with_connect_info`, so `middleware::auth` cannot
        // observe that the caller is loopback and would fail closed with 401
        // on the empty-api_key case. Opting out here is what lets the request
        // REACH `issue_credential`, so its own fail-open refusal (404) is the
        // thing actually asserted. Mainline's start_test_server_with_auth sets
        // this the same way. Production is unaffected: server.rs derives it
        // from OPENFANG_ALLOW_NO_AUTH, which defaults to false.
        allow_no_auth: true,
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_SECRET_NOT_ALLOWED"}))
        .send()
        .await
        .unwrap();

    // Allowlisted, but unresolvable.
    let unresolvable = client
        .post(format!("{}/api/credentials/issue", server.base_url))
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
            .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
            .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
// The other tests need a REAL, decryptable vault, because the store handler
// refuses (503) when there is nowhere durable to put the value.
//
// Earlier revisions of this test infra relied on `OPENFANG_VAULT_KEY` and let
// `OpenFangKernel::boot_with_config`'s own automatic vault-unlock pick it up
// -- that is this codebase's documented headless/CI escape hatch (see the
// module doc on `crates/openfang-extensions/src/vault.rs`). It turned out to
// be unsound on a real dev machine: `CredentialVault::unlock()`'s key
// resolution (`resolve_master_key` in vault.rs) checks the OS-keyring-file
// fallback (`dirs::data_local_dir()/openfang/.keyring`, a MACHINE-GLOBAL path
// with nothing to do with this test's own `OPENFANG_HOME`) BEFORE the env
// var. On a machine where an unrelated OpenFang process has ever run
// `vault init` for real, that file exists and silently wins, so the kernel's
// automatic unlock attempts to decrypt this test's vault with the WRONG key
// and gives up, leaving `vault: None` -- exactly the ambient
// multi-session-on-one-machine hazard this repo's own coordination rules
// warn about, and not something a test may fix by touching that file.
//
// `attach_test_vault` below sidesteps the whole mechanism: it boots the
// kernel with no `vault.enc` present (so the flaky automatic unlock never
// fires), then reaches into the already-public
// `kernel.credential_resolver: Mutex<CredentialResolver>` and swaps in a
// resolver built around a vault unlocked with an explicit, hardcoded test
// key -- deterministic on any machine, CI included, and not a new escape
// hatch invented for this test: `CredentialVault::init_with_key` /
// `unlock_with_key` are the vault module's own documented
// "for testing / programmatic use" API.

/// Fixed 32-byte vault master key used only by these tests, passed directly
/// to `init_with_key`/`unlock_with_key` -- never resolved via env var or OS
/// keyring, so it can't collide with, or be shadowed by, either.
const TEST_VAULT_KEY: [u8; 32] = [7u8; 32];

/// Replace `kernel`'s credential resolver with one backed by a real vault at
/// `<home>/vault.enc`, unlocked (or, on a first call for this `home`,
/// created) with `TEST_VAULT_KEY`. Must run once, right after boot, before
/// any credential lookup.
///
/// If `vault.enc` already exists (a second call against a `home` a previous
/// call already used -- the restart test's whole point), this unlocks the
/// SAME on-disk file rather than creating a new one, so a value written in
/// an earlier call is still a genuine decrypt of bytes that were actually on
/// disk, not something carried over in memory.
fn attach_test_vault(kernel: &OpenFangKernel, home: &std::path::Path) {
    let vault_path = home.join("vault.enc");
    let mut vault = openfang_extensions::vault::CredentialVault::new(vault_path.clone());
    if vault_path.exists() {
        vault
            .unlock_with_key(zeroize::Zeroizing::new(TEST_VAULT_KEY))
            .expect("test vault should unlock with its own explicit key");
    } else {
        vault
            .init_with_key(zeroize::Zeroizing::new(TEST_VAULT_KEY))
            .expect("test vault should init with an explicit key");
    }

    let dotenv_path = home.join(".env");
    let resolver =
        openfang_extensions::credentials::CredentialResolver::new(Some(vault), Some(&dotenv_path));
    *kernel
        .credential_resolver
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = resolver;
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
    boot_credential_server_with_key(home, CRED_API_KEY, issuable).await
}

/// Same as `boot_credential_server`, but with the daemon's `api_key` under
/// the caller's control -- `""` reproduces the fail-open configuration
/// (`middleware::auth` waves every request through unauthenticated), the
/// same trick `start_credential_test_server_with_key` uses for the
/// non-vault-backed tests.
async fn boot_credential_server_with_key(
    home: &std::path::Path,
    api_key: &str,
    issuable: &[&str],
) -> (String, Arc<AppState>) {
    let config = KernelConfig {
        home_dir: home.to_path_buf(),
        data_dir: home.join("data"),
        api_key: api_key.to_string(),
        issue_key: CRED_ISSUE_KEY.to_string(),
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
    // No `vault.enc` exists yet on a first call for this `home`, so the
    // kernel's own automatic vault-unlock is a no-op here -- `vault: None`
    // until `attach_test_vault` swaps in a resolver we control below. On a
    // restart-simulating second call, `vault.enc` DOES already exist, but
    // the kernel's automatic unlock would try to decrypt it via
    // `resolve_master_key()` (env var / OS keyring) rather than our explicit
    // test key, so we replace the resolver unconditionally either way.
    attach_test_vault(&kernel, home);
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
        store_credential_lock: tokio::sync::Mutex::new(()),
    });

    let api_key = state.kernel.config.api_key.trim().to_string();
    let auth_state = middleware::AuthState {
        api_key: api_key.clone(),
        auth_enabled: state.kernel.config.auth.enabled,
        session_secret: api_key.clone(),
        allow_no_auth: false,
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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
        .header("x-openfang-issue-key", CRED_ISSUE_KEY)
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

/// Review finding (Critical): `/store` needs the SAME fail-open guard
/// `issue_credential` has -- see that function's SECURITY comment for the
/// full threat model, and `test_credential_issue_refused_when_daemon_has_no_auth`
/// for the sibling test this one mirrors.
///
/// Deliberately does NOT use a vault-backed server: the reference is seeded
/// via `.env` instead (resolvable, exactly like the sibling `/issue` test
/// does). That way, if the guard were missing, the request would proceed
/// past it and hit the ORDINARY `409 reference_exists` conflict check
/// (`overwrite` defaults to `false`) -- a different, distinguishable outcome
/// from the guard's own `503`. Comparing against the identical call on a
/// normally authenticated daemon (which DOES reach that 409) is what proves
/// the `503` above comes from the auth guard specifically, not from the
/// reference being unresolvable or some other unrelated cause.
#[tokio::test]
async fn test_credential_store_refused_when_daemon_has_no_auth() {
    const SEEDED: &str = "must-not-be-touched-not-a-real-secret";
    const ATTEMPT: &str = "attempted-overwrite-not-a-real-secret";

    // Fail-open daemon: empty api_key, and the reference already resolves --
    // so only the guard can be what refuses this with a `503` instead of the
    // ordinary `409` a resolvable reference would otherwise hit.
    let open = start_credential_test_server_with_key(
        "",
        &[],
        &[("ROWBOAT_STORE_FAILOPEN_TOKEN", SEEDED)],
    )
    .await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/api/credentials/store", open.base_url))
        .json(&serde_json::json!({
            "reference": "ROWBOAT_STORE_FAILOPEN_TOKEN",
            "value": ATTEMPT
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 503);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "store_unavailable");

    // Same call, same seed, against an ordinarily authenticated daemon: the
    // guard doesn't apply, so this reaches the real conflict check instead --
    // proving the 503 above was the guard, not an unrelated cause.
    let guarded = start_credential_test_server(
        &[],
        &[("ROWBOAT_STORE_FAILOPEN_TOKEN", SEEDED)],
    )
    .await;
    let ordinary = client
        .post(format!("{}/api/credentials/store", guarded.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({
            "reference": "ROWBOAT_STORE_FAILOPEN_TOKEN",
            "value": ATTEMPT
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ordinary.status(), 409);
    let ordinary_body: serde_json::Value = ordinary.json().await.unwrap();
    assert_eq!(ordinary_body["error"], "reference_exists");
}

/// Review finding (Minor): the `503 store_unavailable` branch was previously
/// untested. `start_credential_test_server` boots an ordinary daemon with no
/// `vault.enc` at all (unlike the vault-seeded helper the tests above use).
#[tokio::test]
async fn test_credential_store_returns_store_unavailable_without_vault() {
    let server = start_credential_test_server(&["ROWBOAT_NOVAULT_TOKEN"], &[]).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/api/credentials/store", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({
            "reference": "ROWBOAT_NOVAULT_TOKEN",
            "value": "irrelevant-not-a-real-secret"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 503);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "store_unavailable");
}

/// Review finding (Important #3, TOCTOU): two concurrent `/store` calls for
/// the same NOT-yet-existing reference must not both succeed. Before
/// `state.store_credential_lock` serialized the check-then-write-then-persist
/// sequence, `resolve_credential` and `store_credential_checked` each took
/// the resolver mutex separately, so both calls could observe "nothing there
/// yet", both write, and both answer `200` -- making the `409
/// reference_exists` guard a TOCTOU no-op under real concurrency. With the
/// lock, the outcome is deterministic regardless of scheduling: exactly one
/// call wins.
/// ZWEI DINGE MACHEN DIESEN TEST FEHLSCHLAGFAEHIG, beide tragend. Ein
/// blankes `#[tokio::test]` laeuft auf einer CURRENT-THREAD-Laufzeit, und das
/// Fenster zwischen Existenzpruefung und Schreiben enthaelt kein `.await` --
/// ein Handler laeuft also immer ganz durch, bevor der andere beginnt, und
/// der Test bestuende identisch mit geloeschtem Lock. Er verlangt deshalb
/// eine mehrfaedige Laufzeit, damit beide Handler wirklich gleichzeitig
/// laufen. Auch dann kann eine einzelne Runde das Fenster durch Glueck
/// verfehlen, also rennt er mehrfach mit je frischer Referenz.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_credential_store_concurrent_calls_race_to_one_winner() {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    let (base_url, state) = boot_credential_server(tmp.path(), &[]).await;

    let store_url = format!("{}/api/credentials/store", base_url);
    let client = reqwest::Client::new();

    for round in 0..12 {
        let reference = format!("ROWBOAT_RACE_TOKEN_{round}");

        let (url_a, client_a, reference_a) =
            (store_url.clone(), client.clone(), reference.clone());
        let task_a = tokio::spawn(async move {
            client_a
                .post(url_a)
                .bearer_auth(CRED_API_KEY)
                .json(&serde_json::json!({
                    "reference": reference_a,
                    "value": "race-value-a-not-a-real-secret"
                }))
                .send()
                .await
                .unwrap()
        });
        let (url_b, client_b, reference_b) =
            (store_url.clone(), client.clone(), reference.clone());
        let task_b = tokio::spawn(async move {
            client_b
                .post(url_b)
                .bearer_auth(CRED_API_KEY)
                .json(&serde_json::json!({
                    "reference": reference_b,
                    "value": "race-value-b-not-a-real-secret"
                }))
                .send()
                .await
                .unwrap()
        });

        let (resp_a, resp_b) = tokio::join!(task_a, task_b);
        let resp_a = resp_a.expect("task a should not panic");
        let resp_b = resp_b.expect("task b should not panic");

        let statuses = [resp_a.status().as_u16(), resp_b.status().as_u16()];
        let winners = statuses.iter().filter(|&&s| s == 200).count();
        let conflicts = statuses.iter().filter(|&&s| s == 409).count();
        assert_eq!(
            (winners, conflicts),
            (1, 1),
            "round {round}: exactly one concurrent /store call for one \
             reference must win (200), the other must see 409; got \
             {statuses:?}"
        );
    }

    state.kernel.shutdown();
}


// ---------------------------------------------------------------------------
// Eigener Issue-Key (2026-10-06): der allgemeine OPENFANG_API_KEY steht in der
// Root-.env, lesbar fuer jeden lokalen Prozess und jeden Agenten mit
// Dateiwerkzeugen. Wer ihn hatte, durfte jeden freigegebenen Schluessel im
// Klartext abholen. Jetzt braucht /issue zusaetzlich einen eigenen Key, und
// ohne konfigurierten Issue-Key ist /issue aus.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_credential_issue_refuses_general_key_without_issue_key() {
    let server =
        start_credential_test_server(&["ROWBOAT_TEST_TOKEN"], &[("ROWBOAT_TEST_TOKEN", "seeded")])
            .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/api/credentials/issue", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .json(&serde_json::json!({"reference": "ROWBOAT_TEST_TOKEN"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "credential_unavailable");
    assert!(body["value"].is_null());
}

#[tokio::test]
async fn test_credential_issue_refuses_wrong_issue_key() {
    let server =
        start_credential_test_server(&["ROWBOAT_TEST_TOKEN"], &[("ROWBOAT_TEST_TOKEN", "seeded")])
            .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/api/credentials/issue", server.base_url))
        .bearer_auth(CRED_API_KEY)
        .header("x-openfang-issue-key", "falsch")
        .json(&serde_json::json!({"reference": "ROWBOAT_TEST_TOKEN"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "credential_unavailable");
}

#[tokio::test]
async fn test_credential_issue_is_off_without_configured_issue_key() {
    let server = start_credential_test_server_with_keys(
        CRED_API_KEY,
        "",
        &["ROWBOAT_TEST_TOKEN"],
        &[("ROWBOAT_TEST_TOKEN", "seeded")],
    )
    .await;
    for presented in ["", CRED_ISSUE_KEY] {
        let resp = reqwest::Client::new()
            .post(format!("{}/api/credentials/issue", server.base_url))
            .bearer_auth(CRED_API_KEY)
            .header("x-openfang-issue-key", presented)
            .json(&serde_json::json!({"reference": "ROWBOAT_TEST_TOKEN"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "presented={presented:?}");
    }
}

#[test]
fn test_issue_key_never_leaves_the_config() {
    let config = KernelConfig {
        issue_key: "ganz-geheimer-issue-key".to_string(),
        ..KernelConfig::default()
    };
    let json = serde_json::to_string(&config).unwrap();
    assert!(!json.contains("ganz-geheimer-issue-key"), "issue_key darf nie serialisiert werden");
    let dbg = format!("{config:?}");
    assert!(!dbg.contains("ganz-geheimer-issue-key"), "issue_key darf nie im Debug-Text stehen");
    // Und aus einer Konfigurationsdatei laesst er sich nicht setzen.
    let from_file: KernelConfig =
        toml::from_str("issue_key = \"aus-der-datei\"").unwrap_or_default();
    assert!(from_file.issue_key.is_empty(), "issue_key darf nie aus einer Datei kommen");
}
