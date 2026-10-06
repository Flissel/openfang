//! End-to-End: Remote-Integrationen gegen einen lokalen Mini-MCP-Server.
//!
//! Bootet einen echten Kernel (tool-only) mit einem Vorlagen-Ordner und prueft
//! Zulassung, `zustand`/`detail` und dass kein Header-Wert in API-Antworten
//! auftaucht. Alle Schluesselwerte sind erfunden.

use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use openfang_api::routes::{self, AppState};
use openfang_kernel::OpenFangKernel;
use openfang_types::config::KernelConfig;
use std::{sync::Arc, time::Instant};

/// Minimaler MCP-Server (Streamable HTTP, nur JSON-Antworten).
/// `Authorization: Bearer GUT` → bedient `initialize`/`tools/list`, sonst 401.
async fn mini_mcp(headers: HeaderMap, Json(req): Json<serde_json::Value>) -> Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer GUT") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"unauthorized"})),
        )
            .into_response();
    }
    // Notifications (ohne id) werden laut Spezifikation mit 202 ohne Body quittiert.
    let Some(id) = req.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match req["method"].as_str().unwrap_or("") {
        "initialize" => serde_json::json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mini","version":"1"}}),
        "tools/list" => serde_json::json!({"tools":[
            {"name":"get_me","description":"wer bin ich","inputSchema":{"type":"object"}},
            {"name":"create_issue","description":"schreibt","inputSchema":{"type":"object"}}]}),
        _ => serde_json::json!({}),
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})),
    )
        .into_response()
}

async fn start_mini_mcp() -> String {
    let app = Router::new().route("/mcp", post(mini_mcp));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

fn template(url: &str, credential: &str, admission: &str) -> String {
    format!(
        r#"
id = "probe"
name = "Probe"
description = "Testvorlage"
category = "devtools"
read_only_tools = ["get_me"]
[transport]
type = "http"
url = "{url}"
[[auth_headers]]
name = "Authorization"
format = "Bearer {{credential}}"
credential = "{credential}"
[catalog]
admission = "{admission}"
"#
    )
}

struct Harness {
    base: String,
    state: Arc<AppState>,
    _tmp: tempfile::TempDir,
}

async fn harness(template_body: String) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("integrations");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("probe.toml"), template_body).unwrap();
    let mut config = KernelConfig {
        home_dir: tmp.path().to_path_buf(),
        data_dir: tmp.path().join("data"),
        ..KernelConfig::default()
    };
    config.extensions.template_dirs = vec![dir];
    config.runtime.tool_only = true;
    let kernel = Arc::new(OpenFangKernel::boot_with_config(config).unwrap());
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
        .route(
            "/api/integrations",
            axum::routing::get(routes::list_integrations),
        )
        .route("/api/integrations/add", post(routes::add_integration))
        .route(
            "/api/integrations/health",
            axum::routing::get(routes::integrations_health),
        )
        .route("/api/config", axum::routing::get(routes::get_config))
        .with_state(state.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Harness {
        base: format!("http://{addr}"),
        state,
        _tmp: tmp,
    }
}

async fn add(h: &Harness) -> (u16, serde_json::Value) {
    let r = reqwest::Client::new()
        .post(format!("{}/api/integrations/add", h.base))
        .json(&serde_json::json!({"id":"probe"}))
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

async fn body(h: &Harness, path: &str) -> String {
    reqwest::get(format!("{}{path}", h.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn connected_with_vault_key_and_value_never_leaks() {
    // Eindeutiger Kanarienwert als Prozess-Umgebung: der CredentialResolver faellt auf env zurueck.
    std::env::set_var("PROBE_KEY_GUT", "GUT");
    let url = start_mini_mcp().await;
    let h = harness(template(&url, "PROBE_KEY_GUT", "admitted")).await;
    let (status, json) = add(&h).await;
    assert_eq!(status, 201, "{json}");
    assert_eq!(json["zustand"], "verbunden", "{json}");
    for p in [
        "/api/integrations",
        "/api/integrations/health",
        "/api/config",
    ] {
        let b = body(&h, p).await;
        assert!(!b.contains("Bearer GUT"), "{p} leaks header: {b}");
    }
    let list = body(&h, "/api/integrations").await;
    assert!(
        list.contains("PROBE_KEY_GUT"),
        "reference name should be listed: {list}"
    );
    // Freigabe-Standard ueber den echten Kernel:
    assert!(!h.state.kernel.integration_requires_approval("mcp_probe_get_me"));
    assert!(h
        .state
        .kernel
        .integration_requires_approval("mcp_probe_create_issue"));
    std::env::remove_var("PROBE_KEY_GUT");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_key_is_key_rejected() {
    std::env::set_var("PROBE_KEY_FALSCH", "FALSCH");
    let url = start_mini_mcp().await;
    let h = harness(template(&url, "PROBE_KEY_FALSCH", "admitted")).await;
    let (_, json) = add(&h).await;
    assert_eq!(json["zustand"], "schluessel_abgelehnt", "{json}");
    assert!(!body(&h, "/api/integrations/health")
        .await
        .contains("FALSCH\""));
    std::env::remove_var("PROBE_KEY_FALSCH");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_key_is_reported_by_name() {
    let url = start_mini_mcp().await;
    let h = harness(template(&url, "PROBE_KEY_FEHLT_UEBERALL", "admitted")).await;
    let (status, json) = add(&h).await;
    assert_eq!(status, 201);
    assert_eq!(json["zustand"], "fehlt_schluessel", "{json}");
    assert!(json["detail"]
        .as_str()
        .unwrap()
        .contains("PROBE_KEY_FEHLT_UEBERALL"));
}

#[tokio::test(flavor = "multi_thread")]
async fn review_required_is_not_installable() {
    let url = start_mini_mcp().await;
    let h = harness(template(&url, "PROBE_KEY_EGAL", "review_required")).await;
    let (status, json) = add(&h).await;
    assert_eq!(status, 409);
    assert_eq!(json["error"], "integration_not_admitted");
}
