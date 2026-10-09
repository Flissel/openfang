//! MCP-OAuth protocol layer (Teilprojekt 1b).
//!
//! Pure protocol functions for remote MCP servers that require OAuth 2.1:
//! discovery (RFC 9728 protected-resource metadata + RFC 8414 authorization
//! server metadata), dynamic client registration (RFC 7591), PKCE (S256),
//! authorization-code exchange, refresh and revocation (RFC 7009).
//!
//! Security rules enforced here:
//! - Only `https` URLs are accepted. `http` on loopback is possible only with
//!   the cargo feature `test-insecure-oauth` (test builds).
//! - Provider response bodies are only ever deserialised into expected fields.
//!   They never appear in errors, logs or [`OAuthError::class`].
//! - Tokens and the client id never appear in `Debug` output.

use base64::Engine;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use zeroize::Zeroizing;

/// Timeout for every request to a provider.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound for a provider response body we are willing to read.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Default token lifetime when the provider omits `expires_in`.
const DEFAULT_EXPIRES_IN: i64 = 3600;
/// Upper bound for `expires_in` (one year) to keep the arithmetic sane.
const MAX_EXPIRES_IN: i64 = 365 * 24 * 3600;
/// Refresh this long before the access token expires.
const REFRESH_MARGIN_SECS: i64 = 120;
/// Current schema version of [`OAuthRecord`].
const RECORD_VERSION: u8 = 1;

// ─── URL policy ──────────────────────────────────────────────────────────────

/// Which URLs the protocol layer may contact.
#[derive(Debug, Clone)]
pub struct UrlPolicy {
    allow_loopback_http: bool,
}

impl UrlPolicy {
    /// Production policy: `https` only.
    pub fn strict() -> Self {
        Self {
            allow_loopback_http: false,
        }
    }

    /// Test-only policy: additionally allows `http` to `127.0.0.1`, `::1`
    /// and `localhost`. Exists only with the feature `test-insecure-oauth`.
    #[cfg(feature = "test-insecure-oauth")]
    pub fn allow_loopback_http() -> Self {
        Self {
            allow_loopback_http: true,
        }
    }

    /// Parse `url` and check it against the policy.
    pub fn check(&self, url: &str) -> Result<url::Url, OAuthError> {
        let parsed = url::Url::parse(url).map_err(|_| OAuthError::Discovery("ungueltige url"))?;
        match parsed.scheme() {
            "https" => Ok(parsed),
            "http" if self.allow_loopback_http && is_loopback_host(&parsed) => Ok(parsed),
            _ => Err(OAuthError::Discovery("unsicheres schema")),
        }
    }
}

fn is_loopback_host(u: &url::Url) -> bool {
    match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Why a refresh failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailure {
    /// The refresh token is invalid/consumed (or missing). A new login is needed.
    InvalidGrant,
    /// Any other HTTP status from the token endpoint.
    Http(u16),
}

/// Protocol errors. Carries only fixed reasons and HTTP status codes, never
/// provider response bodies or secret values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthError {
    Discovery(&'static str),
    Registration(u16),
    TokenExchange(u16),
    Refresh(RefreshFailure),
    Revoke(u16),
    Network(&'static str),
    Vault,
}

impl OAuthError {
    /// Fixed class text for `detail`. Never contains response bodies.
    pub fn class(&self) -> String {
        match self {
            OAuthError::Discovery(r) => format!("ermittlung fehlgeschlagen: {r}"),
            OAuthError::Registration(s) => format!("registrierung abgelehnt (http {s})"),
            OAuthError::TokenExchange(s) => format!("token-tausch fehlgeschlagen (http {s})"),
            OAuthError::Refresh(RefreshFailure::InvalidGrant) => {
                "erneuerung fehlgeschlagen".to_string()
            }
            OAuthError::Refresh(RefreshFailure::Http(s)) => {
                format!("erneuerung fehlgeschlagen (http {s})")
            }
            OAuthError::Revoke(s) => format!("widerruf fehlgeschlagen (http {s})"),
            OAuthError::Network(c) => format!("nicht erreichbar: {c}"),
            OAuthError::Vault => "tresor nicht verfuegbar".to_string(),
        }
    }
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.class())
    }
}

impl std::error::Error for OAuthError {}

/// Map a transport error to a fixed network class. The inner error texts are
/// only inspected locally to tell TLS failures apart, never passed on.
fn network_error(e: &reqwest::Error) -> OAuthError {
    if e.is_timeout() {
        return OAuthError::Network("timeout");
    }
    if is_tls_error(e) {
        return OAuthError::Network("tls");
    }
    OAuthError::Network("verbindung")
}

