//! Ende-zu-Ende: OAuth-Anmeldung ueber die HTTP-API, Einmal-Schluesselseite,
//! Loopback-Sperre der Browser-Routen und die `INTEGRATION_`-Sperre von
//! `/api/credentials/*`.
//!
//! Echter Kernel + echter Router aus `server.rs` (mit Middleware und
//! `ConnectInfo`), dazu ein lokaler Test-Anmeldeserver mit Test-MCP-Server.
//! Alle Werte sind erfundene Kanarien (`KANARIE-…`).
#![cfg(feature = "test-insecure-oauth")]

use axum::{
    body::Body,
    extract::{ConnectInfo, Form, Query},
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post as rpost},
    Json, Router,
};
use openfang_api::routes::{self, AppState};
use openfang_kernel::OpenFangKernel;
use openfang_types::config::KernelConfig;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const API_KEY: &str = "oauth-test-api-key";
const ISSUE_KEY: &str = "oauth-test-issue-key";

// ---------------------------------------------------------------------------
// Test-Anmeldeserver + Test-MCP-Server
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counters {
    code: AtomicUsize,
    refresh: AtomicUsize,
    revoke: AtomicUsize,
    /// Schalter: MCP akzeptiert nur noch `KANARIE-AT-2`.
    only_at2: AtomicBool,
}

struct Mock {
    base: String,
    c: Arc<Counters>,
}

fn mock_mcp(base: &str, c: &Counters, headers: HeaderMap, req: serde_json::Value) -> Response {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let accepted = if c.only_at2.load(Ordering::SeqCst) {
        auth == "Bearer KANARIE-AT-2"
    } else {
        auth.starts_with("Bearer KANARIE-AT-") || auth == "Bearer KANARIE-STATIC-9"
    };
    if !accepted {
        let mut h = HeaderMap::new();
        h.insert(
            "www-authenticate",
            format!("Bearer error=\"invalid_token\", resource_metadata=\"{base}/.well-known/oauth-protected-resource\"")
                .parse()
                .unwrap(),
        );
        return (StatusCode::UNAUTHORIZED, h).into_response();
    }
    let Some(id) = req.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match req["method"].as_str().unwrap_or("") {
        "initialize" => {
            serde_json::json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mini","version":"1"}})
        }
        "tools/list" => serde_json::json!({"tools":[
            {"name":"get_me","description":"wer bin ich","inputSchema":{"type":"object"}}]}),
        "tools/call" => {
            serde_json::json!({"content":[{"type":"text","text":"probe-ok"}],"isError":false})
        }
        _ => serde_json::json!({}),
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})),
    )
        .into_response()
}

fn mock_token(c: &Counters, f: HashMap<String, String>) -> Response {
    match f.get("grant_type").map(String::as_str) {
        Some("authorization_code")
            if f.get("code").map(String::as_str) == Some("CODE-OK")
                && f.contains_key("code_verifier") =>
        {
            c.code.fetch_add(1, Ordering::SeqCst);
            Json(serde_json::json!({"access_token":"KANARIE-AT-1","refresh_token":"KANARIE-RT-1","expires_in":3600,"token_type":"Bearer"}))
                .into_response()
        }
        Some("refresh_token") => {
            let n = c.refresh.fetch_add(1, Ordering::SeqCst) + 1;
            if f.get("refresh_token") != Some(&format!("KANARIE-RT-{n}")) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"invalid_grant"})),
                )
                    .into_response();
            }
            Json(
                serde_json::json!({"access_token": format!("KANARIE-AT-{}", n + 1),
                "refresh_token": format!("KANARIE-RT-{}", n + 1), "expires_in": 3600}),
            )
            .into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid_request"})),
        )
            .into_response(),
    }
}

