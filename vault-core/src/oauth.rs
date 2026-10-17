//! Minimal OAuth 2.1 for the MCP server, in the shape Claude's connectors
//! (claude.ai, desktop and mobile apps) expect: discovery metadata (RFC 9728
//! and RFC 8414), dynamic client registration (RFC 7591), authorization code
//! with PKCE S256, and rotating refresh tokens.
//!
//! There is a single user: the authorization page asks for `MCP_PASSWORD`.
//! The static `MCP_TOKEN` is also accepted as a bearer token, for clients that
//! let you set headers (Claude Code).
//!
//! The code is public, so the defenses assume an attacker who has read it:
//! secrets are compared in constant time and stored only as hashes, password
//! attempts and registrations are rate limited per client IP (so an attacker
//! can only lock out themselves), and registrations that never complete an
//! authorization are pruned.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Form, Query, Request, State as AxState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD};
use base64::Engine;
use rand::{CryptoRng, Rng};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::config::McpConfig;
use crate::state::{OAuthClient, State};

const ACCESS_TTL: Duration = Duration::from_secs(60 * 60);
const REFRESH_TTL: Duration = Duration::from_secs(90 * 24 * 60 * 60);
const CODE_TTL: Duration = Duration::from_secs(10 * 60);
/// Registration is open (the protocol requires it); this caps the table.
const MAX_CLIENTS: i64 = 1000;
/// Registrations that never complete an authorization are dropped after this.
const UNAUTHORIZED_CLIENT_TTL: Duration = Duration::from_secs(60 * 60);
/// Wrong passwords allowed per client IP within the window.
const MAX_FAILURES: usize = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Registrations allowed per client IP within the window.
const MAX_REGISTRATIONS: usize = 20;
const REGISTRATION_WINDOW: Duration = Duration::from_secs(60 * 60);
#[cfg(not(test))]
const FAILURE_DELAY: Duration = Duration::from_secs(1);
#[cfg(test)]
const FAILURE_DELAY: Duration = Duration::from_millis(10);

pub struct Auth {
    public_url: String,
    token: Option<[u8; 32]>,
    password: Option<[u8; 32]>,
    state: Arc<State>,
    /// Authorization codes not exchanged yet, keyed by hash. They live for
    /// minutes, so they don't need to survive a restart.
    codes: Mutex<HashMap<String, PendingCode>>,
    failures: Throttle,
    registrations: Throttle,
}

struct PendingCode {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    created: Instant,
}

type OAuthError = (&'static str, &'static str);

impl Auth {
    pub fn new(cfg: &McpConfig, state: Arc<State>) -> anyhow::Result<Self> {
        if let Some(password) = &cfg.password {
            revoke_if_password_changed(&state, password)?;
        }
        Ok(Self {
            public_url: cfg.public_url.clone(),
            token: cfg.token.as_deref().map(digest),
            password: cfg.password.as_deref().map(digest),
            state,
            codes: Mutex::default(),
            failures: Throttle::new(MAX_FAILURES, FAILURE_WINDOW),
            registrations: Throttle::new(MAX_REGISTRATIONS, REGISTRATION_WINDOW),
        })
    }

    fn oauth_enabled(&self) -> bool {
        self.password.is_some()
    }