fn is_tls_error(e: &reqwest::Error) -> bool {
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(inner) = src {
        let msg = inner.to_string().to_ascii_lowercase();
        if msg.contains("certificate") || msg.contains("tls") || msg.contains("handshake") {
            return true;
        }
        src = inner.source();
    }
    false
}

// ─── Record ──────────────────────────────────────────────────────────────────

/// Everything needed to log in, refresh and revoke for one integration.
/// Stored in the vault (serialised as JSON).
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthRecord {
    pub v: u8,
    pub resource: String,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub client_id: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
}

fn redact(v: &Option<String>) -> Option<&'static str> {
    v.as_ref().map(|_| "<redacted>")
}

impl std::fmt::Debug for OAuthRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthRecord")
            .field("v", &self.v)
            .field("resource", &self.resource)
            .field("issuer", &self.issuer)
            .field("authorization_endpoint", &self.authorization_endpoint)
            .field("token_endpoint", &self.token_endpoint)
            .field("registration_endpoint", &self.registration_endpoint)
            .field("revocation_endpoint", &self.revocation_endpoint)
            .field("client_id", &redact(&self.client_id))
            .field("access_token", &redact(&self.access_token))
            .field("refresh_token", &redact(&self.refresh_token))
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl OAuthRecord {
    /// True when there is no access token, its expiry is unknown, or it
    /// expires within 120 seconds.
    pub fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        self.access_token.is_none()
            || match self.expires_at {
                None => true,
                Some(e) => e - now < chrono::Duration::seconds(REFRESH_MARGIN_SECS),
            }
    }

    /// True when an access token is present.
    pub fn has_tokens(&self) -> bool {
        self.access_token.is_some()
    }

    #[cfg(test)]
    pub(crate) fn empty_for_tests() -> Self {
        Self {
            v: RECORD_VERSION,
            resource: String::new(),
            issuer: String::new(),
            authorization_endpoint: String::new(),
            token_endpoint: String::new(),
            registration_endpoint: None,
            revocation_endpoint: None,
            client_id: None,
            access_token: None,
            refresh_token: None,
            expires_at: None,
            scopes: Vec::new(),
        }
    }
}

// ─── HTTP helpers ────────────────────────────────────────────────────────────

/// Read a response body (bounded) and deserialise it into `T`.
/// Returns `None` on oversize or malformed bodies; the raw body is dropped
/// (and zeroised) without ever being exposed.
async fn read_json<T: serde::de::DeserializeOwned>(mut resp: reqwest::Response) -> Option<T> {
    if resp
        .content_length()
        .is_some_and(|l| l > MAX_BODY_BYTES as u64)
    {
        return None;
    }
    let mut buf: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::new());
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > MAX_BODY_BYTES {
                    return None;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    serde_json::from_slice(&buf).ok()
}

/// GET a JSON document. `Ok(None)` means non-success status or unreadable body.
async fn get_json<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    policy: &UrlPolicy,
    url: &str,
) -> Result<Option<T>, OAuthError> {
    let u = policy.check(url)?;
    let resp = http
        .get(u)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| network_error(&e))?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    Ok(read_json(resp).await)
}

fn expires_at_from(expires_in: Option<i64>) -> DateTime<Utc> {
    let secs = expires_in
        .unwrap_or(DEFAULT_EXPIRES_IN)
        .clamp(0, MAX_EXPIRES_IN);
    Utc::now() + chrono::Duration::seconds(secs)
}

/// Extract `resource_metadata="…"` from a `WWW-Authenticate` header value.
fn parse_resource_metadata(header: &str) -> Option<String> {
    const KEY: &str = "resource_metadata=\"";
    let start = header.find(KEY)? + KEY.len();
    let rest = &header[start..];
    let end = rest.find('"')?;
    let v = &rest[..end];
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

fn same_host(a: &url::Url, b: &url::Url) -> bool {
    a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase)
        && a.port_or_known_default() == b.port_or_known_default()
}

// ─── Discovery ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Deserialize)]
struct AuthServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    revocation_endpoint: Option<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