async fn start_mock() -> Mock {
    let c = Arc::new(Counters::default());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let (b1, b2, b3, b4) = (base.clone(), base.clone(), base.clone(), base.clone());
    let (c1, c2, c3) = (c.clone(), c.clone(), c.clone());
    let app = Router::new()
        .route(
            "/mcp",
            rpost(move |headers: HeaderMap, Json(req): Json<serde_json::Value>| {
                let (b, c) = (b1.clone(), c1.clone());
                async move { mock_mcp(&b, &c, headers, req) }
            }),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(move || {
                let b = b2.clone();
                async move {
                    Json(serde_json::json!({"resource": format!("{b}/mcp"), "authorization_servers": [b]}))
                }
            }),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(move || {
                let b = b3.clone();
                async move {
                    Json(serde_json::json!({"issuer": b, "authorization_endpoint": format!("{b}/authorize"),
                      "token_endpoint": format!("{b}/token"), "registration_endpoint": format!("{b}/register"),
                      "revocation_endpoint": format!("{b}/revoke"), "code_challenge_methods_supported": ["S256"],
                      "authorization_response_iss_parameter_supported": true}))
                }
            }),
        )
        // "Browser-Login": leitet sofort mit code=CODE-OK an redirect_uri weiter.
        .route(
            "/authorize",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let b = b4.clone();
                async move {
                    let mut to = reqwest::Url::parse(&q["redirect_uri"]).unwrap();
                    to.query_pairs_mut()
                        .append_pair("code", "CODE-OK")
                        .append_pair("state", &q["state"])
                        .append_pair("iss", &b);
                    (StatusCode::FOUND, [("location", to.to_string())]).into_response()
                }
            }),
        )
        .route(
            "/register",
            rpost(|Json(_b): Json<serde_json::Value>| async move {
                (
                    StatusCode::CREATED,
                    Json(serde_json::json!({"client_id": "KANARIE-CLIENT"})),
                )
            }),
        )
        .route(
            "/token",
            rpost(move |Form(f): Form<HashMap<String, String>>| {
                let c = c2.clone();
                async move { mock_token(&c, f) }
            }),
        )
        .route(
            "/revoke",
            rpost(move || {
                let c = c3.clone();
                async move {
                    c.revoke.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Mock { base, c }
}

// ---------------------------------------------------------------------------
// Harness: echter Kernel + echter Router (server.rs) mit ConnectInfo
// ---------------------------------------------------------------------------

struct H {
    base: String,
    state: Arc<AppState>,
    app: Router,
    _tmp: tempfile::TempDir,
}

impl Drop for H {
    fn drop(&mut self) {
        self.state.kernel.shutdown();
    }
}

fn oauth_template(mcp_base: &str) -> String {
    format!(
        r#"
id = "probe"
name = "Probe"
description = "Testvorlage"
category = "devtools"
read_only_tools = ["get_me"]
[transport]
type = "http"
url = "{mcp_base}/mcp"
[auth]
type = "oauth"
scopes = ["offline_access"]
[catalog]
admission = "admitted"
"#
    )
}

fn static_template(mcp_base: &str, credential: &str) -> String {
    format!(
        r#"
id = "probe"
name = "Probe"
description = "Testvorlage"
category = "devtools"
read_only_tools = ["get_me"]
[transport]
type = "http"
url = "{mcp_base}/mcp"
[[auth_headers]]
name = "Authorization"
format = "Bearer {{credential}}"
credential = "{credential}"
[catalog]
admission = "admitted"
"#
    )
}

struct Opts {
    template: Option<String>,
    with_vault: bool,
    /// Vor dem Boot in `<home>` zu schreibende Dateien.
    files: Vec<(&'static str, String)>,
}

async fn harness(opts: Opts) -> H {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("integrations");
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(t) = &opts.template {
        std::fs::write(dir.join("probe.toml"), t).unwrap();
    }
    for (name, content) in &opts.files {
        std::fs::write(tmp.path().join(name), content).unwrap();
    }
    // Router-Adresse VOR dem Boot binden: die Rueckruf-URL haengt an api_listen.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = KernelConfig {
        home_dir: tmp.path().to_path_buf(),
        data_dir: tmp.path().join("data"),
        api_listen: addr.to_string(),
        api_key: API_KEY.to_string(),
        issue_key: ISSUE_KEY.to_string(),
        ..KernelConfig::default()
    };
    config.extensions.template_dirs = vec![dir];
    config.extensions.oauth_allow_loopback_http = true;
    config.runtime.tool_only = true;
    let kernel = Arc::new(OpenFangKernel::boot_with_config(config).unwrap());
    kernel.set_self_handle();
    if opts.with_vault {
        let mut v = openfang_extensions::vault::CredentialVault::new(tmp.path().join("vault.enc"));
        v.init_with_key(zeroize::Zeroizing::new([7u8; 32])).unwrap();
        *kernel.credential_resolver.lock().unwrap() =
            openfang_extensions::credentials::CredentialResolver::new(Some(v), None);
    }
    let (app, state) = openfang_api::server::build_router(kernel, addr).await;
    let served = app.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            served.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    H {
        base: format!("http://{addr}"),
        state,
        app,
        _tmp: tmp,
    }
}

async fn harness_with_oauth_probe(mock: &Mock) -> H {
    harness(Opts {
        template: Some(oauth_template(&mock.base)),
        with_vault: true,
        files: vec![],
    })
    .await
}

async fn post(h: &H, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let r = reqwest::Client::new()
        .post(format!("{}{path}", h.base))
        .bearer_auth(API_KEY)
        .json(&body)
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    let t = r.text().await.unwrap();
    (
        s,
        serde_json::from_str(&t).unwrap_or(serde_json::Value::String(t)),
    )
}

async fn get_text(h: &H, path: &str) -> (u16, String) {
    let r = reqwest::Client::new()
        .get(format!("{}{path}", h.base))
        .bearer_auth(API_KEY)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

async fn get_json(h: &H, path: &str) -> serde_json::Value {
    serde_json::from_str(&get_text(h, path).await.1).unwrap()
}

fn zustand_of(list: &serde_json::Value) -> String {
    list["installed"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "probe")
        .unwrap()["zustand"]
        .as_str()
        .unwrap()
        .to_string()
}

fn url_param(url: &str, name: &str) -> String {
    reqwest::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| panic!("param {name} fehlt"))
}

/// "Browser": GET authorize -> 302 -> GET callback (Loopback, ohne Bearer).
async fn follow_login_redirect(url: &str) -> (u16, String) {
    let r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(2))
        .build()
        .unwrap()
        .get(url)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

fn callback_path_from(url: &str, code: &str, iss: &str) -> String {
    let mut u = reqwest::Url::parse("http://x/api/integrations/probe/oauth/callback").unwrap();
    u.query_pairs_mut()
        .append_pair("code", code)
        .append_pair("state", &url_param(url, "state"))
        .append_pair("iss", iss);
    format!("{}?{}", u.path(), u.query().unwrap())
}

async fn get_page(url: &str) -> (u16, String, HeaderMap) {
    let r = reqwest::get(url).await.unwrap();
    let s = r.status().as_u16();
    let headers = r.headers().clone();
    (s, r.text().await.unwrap(), headers)
}

async fn post_form(url: &str, form: &[(&str, &str)]) -> (u16, String) {
    let r = reqwest::Client::new()
        .post(url)
        .form(form)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

fn assert_html_headers(headers: &HeaderMap) {
    let h = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    // security_headers erweitert no-store zu "no-store, no-cache, must-revalidate".
    assert!(
        h("cache-control").contains("no-store"),
        "{}",
        h("cache-control")
    );
    assert_eq!(h("referrer-policy"), "no-referrer");
    assert_eq!(
        h("content-security-policy"),
        "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'"
    );
    assert!(
        h("content-type").starts_with("text/html"),
        "{}",
        h("content-type")
    );
}

/// Kompletter Login ueber HTTP; liefert die Anmelde-URL.
async fn login(h: &H) -> String {
    let (s, j) = post(
        h,
        "/api/integrations/add",
        serde_json::json!({"id":"probe"}),
    )
    .await;
    assert_eq!(s, 201, "{j}");
    assert_eq!(j["zustand"], "anmeldung_noetig", "{j}");
    let (s, j) = post(
        h,
        "/api/integrations/probe/oauth/start",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(s, 200, "{j}");
    let url = j["anmelde_url"].as_str().unwrap().to_string();
    let (s, page) = follow_login_redirect(&url).await;
    assert_eq!(s, 200, "{page}");
    assert!(page.contains("Anmeldung erfolgreich"), "{page}");
    url
}

fn oneshot_req(peer: &str, method: &str, path: &str, body: Body, form: bool) -> Request<Body> {
    let addr: SocketAddr = peer.parse().unwrap();
    let mut b = Request::builder().method(method).uri(path);
    if form {
        b = b.header("content-type", "application/x-www-form-urlencoded");
    }
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

async fn oneshot_from(app: &Router, peer: &str, method: &str, path: &str) -> Response {
    app.clone()
        .oneshot(oneshot_req(peer, method, path, Body::empty(), false))
        .await
        .unwrap()
}

async fn body_text(resp: Response) -> String {
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8_lossy(&b).into_owned()
}

const READ_PATHS: [&str; 4] = [
    "/api/integrations",
    "/api/integrations/health",
    "/api/config",
    "/api/audit/recent?n=200",
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn oauth_full_cycle_over_http_and_no_token_leaks() {
    let mock = start_mock().await;
    let h = harness_with_oauth_probe(&mock).await;

    // Start verlangt den Bearer.
    let r = reqwest::Client::new()
        .post(format!("{}/api/integrations/probe/oauth/start", h.base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);

    let url = login(&h).await;
    let state = url_param(&url, "state");
    assert_eq!(
        zustand_of(&get_json(&h, "/api/integrations").await),
        "verbunden"
    );
    assert_eq!(mock.c.code.load(Ordering::SeqCst), 1);

    // Seite des Rueckrufs: Kopfzeilen, und weder code noch state noch iss.
    // Zweiter Rueckruf mit demselben state: abgelehnt.
    let r = reqwest::get(format!(
        "{}{}",
        h.base,
        callback_path_from(&url, "CODE-OK", &mock.base)
    ))
    .await
    .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    assert_html_headers(r.headers());
    let again = r.text().await.unwrap();
    assert!(again.contains("Anmeldung fehlgeschlagen"), "{again}");
    assert!(again.contains("state ungueltig"), "{again}");
    for c in ["CODE-OK", state.as_str(), mock.base.as_str(), "KANARIE"] {
        assert!(!again.contains(c), "callback page leaks {c}");
    }
    assert_eq!(mock.c.code.load(Ordering::SeqCst), 1, "no second exchange");
    assert_eq!(
        zustand_of(&get_json(&h, "/api/integrations").await),
        "verbunden"
    );

    for p in READ_PATHS {
        let b = get_text(&h, p).await.1;
        for c in [
            "KANARIE-AT-1",
            "KANARIE-AT-2",
            "KANARIE-RT-1",
            "KANARIE-CLIENT",
            "CODE-OK",
            state.as_str(),
        ] {
            assert!(!b.contains(c), "{p} leaks {c}");
        }
    }

    // Abmelden verlangt den Bearer, dann 200 und anmeldung_noetig.
    let r = reqwest::Client::new()
        .post(format!("{}/api/integrations/probe/oauth/abmelden", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    let (s, j) = post(
        &h,
        "/api/integrations/probe/oauth/abmelden",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(s, 200, "{j}");
    assert_eq!(j["status"], "abgemeldet");
    assert_eq!(mock.c.revoke.load(Ordering::SeqCst), 1);
    assert_eq!(
        zustand_of(&get_json(&h, "/api/integrations").await),
        "anmeldung_noetig"
    );

    // Start fuer eine nicht installierte Integration: 409 mit Klasse.
    let (s, j) = post(
        &h,
        "/api/integrations/gibtsnicht/oauth/start",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(s, 409, "{j}");
    assert_eq!(j["error"], "keine oauth-integration");
}

/// Abgelehntes Access-Token beim Werkzeugaufruf ueber die echte `/mcp`-Route:
/// genau eine Erneuerung, ein Wiederholungsversuch, genau eine Audit-Zeile.
#[tokio::test(flavor = "multi_thread")]
async fn rejected_token_on_tool_call_refreshes_once_and_audits_once() {
    use openfang_types::agent::{
        AgentEntry, AgentId, AgentManifest, AgentMode, AgentState, SessionId,
    };
    let mock = start_mock().await;
    let h = harness_with_oauth_probe(&mock).await;
    login(&h).await;

    let agent_id = AgentId::new();
    let manifest = AgentManifest {
        name: "probe-caller".into(),
        mcp_servers: vec!["probe".into()],
        ..Default::default()
    };
    h.state
        .kernel
        .registry
        .register(AgentEntry {
            id: agent_id,
            name: manifest.name.clone(),
            manifest,
            state: AgentState::Running,
            mode: AgentMode::default(),
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
            parent: None,
            children: vec![],
            session_id: SessionId::new(),
            tags: vec![],
            identity: Default::default(),
            onboarding_completed: false,
            onboarding_completed_at: None,
        })
        .unwrap();

    // Anbieter lehnt ab jetzt KANARIE-AT-1 ab.
    mock.c.only_at2.store(true, Ordering::SeqCst);
    let audit_before: usize = integration_rows(&h).await.len();
    assert_eq!(audit_before, 0);

    let r = reqwest::Client::new()
        .post(format!("{}/mcp", h.base))
        .bearer_auth(API_KEY)
        .header("X-OpenFang-Agent-Id", agent_id.to_string())
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "mcp_probe_get_me", "arguments": {}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let j: serde_json::Value = r.json().await.unwrap();
    assert!(j.get("error").is_none(), "{j}");
    assert_eq!(j["result"]["isError"], false, "{j}");
    assert!(
        j["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("probe-ok"),
        "{j}"
    );
    assert_eq!(
        mock.c.refresh.load(Ordering::SeqCst),
        1,
        "exactly one refresh"
    );

    let rows = integration_rows(&h).await;
    assert_eq!(rows.len(), 1, "exactly one integration_tool row: {rows:?}");
    assert!(
        rows[0]["detail"]
            .as_str()
            .unwrap()
            .starts_with("integration_tool=mcp_probe_get_me "),
        "{rows:?}"
    );
    assert_eq!(rows[0]["outcome"], "ok", "{rows:?}");
    assert_eq!(
        zustand_of(&get_json(&h, "/api/integrations").await),
        "verbunden"
    );
    for p in READ_PATHS {
        let b = get_text(&h, p).await.1;
        for c in ["KANARIE-AT-", "KANARIE-RT-"] {
            assert!(!b.contains(c), "{p} leaks {c}");
        }
    }
}

async fn integration_rows(h: &H) -> Vec<serde_json::Value> {
    get_json(h, "/api/audit/recent?n=200").await["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            e["detail"]
                .as_str()
                .is_some_and(|d| d.starts_with("integration_tool="))
        })
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn integration_prefix_is_never_issuable_or_storable() {
    // Allowlistet UND aufloesbar: ohne Sperre wuerde der Wert ausgegeben.
    let h = harness(Opts {
        template: None,
        with_vault: false,
        files: vec![
            (
                "issuable_credentials.list",
                "INTEGRATION_GITHUB_PAT\n".into(),
            ),
            (".env", "INTEGRATION_GITHUB_PAT=KANARIE-ISSUE-3\n".into()),
        ],
    })
    .await;
    let r = reqwest::Client::new()
        .post(format!("{}/api/credentials/issue", h.base))
        .bearer_auth(API_KEY)
        .header("x-openfang-issue-key", ISSUE_KEY)
        .json(&serde_json::json!({"reference": "INTEGRATION_GITHUB_PAT"}))
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    let b = r.text().await.unwrap();
    assert!(
        !b.contains("KANARIE-ISSUE-3"),
        "issued an INTEGRATION_ value"
    );
    assert_eq!(s, 404);
    let j: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(j["error"], "credential_unavailable");

    let (s, j) = post(
        &h,
        "/api/credentials/store",
        serde_json::json!({"reference": "INTEGRATION_GITHUB_PAT", "value": "KANARIE-STORE", "overwrite": true}),
    )
    .await;
    assert_eq!(s, 400, "{j}");
    assert_eq!(j["error"], "reference_invalid");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_time_key_page_stores_once_and_never_echoes() {
    let mock = start_mock().await;
    let h = harness(Opts {
        template: Some(static_template(&mock.base, "INTEGRATION_PROBE_KEY")),
        with_vault: true,
        files: vec![],
    })
    .await;
    let (s, j) = post(
        &h,
        "/api/integrations/add",
        serde_json::json!({"id":"probe"}),
    )
    .await;
    assert_eq!(s, 201, "{j}");
    assert_eq!(j["zustand"], "fehlt_schluessel", "{j}");

    // Link verlangt den Bearer.
    let r = reqwest::Client::new()
        .post(format!("{}/api/integrations/probe/schluessel/link", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    let (s, j) = post(
        &h,
        "/api/integrations/probe/schluessel/link",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(s, 200, "{j}");
    let url = j["url"].as_str().unwrap().to_string();
    assert!(
        url.starts_with(&format!("{}/api/integrations/probe/schluessel/", h.base)),
        "{url}"
    );

    let (s, form, headers) = get_page(&url).await;
    assert_eq!(s, 200, "{form}");
    assert!(form.contains("type=\"password\""), "{form}");
    assert!(form.contains("name=\"wert\""), "{form}");
    assert_html_headers(&headers);

    let (s, body) = post_form(&url, &[("wert", "KANARIE-STATIC-9")]).await;
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("gespeichert"), "{body}");
    assert!(!body.contains("KANARIE-STATIC-9"));
    assert_eq!(
        zustand_of(&get_json(&h, "/api/integrations").await),
        "verbunden"
    );

    let (s, body) = post_form(&url, &[("wert", "noch einmal")]).await;
    assert_eq!(s, 400, "single use");
    assert!(!body.contains("noch einmal"));
    assert_eq!(get_page(&url).await.0, 404, "consumed link is gone");

    for p in READ_PATHS {
        assert!(!get_text(&h, p).await.1.contains("KANARIE-STATIC-9"), "{p}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_routes_refuse_non_loopback_peers() {
    let mock = start_mock().await;
    let h = harness(Opts {
        template: Some(static_template(&mock.base, "INTEGRATION_PROBE_KEY_LB")),
        with_vault: true,
        files: vec![],
    })
    .await;
    let (s, _) = post(
        &h,
        "/api/integrations/add",
        serde_json::json!({"id":"probe"}),
    )
    .await;
    assert_eq!(s, 201);
    let (_, j) = post(
        &h,
        "/api/integrations/probe/schluessel/link",
        serde_json::json!({}),
    )
    .await;
    let url = j["url"].as_str().unwrap().to_string();
    let path = reqwest::Url::parse(&url).unwrap().path().to_string();

    let far = "100.67.177.45:5555";
    for p in [
        "/api/integrations/probe/oauth/callback?state=x&code=y",
        path.as_str(),
    ] {
        let resp = oneshot_from(&h.app, far, "GET", p).await;
        let st = resp.status();
        assert!(st == 401 || st == 404, "{p}: {st}");
        assert!(!body_text(resp).await.contains("password"), "{p}");
    }
    // POST vom Fernnetz verbraucht den Link nicht.
    let resp = h
        .app
        .clone()
        .oneshot(oneshot_req(
            far,
            "POST",
            &path,
            Body::from("wert=KANARIE-FERN"),
            true,
        ))
        .await
        .unwrap();
    assert!(
        resp.status() == 401 || resp.status() == 404,
        "{}",
        resp.status()
    );
    // Loopback sieht das Formular weiterhin (Link nicht verbraucht).
    let resp = oneshot_from(&h.app, "127.0.0.1:5555", "GET", &path).await;
    assert_eq!(resp.status(), 200);
    assert!(body_text(resp).await.contains("password"));

    // Defence in Depth: die Handler selbst pruefen ConnectInfo (ohne Middleware).
    let bare = Router::new()
        .route(
            "/api/integrations/{id}/oauth/callback",
            get(routes::integration_oauth_callback),
        )
        .route(
            "/api/integrations/{id}/schluessel/{token}",
            get(routes::integration_schluessel_page).post(routes::integration_schluessel_submit),
        )
        .with_state(h.state.clone());
    for p in [
        "/api/integrations/probe/oauth/callback?state=x&code=y",
        path.as_str(),
    ] {
        let resp = oneshot_from(&bare, far, "GET", p).await;
        assert_eq!(resp.status(), 404, "{p}");
        assert!(!body_text(resp).await.contains("password"));
        // Ganz ohne ConnectInfo: ebenfalls abgelehnt.
        let resp = bare
            .clone()
            .oneshot(Request::builder().uri(p).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{p} without ConnectInfo");
    }
    let resp = bare
        .clone()
        .oneshot(oneshot_req(
            far,
            "POST",
            &path,
            Body::from("wert=KANARIE-FERN"),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = oneshot_from(&bare, "127.0.0.1:5555", "GET", &path).await;
    assert_eq!(resp.status(), 200, "link still unused");
}