    fn resource_metadata_url(&self) -> String {
        format!(
            "{}/.well-known/oauth-protected-resource/mcp",
            self.public_url
        )
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(token) = bearer(headers) else {
            return false;
        };
        if self.token.is_some_and(|t| ct_eq(&t, &digest(token))) {
            return true;
        }
        if !self.oauth_enabled() {
            return false;
        }
        match self.state.token_client(&hash(token), "access", now()) {
            Ok(client) => client.is_some(),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "failed to validate token");
                false
            }
        }
    }

    fn check_password(&self, password: &str) -> bool {
        self.password.is_some_and(|p| ct_eq(&p, &digest(password)))
    }

    /// Checks the `/oauth/authorize` parameters. Errors here render an error
    /// page and never redirect: the `redirect_uri` is not trusted yet.
    fn validate(&self, p: &AuthorizeParams) -> Result<OAuthClient, &'static str> {
        let client = match self.state.client(&p.client_id) {
            Ok(Some(c)) => c,
            Ok(None) => return Err("Unknown client. Remove the connector and add it again."),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "failed to read OAuth client");
                return Err("Internal error.");
            }
        };
        if !client.redirect_uris.contains(&p.redirect_uri) {
            return Err("This redirect address is not registered for the client.");
        }
        if p.response_type != "code" {
            return Err("Only the authorization code flow is supported.");
        }
        if p.code_challenge_method != "S256" || !(43..=128).contains(&p.code_challenge.len()) {
            return Err("PKCE with S256 is required.");
        }
        Ok(client)
    }

    fn new_code(&self, p: &AuthorizeParams) -> String {
        let code = random_token();
        let mut codes = lock(&self.codes);
        codes.retain(|_, c| c.created.elapsed() < CODE_TTL);
        codes.insert(
            hash(&code),
            PendingCode {
                client_id: p.client_id.clone(),
                redirect_uri: p.redirect_uri.clone(),
                challenge: p.code_challenge.clone(),
                created: Instant::now(),
            },
        );
        code
    }

    fn exchange_code(&self, req: &TokenRequest) -> Result<String, OAuthError> {
        let invalid = ("invalid_grant", "invalid or expired code");
        let code = req.code.as_deref().ok_or(invalid)?;
        // Single use: the code leaves the map even if the exchange fails.
        let pending = lock(&self.codes).remove(&hash(code)).ok_or(invalid)?;
        if pending.created.elapsed() >= CODE_TTL {
            return Err(invalid);
        }
        if req
            .client_id
            .as_ref()
            .is_some_and(|c| *c != pending.client_id)
            || req
                .redirect_uri
                .as_ref()
                .is_some_and(|r| *r != pending.redirect_uri)
        {
            return Err(("invalid_grant", "client or redirect_uri mismatch"));
        }
        let verifier = req
            .code_verifier
            .as_deref()
            .ok_or(("invalid_request", "missing code_verifier"))?;
        if URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) != pending.challenge {
            return Err(("invalid_grant", "code_verifier mismatch"));
        }
        // From now on the client is legitimate and is never pruned.
        self.state
            .mark_client_authorized(&pending.client_id)
            .map_err(|_| ("server_error", "internal error"))?;
        Ok(pending.client_id)
    }

    fn refresh(&self, req: &TokenRequest) -> Result<String, OAuthError> {
        let invalid = ("invalid_grant", "invalid or expired refresh token");
        let token = req.refresh_token.as_deref().ok_or(invalid)?;
        // Rotating: a used refresh token stops working.
        let client = self
            .state
            .take_token(&hash(token), "refresh", now())
            .map_err(|_| ("server_error", "internal error"))?
            .ok_or(invalid)?;
        if req.client_id.as_ref().is_some_and(|c| *c != client) {
            return Err(invalid);
        }
        Ok(client)
    }

    fn issue(&self, client_id: &str) -> anyhow::Result<Value> {
        let now = now();
        self.state.purge_expired_tokens(now)?;
        let access = random_token();
        let refresh = random_token();
        self.state.add_token(
            &hash(&access),
            "access",
            client_id,
            now + ACCESS_TTL.as_secs() as i64,
        )?;
        self.state.add_token(
            &hash(&refresh),
            "refresh",
            client_id,
            now + REFRESH_TTL.as_secs() as i64,
        )?;
        Ok(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL.as_secs(),
            "refresh_token": refresh,
        }))
    }
}

/// Sliding-window counter per client IP.
struct Throttle {
    max: usize,
    window: Duration,
    hits: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

impl Throttle {
    fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            hits: Mutex::default(),
        }
    }

    /// Whether `ip` has used up its allowance in the current window.
    fn exceeded(&self, ip: IpAddr) -> bool {
        let mut hits = lock(&self.hits);
        hits.retain(|_, times| {
            times.retain(|t| t.elapsed() < self.window);
            !times.is_empty()
        });
        hits.get(&ip).is_some_and(|times| times.len() >= self.max)
    }

    fn record(&self, ip: IpAddr) {
        lock(&self.hits).entry(ip).or_default().push(Instant::now());
    }
}

/// The client's IP. Behind a reverse proxy on a private network (Traefik,
/// Caddy, nginx) every request comes from the proxy, so the last hop the proxy
/// appended to `X-Forwarded-For` is used instead. A client connecting directly
/// from a public address cannot spoof this, because the header is ignored then.
fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    let ip = peer.ip();
    if !is_private(ip) {
        return ip;
    }
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(ip)
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local, fc00::/7
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_private() || v4.is_loopback())
        }
    }
}

/// Changing `MCP_PASSWORD` revokes every OAuth grant on the next start.
/// The state keeps only `salt:sha256(salt:password)` to compare against.
fn revoke_if_password_changed(state: &State, password: &str) -> anyhow::Result<()> {
    const KEY: &str = "oauth_password";
    let stored = state.get(KEY)?;
    let unchanged = stored
        .as_deref()
        .and_then(|s| s.split_once(':'))
        .is_some_and(|(salt, h)| hash(&format!("{salt}:{password}")) == h);
    if unchanged {
        return Ok(());
    }
    let revoked = state.clear_tokens()?;
    if stored.is_some() {
        info!(revoked, "MCP_PASSWORD changed; OAuth grants revoked");
    }
    let salt = random_token();
    let h = hash(&format!("{salt}:{password}"));
    state.set(KEY, &format!("{salt}:{h}"))
}