/// Discover the authorization server of an MCP resource and validate its
/// metadata. The returned record carries no client id and no tokens.
pub async fn discover(
    http: &reqwest::Client,
    policy: &UrlPolicy,
    resource_url: &str,
) -> Result<OAuthRecord, OAuthError> {
    let resource = policy.check(resource_url)?;

    // 1. Unauthenticated initialize; a 401 names the metadata document.
    let probe = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "OpenFang", "version": env!("CARGO_PKG_VERSION")}
        }
    });
    let resp = http
        .post(resource.clone())
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(&probe)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| network_error(&e))?;
    let from_header = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        resp.headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(parse_resource_metadata)
    } else {
        None
    };
    drop(resp);

    // 2. Fallback: well-known location on the resource origin.
    let prm_url = from_header.unwrap_or_else(|| {
        format!(
            "{}/.well-known/oauth-protected-resource",
            resource.origin().ascii_serialization()
        )
    });

    // 3. Protected-resource metadata -> first authorization server -> its metadata.
    let prm: ProtectedResourceMetadata = get_json(http, policy, &prm_url)
        .await?
        .ok_or(OAuthError::Discovery("schutzbeschreibung nicht lesbar"))?;
    let as_url = prm
        .authorization_servers
        .into_iter()
        .next()
        .ok_or(OAuthError::Discovery("kein anmeldeserver"))?;
    policy.check(&as_url)?;
    let as_base = as_url.trim_end_matches('/');

    let meta: AuthServerMetadata = match get_json(
        http,
        policy,
        &format!("{as_base}/.well-known/oauth-authorization-server"),
    )
    .await?
    {
        Some(m) => m,
        None => get_json(
            http,
            policy,
            &format!("{as_base}/.well-known/openid-configuration"),
        )
        .await?
        .ok_or(OAuthError::Discovery(
            "anmeldeserver-metadaten nicht lesbar",
        ))?,
    };

    // 4a. Every URL passes the policy.
    let issuer = policy.check(&meta.issuer)?;
    let mut endpoints = vec![
        policy.check(&meta.authorization_endpoint)?,
        policy.check(&meta.token_endpoint)?,
    ];
    if let Some(r) = &meta.registration_endpoint {
        endpoints.push(policy.check(r)?);
    }
    if let Some(r) = &meta.revocation_endpoint {
        endpoints.push(policy.check(r)?);
    }

    // 4b. issuer must equal the announced authorization server (trailing '/' ignored).
    if meta.issuer.trim_end_matches('/') != as_base {
        return Err(OAuthError::Discovery("issuer passt nicht"));
    }

    // 4c. All endpoints live on the issuer's host.
    if !endpoints.iter().all(|e| same_host(e, &issuer)) {
        return Err(OAuthError::Discovery("fremder host"));
    }

    // 4d. PKCE with S256.
    if !meta
        .code_challenge_methods_supported
        .iter()
        .any(|m| m == "S256")
    {
        return Err(OAuthError::Discovery("kein s256"));
    }

    Ok(OAuthRecord {
        v: RECORD_VERSION,
        resource: resource_url.to_string(),
        issuer: meta.issuer,
        authorization_endpoint: meta.authorization_endpoint,
        token_endpoint: meta.token_endpoint,
        registration_endpoint: meta.registration_endpoint,
        revocation_endpoint: meta.revocation_endpoint,
        client_id: None,
        access_token: None,
        refresh_token: None,
        expires_at: None,
        scopes: Vec::new(),
    })
}

// ─── Dynamic client registration ─────────────────────────────────────────────

#[derive(Deserialize)]
struct RegistrationResponse {
    #[serde(default)]
    client_id: Option<String>,
}

/// Register a public client (no secret) via RFC 7591 and store its client id.
pub async fn register_client(
    http: &reqwest::Client,
    policy: &UrlPolicy,
    rec: &mut OAuthRecord,
    redirect_uri: &str,
) -> Result<(), OAuthError> {
    let endpoint = rec
        .registration_endpoint
        .as_deref()
        .ok_or(OAuthError::Discovery("keine selbstregistrierung"))?;
    let u = policy.check(endpoint)?;
    let mut body = serde_json::json!({
        "client_name": "OpenFang (VibeMind)",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none"
    });
    if !rec.scopes.is_empty() {
        body["scope"] = serde_json::Value::String(rec.scopes.join(" "));
    }
    let resp = http
        .post(u)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| network_error(&e))?;
    let status = resp.status().as_u16();
    if status != 200 && status != 201 {
        return Err(OAuthError::Registration(status));
    }
    let parsed: RegistrationResponse = read_json(resp)
        .await
        .ok_or(OAuthError::Registration(status))?;
    match parsed.client_id {
        Some(id) if !id.is_empty() => {
            rec.client_id = Some(id);
            Ok(())
        }
        _ => Err(OAuthError::Registration(status)),
    }
}

// ─── PKCE / random values ────────────────────────────────────────────────────

/// PKCE verifier (secret) and its S256 challenge. Deliberately not `Debug`.
pub struct Pkce {
    pub verifier: Zeroizing<String>,
    pub challenge: String,
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn random_b64url_32() -> Zeroizing<String> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(bytes.as_mut());
    Zeroizing::new(b64url(bytes.as_ref()))
}

/// Fresh PKCE pair: 32 random bytes (base64url) as verifier, challenge = S256(verifier).
pub fn pkce_pair() -> Pkce {
    let verifier = random_b64url_32();
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// 32 random bytes, base64url without padding (state, one-time tokens).
pub fn random_token() -> Zeroizing<String> {
    random_b64url_32()
}

// ─── Authorization request ───────────────────────────────────────────────────

/// Build the browser authorization URL (authorization code + PKCE S256).
pub fn authorize_url(
    rec: &OAuthRecord,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<String, OAuthError> {
    let client_id = rec
        .client_id
        .as_deref()
        .ok_or(OAuthError::Discovery("keine client_id"))?;
    let mut u = url::Url::parse(&rec.authorization_endpoint)
        .map_err(|_| OAuthError::Discovery("ungueltige url"))?;
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", &rec.resource);
        if !rec.scopes.is_empty() {
            q.append_pair("scope", &rec.scopes.join(" "));
        }
    }
    Ok(u.into())
}

// ─── Token endpoint ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    #[serde(default)]
    error: Option<String>,
}

async fn post_form(
    http: &reqwest::Client,
    policy: &UrlPolicy,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<reqwest::Response, OAuthError> {
    let u = policy.check(endpoint)?;
    http.post(u)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(form)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| network_error(&e))
}

/// Exchange an authorization code (with PKCE verifier) for tokens.
pub async fn exchange_code(
    http: &reqwest::Client,
    policy: &UrlPolicy,
    rec: &mut OAuthRecord,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<(), OAuthError> {
    let client_id = rec
        .client_id
        .clone()
        .ok_or(OAuthError::Discovery("keine client_id"))?;
    let resp = post_form(
        http,
        policy,
        &rec.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &client_id),
            ("code_verifier", verifier),
            ("resource", &rec.resource),
        ],
    )
    .await?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(OAuthError::TokenExchange(status));
    }
    let tr: TokenResponse = read_json(resp)
        .await
        .ok_or(OAuthError::TokenExchange(status))?;
    let access = tr
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or(OAuthError::TokenExchange(status))?;
    rec.access_token = Some(access);
    rec.refresh_token = tr.refresh_token.filter(|t| !t.is_empty());
    rec.expires_at = Some(expires_at_from(tr.expires_in));
    Ok(())
}

/// Refresh the access token. Adopts a rotated refresh token if one is returned,
/// otherwise keeps the old one. No automatic retry; `InvalidGrant` means a new
/// login is required.
pub async fn refresh(
    http: &reqwest::Client,
    policy: &UrlPolicy,
    rec: &mut OAuthRecord,
) -> Result<(), OAuthError> {
    let refresh_token = match rec.refresh_token.as_deref() {
        Some(t) if !t.is_empty() => Zeroizing::new(t.to_string()),
        _ => return Err(OAuthError::Refresh(RefreshFailure::InvalidGrant)),
    };
    let client_id = rec.client_id.clone().unwrap_or_default();
    let resp = post_form(
        http,
        policy,
        &rec.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", &client_id),
            ("resource", &rec.resource),
        ],
    )
    .await?;
    let status = resp.status().as_u16();
    if status == 400 || status == 401 {
        let err: Option<TokenErrorResponse> = read_json(resp).await;
        if err.and_then(|e| e.error).as_deref() == Some("invalid_grant") {
            return Err(OAuthError::Refresh(RefreshFailure::InvalidGrant));
        }
        return Err(OAuthError::Refresh(RefreshFailure::Http(status)));
    }
    if status != 200 {
        return Err(OAuthError::Refresh(RefreshFailure::Http(status)));
    }
    let tr: TokenResponse = read_json(resp)
        .await
        .ok_or(OAuthError::Refresh(RefreshFailure::Http(status)))?;
    let access = tr
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or(OAuthError::Refresh(RefreshFailure::Http(status)))?;
    rec.access_token = Some(access);
    if let Some(rt) = tr.refresh_token.filter(|t| !t.is_empty()) {
        rec.refresh_token = Some(rt);
    }
    rec.expires_at = Some(expires_at_from(tr.expires_in));
    Ok(())
}