/// Discovery and OAuth flow routes. Empty without `MCP_PASSWORD`.
pub fn routes(auth: Arc<Auth>) -> Router {
    if !auth.oauth_enabled() {
        return Router::new();
    }
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route("/oauth/register", post(register))
        .route(
            "/oauth/authorize",
            get(authorize_page).post(authorize_submit),
        )
        .route("/oauth/token", post(token))
        .with_state(auth)
}

/// `/mcp` middleware: without a valid bearer token, 401 pointing to OAuth discovery.
pub async fn require_bearer(
    AxState(auth): AxState<Arc<Auth>>,
    req: Request,
    next: Next,
) -> Response {
    if auth.authorized(req.headers()) {
        return next.run(req).await;
    }
    let challenge = if auth.oauth_enabled() {
        format!(
            "Bearer resource_metadata=\"{}\"",
            auth.resource_metadata_url()
        )
    } else {
        "Bearer".to_string()
    };
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
        Json(json!({ "error": "invalid_token" })),
    )
        .into_response()
}

async fn resource_metadata(AxState(auth): AxState<Arc<Auth>>) -> Json<Value> {
    Json(json!({
        "resource": format!("{}/mcp", auth.public_url),
        "authorization_servers": [auth.public_url],
        "bearer_methods_supported": ["header"],
        "resource_name": "vault-sync",
    }))
}

async fn server_metadata(AxState(auth): AxState<Arc<Auth>>) -> Json<Value> {
    let base = &auth.public_url;
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

#[derive(Deserialize)]
struct RegisterRequest {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
}

async fn register(
    AxState(auth): AxState<Arc<Auth>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> Response {
    let ip = client_ip(peer, &headers);
    if auth.registrations.exceeded(ip) {
        warn!(%ip, "too many OAuth client registrations");
        return oauth_error(
            StatusCode::TOO_MANY_REQUESTS,
            "invalid_client_metadata",
            "too many registrations, try again later",
        );
    }
    if req.redirect_uris.is_empty()
        || req.redirect_uris.len() > 10
        || !req.redirect_uris.iter().all(|u| valid_redirect(u))
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect_uris must be https:// or http://localhost",
        );
    }
    let cutoff = now() - UNAUTHORIZED_CLIENT_TTL.as_secs() as i64;
    if let Err(e) = auth.state.prune_unauthorized_clients(cutoff) {
        return internal(e);
    }
    match auth.state.client_count() {
        Ok(n) if n < MAX_CLIENTS => {}
        Ok(_) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                "client limit reached",
            )
        }
        Err(e) => return internal(e),
    }
    let client = OAuthClient {
        id: random_token(),
        name: req
            .client_name
            .map(|n| n.chars().take(100).collect())
            .unwrap_or_else(|| "MCP client".to_string()),
        redirect_uris: req.redirect_uris,
    };
    if let Err(e) = auth.state.add_client(&client, now()) {
        return internal(e);
    }
    auth.registrations.record(ip);
    info!(client = %client.name, "OAuth client registered");
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": client.id,
            "client_name": client.name,
            "redirect_uris": client.redirect_uris,
            "client_id_issued_at": now(),
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        })),
    )
        .into_response()
}

/// `/oauth/authorize` parameters; the form POST also carries the password.
#[derive(Deserialize)]
struct AuthorizeParams {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    state: Option<String>,
    #[serde(default)]
    password: String,
}

async fn authorize_page(
    AxState(auth): AxState<Arc<Auth>>,
    Query(p): Query<AuthorizeParams>,
) -> Response {
    match auth.validate(&p) {
        Ok(client) => form_page(StatusCode::OK, &client, &p, None),
        Err(msg) => error_page(msg),
    }
}

async fn authorize_submit(
    AxState(auth): AxState<Arc<Auth>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(p): Form<AuthorizeParams>,
) -> Response {
    let client = match auth.validate(&p) {
        Ok(c) => c,
        Err(msg) => return error_page(msg),
    };
    let ip = client_ip(peer, &headers);
    if auth.failures.exceeded(ip) {
        let msg = "Too many wrong attempts. Try again in 15 minutes.";
        return form_page(StatusCode::TOO_MANY_REQUESTS, &client, &p, Some(msg));
    }
    if !auth.check_password(&p.password) {
        auth.failures.record(ip);
        warn!(client = %client.name, %ip, "wrong password on OAuth authorization");
        tokio::time::sleep(FAILURE_DELAY).await;
        return form_page(
            StatusCode::UNAUTHORIZED,
            &client,
            &p,
            Some("Wrong password."),
        );
    }

    let code = auth.new_code(&p);
    let Ok(mut target) = Url::parse(&p.redirect_uri) else {
        return error_page("Invalid redirect address.");
    };
    {
        let mut query = target.query_pairs_mut();
        query.append_pair("code", &code);
        if let Some(state) = &p.state {
            query.append_pair("state", state);
        }
    }
    info!(client = %client.name, "vault access authorized");
    Redirect::to(target.as_str()).into_response()
}

#[derive(Deserialize)]
struct TokenRequest {
    #[serde(default)]
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

async fn token(
    AxState(auth): AxState<Arc<Auth>>,
    headers: HeaderMap,
    Form(mut req): Form<TokenRequest>,
) -> Response {
    // Some clients send the client_id through HTTP Basic even without a secret.
    if req.client_id.is_none() {
        req.client_id = basic_user(&headers);
    }
    let client = match req.grant_type.as_str() {
        "authorization_code" => auth.exchange_code(&req),
        "refresh_token" => auth.refresh(&req),
        _ => Err((
            "unsupported_grant_type",
            "use authorization_code or refresh_token",
        )),
    };
    match client.map(|c| auth.issue(&c)) {
        Ok(Ok(body)) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(body),
        )
            .into_response(),
        Ok(Err(e)) => internal(e),
        Err((code, description)) => oauth_error(StatusCode::BAD_REQUEST, code, description),
    }
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

fn internal(e: anyhow::Error) -> Response {
    warn!(error = %format!("{e:#}"), "OAuth error");
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "internal error",
    )
}