// ─── Revocation ──────────────────────────────────────────────────────────────

/// Revoke the refresh token (or, without one, the access token) at the
/// provider. Without a revocation endpoint or without any token this is a no-op.
pub async fn revoke(
    http: &reqwest::Client,
    policy: &UrlPolicy,
    rec: &OAuthRecord,
) -> Result<(), OAuthError> {
    let Some(endpoint) = rec.revocation_endpoint.as_deref() else {
        return Ok(());
    };
    let Some(token) = rec.refresh_token.as_deref().or(rec.access_token.as_deref()) else {
        return Ok(());
    };
    let client_id = rec.client_id.as_deref().unwrap_or_default();
    let resp = post_form(
        http,
        policy,
        endpoint,
        &[("token", token), ("client_id", client_id)],
    )
    .await?;
    let status = resp.status().as_u16();
    if status == 200 {
        Ok(())
    } else {
        Err(OAuthError::Revoke(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "test-insecure-oauth")]
    use axum::{
        extract::Form,
        http::{HeaderMap, StatusCode},
        routing::{get, post},
        Json, Router,
    };
    #[cfg(feature = "test-insecure-oauth")]
    use std::sync::{Arc, Mutex};

    #[cfg(feature = "test-insecure-oauth")]
    #[derive(Default)]
    struct Seen {
        register: u32,
        token_bodies: Vec<String>,
        revoke: u32,
    }

    /// Test-Anmeldeserver + Schutzbeschreibung auf 127.0.0.1:<port>; Antwortverhalten per Flags.
    #[cfg(feature = "test-insecure-oauth")]
    async fn mock_as(rotate_refresh: bool, register_status: u16) -> (String, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let b = base.clone();
        let s1 = seen.clone();
        let s2 = seen.clone();
        let s3 = seen.clone();
        let app = Router::new()
            .route(
                "/mcp",
                post(move || {
                    let b = b.clone();
                    async move {
                        let mut h = HeaderMap::new();
                        h.insert(
                            "www-authenticate",
                            format!("Bearer error=\"invalid_token\", resource_metadata=\"{b}/.well-known/oauth-protected-resource\"")
                                .parse()
                                .unwrap(),
                        );
                        (StatusCode::UNAUTHORIZED, h)
                    }
                }),
            )
            .route(
                "/.well-known/oauth-protected-resource",
                get({
                    let b = base.clone();
                    move || async move {
                        Json(serde_json::json!({"resource": format!("{b}/mcp"), "authorization_servers": [b]}))
                    }
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get({
                    let b = base.clone();
                    move || async move {
                        Json(serde_json::json!({"issuer": b, "authorization_endpoint": format!("{b}/authorize"),
                          "token_endpoint": format!("{b}/token"), "registration_endpoint": format!("{b}/register"),
                          "revocation_endpoint": format!("{b}/revoke"), "code_challenge_methods_supported": ["S256"],
                          "grant_types_supported": ["authorization_code","refresh_token"],
                          "token_endpoint_auth_methods_supported": ["none"]}))
                    }
                }),
            )
            .route(
                "/register",
                post(move |Json(_b): Json<serde_json::Value>| {
                    let s = s1.clone();
                    async move {
                        s.lock().unwrap().register += 1;
                        if register_status != 201 {
                            return (
                                StatusCode::from_u16(register_status).unwrap(),
                                Json(serde_json::json!({"error":"KANARIE-REG-BODY"})),
                            );
                        }
                        (StatusCode::CREATED, Json(serde_json::json!({"client_id": "KANARIE-CLIENT"})))
                    }
                }),
            )
            .route(
                "/token",
                post(move |Form(f): Form<std::collections::HashMap<String, String>>| {
                    let s = s2.clone();
                    async move {
                        let mut g = s.lock().unwrap();
                        g.token_bodies.push(format!("{:?}", f.get("grant_type")));
                        match f.get("grant_type").map(String::as_str) {
                            Some("authorization_code")
                                if f.get("code").map(String::as_str) == Some("CODE-OK")
                                    && f.contains_key("code_verifier") =>
                            {
                                (StatusCode::OK, Json(serde_json::json!({"access_token":"KANARIE-AT-1","refresh_token":"KANARIE-RT-1","expires_in":3600,"token_type":"Bearer"})))
                            }
                            Some("refresh_token")
                                if f.get("refresh_token").map(String::as_str) == Some("KANARIE-RT-1") =>
                            {
                                (
                                    StatusCode::OK,
                                    Json(if rotate_refresh {
                                        serde_json::json!({"access_token":"KANARIE-AT-2","refresh_token":"KANARIE-RT-2","expires_in":3600})
                                    } else {
                                        serde_json::json!({"access_token":"KANARIE-AT-2","expires_in":3600})
                                    }),
                                )
                            }
                            Some("refresh_token") => (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({"error":"invalid_grant","error_description":"KANARIE-ERR-BODY"})),
                            ),
                            _ => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"invalid_request"}))),
                        }
                    }
                }),
            )
            .route(
                "/revoke",
                post(move || {
                    let s = s3.clone();
                    async move {
                        s.lock().unwrap().revoke += 1;
                        StatusCode::OK
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (base, seen)
    }

    #[cfg(feature = "test-insecure-oauth")]
    fn http() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[test]
    fn strict_policy_rejects_http_even_on_loopback() {
        assert!(UrlPolicy::strict().check("http://127.0.0.1:9/mcp").is_err());
        assert!(UrlPolicy::strict().check("https://mcp.vercel.com/").is_ok());
    }

    #[cfg(feature = "test-insecure-oauth")]
    #[tokio::test]
    async fn discover_register_authorize_exchange_refresh_revoke_full_cycle() {
        let (base, seen) = mock_as(true, 201).await;
        let p = UrlPolicy::allow_loopback_http();
        let mut rec = discover(&http(), &p, &format!("{base}/mcp")).await.unwrap();
        assert_eq!(rec.issuer, base);
        register_client(
            &http(),
            &p,
            &mut rec,
            "http://127.0.0.1:4200/api/integrations/x/oauth/callback",
        )
        .await
        .unwrap();
        assert_eq!(rec.client_id.as_deref(), Some("KANARIE-CLIENT"));
        let pk = pkce_pair();
        let st = random_token();
        let url = authorize_url(
            &rec,
            "http://127.0.0.1:4200/api/integrations/x/oauth/callback",
            &st,
            &pk.challenge,
        )
        .unwrap();
        assert!(
            url.contains("code_challenge_method=S256")
                && url.contains(&pk.challenge)
                && url.contains("response_type=code")
        );
        assert!(!url.contains(pk.verifier.as_str()));
        exchange_code(
            &http(),
            &p,
            &mut rec,
            "CODE-OK",
            &pk.verifier,
            "http://127.0.0.1:4200/api/integrations/x/oauth/callback",
        )
        .await
        .unwrap();
        assert_eq!(rec.access_token.as_deref(), Some("KANARIE-AT-1"));
        refresh(&http(), &p, &mut rec).await.unwrap();
        assert_eq!(rec.access_token.as_deref(), Some("KANARIE-AT-2"));
        assert_eq!(
            rec.refresh_token.as_deref(),
            Some("KANARIE-RT-2"),
            "rotated refresh token adopted"
        );
        revoke(&http(), &p, &rec).await.unwrap();
        assert_eq!(seen.lock().unwrap().revoke, 1);
        let dbg = format!("{rec:?}");
        for c in ["KANARIE-AT-2", "KANARIE-RT-2", "KANARIE-CLIENT"] {
            assert!(!dbg.contains(c), "Debug leaks {c}");
        }
    }

    #[cfg(feature = "test-insecure-oauth")]
    #[tokio::test]
    async fn refresh_keeps_old_refresh_token_when_not_rotated_and_invalid_grant_is_classified() {
        let (base, _) = mock_as(false, 201).await;
        let p = UrlPolicy::allow_loopback_http();
        let mut rec = discover(&http(), &p, &format!("{base}/mcp")).await.unwrap();
        register_client(&http(), &p, &mut rec, "http://127.0.0.1:1/cb")
            .await
            .unwrap();
        let pk = pkce_pair();
        exchange_code(
            &http(),
            &p,
            &mut rec,
            "CODE-OK",
            &pk.verifier,
            "http://127.0.0.1:1/cb",
        )
        .await
        .unwrap();
        refresh(&http(), &p, &mut rec).await.unwrap();
        assert_eq!(rec.refresh_token.as_deref(), Some("KANARIE-RT-1"));
        rec.refresh_token = Some("VERBRAUCHT".into());
        let err = refresh(&http(), &p, &mut rec).await.unwrap_err();
        assert!(matches!(
            err,
            OAuthError::Refresh(RefreshFailure::InvalidGrant)
        ));
        assert!(!err.class().contains("KANARIE-ERR-BODY"));
    }

    #[cfg(feature = "test-insecure-oauth")]
    #[tokio::test]
    async fn registration_rejection_is_a_fixed_class_without_body() {
        let (base, _) = mock_as(true, 403).await;
        let p = UrlPolicy::allow_loopback_http();
        let mut rec = discover(&http(), &p, &format!("{base}/mcp")).await.unwrap();
        let err = register_client(&http(), &p, &mut rec, "http://127.0.0.1:1/cb")
            .await
            .unwrap_err();
        assert_eq!(err.class(), "registrierung abgelehnt (http 403)");
    }

    #[cfg(feature = "test-insecure-oauth")]
    #[tokio::test]
    async fn discovery_rejects_issuer_mismatch_and_foreign_host_endpoints() {
        // Anmeldeserver, dessen issuer nicht zur Schutzbeschreibung passt
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = format!("http://{}", l.local_addr().unwrap());
        let b2 = b.clone();
        let b3 = b.clone();
        let app = Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(move || {
                    let b = b2.clone();
                    async move { Json(serde_json::json!({"authorization_servers":[b]})) }
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(move || {
                    let _b = b3.clone();
                    async move {
                        Json(serde_json::json!({
                        "issuer":"http://evil.example","authorization_endpoint":"http://evil.example/a","token_endpoint":"http://evil.example/t",
                        "code_challenge_methods_supported":["S256"]}))
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let err = discover(
            &http(),
            &UrlPolicy::allow_loopback_http(),
            &format!("{b}/mcp"),
        )
        .await
        .unwrap_err();
        assert!(err.class().starts_with("ermittlung fehlgeschlagen"));
    }

    /// Mock without a `/mcp` route (no 401 header -> well-known fallback).
    /// `as_entry` is the `authorization_servers[0]` value, `meta` builds the AS
    /// metadata from the base URL.
    #[cfg(feature = "test-insecure-oauth")]
    async fn mock_meta(
        as_entry: impl Fn(&str) -> String + Send + 'static,
        meta: impl Fn(&str) -> serde_json::Value + Send + Sync + 'static,
    ) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = format!("http://{}", l.local_addr().unwrap());
        let prm = serde_json::json!({"authorization_servers": [as_entry(&b)]});
        let m = meta(&b);
        let app = Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(move || {
                    let prm = prm.clone();
                    async move { Json(prm) }
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(move || {
                    let m = m.clone();
                    async move { Json(m) }
                }),
            );
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        b
    }

    #[cfg(feature = "test-insecure-oauth")]
    fn good_meta(issuer: &str, ep_base: &str) -> serde_json::Value {
        serde_json::json!({"issuer": issuer,
            "authorization_endpoint": format!("{ep_base}/authorize"),
            "token_endpoint": format!("{ep_base}/token"),
            "code_challenge_methods_supported": ["S256"]})
    }

    #[cfg(feature = "test-insecure-oauth")]
    #[tokio::test]
    async fn discovery_specific_rule_violations_are_classified() {
        let p = UrlPolicy::allow_loopback_http();
        // issuer differs from the announced authorization server (both loopback-allowed)
        let b = mock_meta(
            |b| b.to_string(),
            |b| good_meta(&b.replace("127.0.0.1", "localhost"), b),
        )
        .await;
        let e = discover(&http(), &p, &format!("{b}/mcp"))
            .await
            .unwrap_err();
        assert_eq!(e, OAuthError::Discovery("issuer passt nicht"));

        // token endpoint on a different host than the issuer
        let b = mock_meta(
            |b| b.to_string(),
            |b| {
                let mut m = good_meta(b, b);
                m["token_endpoint"] =
                    serde_json::json!(format!("{}/token", b.replace("127.0.0.1", "localhost")));
                m
            },
        )
        .await;
        let e = discover(&http(), &p, &format!("{b}/mcp"))
            .await
            .unwrap_err();
        assert_eq!(e, OAuthError::Discovery("fremder host"));

        // no S256
        let b = mock_meta(
            |b| b.to_string(),
            |b| {
                let mut m = good_meta(b, b);
                m["code_challenge_methods_supported"] = serde_json::json!(["plain"]);
                m
            },
        )
        .await;
        let e = discover(&http(), &p, &format!("{b}/mcp"))
            .await
            .unwrap_err();
        assert_eq!(e, OAuthError::Discovery("kein s256"));
    }

    #[cfg(feature = "test-insecure-oauth")]
    #[tokio::test]
    async fn discovery_normalises_trailing_slash_and_falls_back_to_well_known() {
        // authorization_servers entry with trailing '/', issuer without (Vercel-like),
        // and no 401 header from the resource -> origin well-known fallback.
        let b = mock_meta(|b| format!("{b}/"), |b| good_meta(b, b)).await;
        let rec = discover(
            &http(),
            &UrlPolicy::allow_loopback_http(),
            &format!("{b}/mcp"),
        )
        .await
        .unwrap();
        assert_eq!(rec.issuer, b);
        assert_eq!(rec.resource, format!("{b}/mcp"));
        assert!(!rec.has_tokens() && rec.client_id.is_none());
        assert!(rec.registration_endpoint.is_none());
        // no registration endpoint -> fixed discovery class
        let mut rec = rec;
        let e = register_client(
            &http(),
            &UrlPolicy::allow_loopback_http(),
            &mut rec,
            "http://127.0.0.1:1/cb",
        )
        .await
        .unwrap_err();
        assert_eq!(
            e.class(),
            "ermittlung fehlgeschlagen: keine selbstregistrierung"
        );
    }

    #[test]
    fn strict_policy_rejects_non_https_schemes_and_parses_header() {
        let p = UrlPolicy::strict();
        for u in [
            "http://localhost/x",
            "http://[::1]/x",
            "ftp://example.com/",
            "file:///etc/passwd",
            "kaputt",
        ] {
            assert!(p.check(u).is_err(), "{u} must be rejected");
        }
        assert_eq!(
            parse_resource_metadata(
                "Bearer error=\"invalid_token\", error_description=\"No authorization provided\", resource_metadata=\"https://mcp.vercel.com/.well-known/oauth-protected-resource\""
            )
            .as_deref(),
            Some("https://mcp.vercel.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(
            parse_resource_metadata("Bearer error=\"invalid_token\""),
            None
        );
    }

    #[cfg(feature = "test-insecure-oauth")]
    #[test]
    fn insecure_policy_allows_only_loopback_http() {
        let p = UrlPolicy::allow_loopback_http();
        assert!(p.check("http://127.0.0.1:9/x").is_ok());
        assert!(p.check("http://localhost:9/x").is_ok());
        assert!(p.check("http://[::1]:9/x").is_ok());
        assert!(p.check("http://evil.example/x").is_err());
        assert!(p.check("http://10.0.0.1/x").is_err());
    }

    #[test]
    fn authorize_url_requires_client_id_and_classes_never_leak() {
        let mut r = OAuthRecord::empty_for_tests();
        r.authorization_endpoint = "https://as.example/authorize".into();
        r.resource = "https://mcp.example/".into();
        assert_eq!(
            authorize_url(&r, "http://127.0.0.1:1/cb", "S", "C").unwrap_err(),
            OAuthError::Discovery("keine client_id")
        );
        r.client_id = Some("KANARIE-CID".into());
        r.scopes = vec!["a".into(), "offline_access".into()];
        let u = url::Url::parse(&authorize_url(&r, "http://127.0.0.1:1/cb", "S", "C").unwrap())
            .unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["resource"], "https://mcp.example/");
        assert_eq!(q["scope"], "a offline_access");
        assert_eq!(q["state"], "S");
        // Debug of a record without tokens shows None, with tokens never the value
        assert!(!format!("{r:?}").contains("KANARIE-CID"));
        for e in [
            OAuthError::TokenExchange(400),
            OAuthError::Refresh(RefreshFailure::Http(500)),
            OAuthError::Revoke(503),
            OAuthError::Network("tls"),
            OAuthError::Vault,
        ] {
            assert!(!e.class().is_empty() && !e.class().contains("KANARIE"));
        }
        assert_eq!(OAuthError::Vault.class(), "tresor nicht verfuegbar");
    }

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let pk = pkce_pair();
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(pk.verifier.as_bytes()));
        assert_eq!(pk.challenge, expect);
        assert!(pk.verifier.len() >= 43);
    }

    #[test]
    fn needs_refresh_under_120_seconds() {
        let now = chrono::Utc::now();
        let mut r = OAuthRecord::empty_for_tests();
        r.access_token = Some("x".into());
        r.expires_at = Some(now + chrono::Duration::seconds(119));
        assert!(r.needs_refresh(now));
        r.expires_at = Some(now + chrono::Duration::seconds(600));
        assert!(!r.needs_refresh(now));
    }
}