fn form_page(
    status: StatusCode,
    client: &OAuthClient,
    p: &AuthorizeParams,
    error: Option<&str>,
) -> Response {
    let host = Url::parse(&p.redirect_uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let mut hidden = String::new();
    for (name, value) in [
        ("response_type", p.response_type.as_str()),
        ("client_id", &p.client_id),
        ("redirect_uri", &p.redirect_uri),
        ("code_challenge", &p.code_challenge),
        ("code_challenge_method", &p.code_challenge_method),
        ("state", p.state.as_deref().unwrap_or_default()),
    ] {
        hidden.push_str(&format!(
            r#"<input type="hidden" name="{name}" value="{}">"#,
            escape(value)
        ));
    }
    let error = error
        .map(|e| format!(r#"<p class="error" role="alert">{}</p>"#, escape(e)))
        .unwrap_or_default();
    let body = format!(
        r#"<h1>Authorize vault access</h1>
<p><strong>{client}</strong> wants to read the notes in your Obsidian vault.</p>
<p class="muted">After authorizing, you go back to <code>{host}</code>.</p>
{error}
<form method="post" action="/oauth/authorize">
{hidden}
<label for="password">Password</label>
<input id="password" name="password" type="password" autocomplete="current-password" required autofocus>
<button type="submit">Authorize</button>
</form>"#,
        client = escape(&client.name),
        host = escape(&host),
    );
    html(status, &body)
}

fn error_page(message: &str) -> Response {
    let body = format!(
        r#"<h1>Authorization failed</h1><p class="error">{}</p>"#,
        escape(message)
    );
    html(StatusCode::BAD_REQUEST, &body)
}

fn html(status: StatusCode, body: &str) -> Response {
    let page = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>vault-sync</title>
<style>
:root {{ color-scheme: light dark; --bg: #f6f6f4; --card: #fff; --text: #1d1d1b; --muted: #6b6b66; --line: #d9d9d4; --accent: #5b4bdb; --error: #b42318; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg: #161615; --card: #20201e; --text: #ecece8; --muted: #a3a39c; --line: #3a3a36; --accent: #9d93ff; --error: #ff8a7a; }} }}
body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: var(--bg); color: var(--text); font: 16px/1.5 system-ui, sans-serif; }}
main {{ width: min(400px, calc(100vw - 32px)); background: var(--card); border: 1px solid var(--line); border-radius: 12px; padding: 28px; box-sizing: border-box; }}
h1 {{ font-size: 1.25rem; margin: 0 0 12px; }}
.muted {{ color: var(--muted); font-size: .9rem; }}
.error {{ color: var(--error); }}
label {{ display: block; margin-top: 20px; font-weight: 600; }}
input[type=password] {{ width: 100%; box-sizing: border-box; margin-top: 6px; padding: 10px 12px; font: inherit; color: inherit; background: transparent; border: 1px solid var(--line); border-radius: 8px; }}
button {{ margin-top: 16px; width: 100%; padding: 10px; font: inherit; font-weight: 600; color: #fff; background: var(--accent); border: 0; border-radius: 8px; cursor: pointer; }}
</style>
</head>
<body><main>
{body}
</main></body>
</html>"#
    );
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::REFERRER_POLICY, "no-referrer"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
        ],
        Html(page),
    )
        .into_response()
}

fn valid_redirect(uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    if url.fragment().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => url.host_str().is_some(),
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

fn basic_user(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(B64.decode(encoded.trim()).ok()?).ok()?;
    let user = decoded.split(':').next()?;
    (!user.is_empty()).then(|| user.to_string())
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    secure_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Tokens and salts must come from a cryptographically secure generator. The
/// `CryptoRng` bound makes a `rand` update that weakens `rng()` fail to compile.
fn secure_rng() -> impl CryptoRng {
    rand::rng()
}

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// Hex hash: what goes into the database instead of the token.
fn hash(s: &str) -> String {
    digest(s).iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time comparison (over hashes, which have the same length).
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changing_the_password_revokes_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(State::open(&dir.path().join("s.db")).unwrap());
        let cfg = |password: &str| McpConfig {
            addr: String::new(),
            public_url: "https://x".into(),
            token: None,
            password: Some(password.into()),
        };
        Auth::new(&cfg("first-long-password"), state.clone()).unwrap();
        state.add_token("h", "access", "c", i64::MAX).unwrap();
        Auth::new(&cfg("first-long-password"), state.clone()).unwrap();
        assert!(state.token_client("h", "access", 0).unwrap().is_some());
        Auth::new(&cfg("second-long-password"), state.clone()).unwrap();
        assert!(state.token_client("h", "access", 0).unwrap().is_none());
    }

    #[test]
    fn client_ip_trusts_forwarded_for_only_from_private_peers() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "9.9.9.9, 1.2.3.4".parse().unwrap());
        let proxy: SocketAddr = "172.18.0.5:40000".parse().unwrap();
        let public: SocketAddr = "8.8.8.8:40000".parse().unwrap();
        // Behind a proxy: the last hop the proxy appended.
        assert_eq!(client_ip(proxy, &h), "1.2.3.4".parse::<IpAddr>().unwrap());
        // Straight from the internet: the header could be forged, so it is ignored.
        assert_eq!(client_ip(public, &h), public.ip());
        // Proxy without the header: its own address.
        assert_eq!(client_ip(proxy, &HeaderMap::new()), proxy.ip());
        assert!(is_private("fd00::1".parse().unwrap()));
        assert!(is_private("::ffff:10.0.0.1".parse().unwrap()));
        assert!(!is_private("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn throttle_is_per_ip() {
        let t = Throttle::new(2, Duration::from_secs(60));
        let a: IpAddr = "1.1.1.1".parse().unwrap();
        let b: IpAddr = "2.2.2.2".parse().unwrap();
        t.record(a);
        assert!(!t.exceeded(a));
        t.record(a);
        assert!(t.exceeded(a));
        assert!(!t.exceeded(b));
    }

    #[test]
    fn stored_hash_format_is_stable() {
        // Tokens and the password fingerprint are stored as this hex SHA-256. If
        // a dependency update changed it, every issued token would silently stop
        // working. Official test vector for "abc".
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(random_token().len(), 43); // 32 bytes, base64url without padding
    }

    #[test]
    fn accepted_redirects() {
        assert!(valid_redirect("https://claude.ai/api/mcp/auth_callback"));
        assert!(valid_redirect("http://localhost:33418/callback"));
        assert!(valid_redirect("http://127.0.0.1/cb"));
        assert!(!valid_redirect("http://example.com/cb"));
        assert!(!valid_redirect("https://claude.ai/cb#frag"));
        assert!(!valid_redirect("javascript:alert(1)"));
        assert!(!valid_redirect("not a url"));
    }

    #[test]
    fn headers_and_escaping() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "bearer abc ".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, "Basic Y2xpZW50ZTo=".parse().unwrap());
        assert_eq!(bearer(&h), None);
        assert_eq!(basic_user(&h).as_deref(), Some("cliente"));
        assert_eq!(
            escape(r#"<a href="x">'&"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;"
        );
        assert!(ct_eq(&digest("x"), &digest("x")));
        assert!(!ct_eq(&digest("x"), &digest("y")));
    }
}
