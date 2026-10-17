//! MCP server (Streamable HTTP) with read-only vault tools.
//!
//! It listens on its own port, separate from the internal API, and only
//! answers requests with a valid bearer token (`MCP_TOKEN` or one issued by
//! OAuth). It is stateless: every request stands alone, so a restart does not
//! drop clients.

use std::fmt::Write as _;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{middleware, Router};
use reqwest::Url;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::catalog::{Catalog, Filter, NoteDetail, NoteMeta, SearchHit, TagCount};
use crate::config::McpConfig;
use crate::oauth::{self, Auth};
use crate::state::State;

const INSTRUCTIONS: &str = "Read-only access to the user's Obsidian vault, mirrored on \
their own server. Use search_notes to find notes by content (accent-insensitive, with \
Portuguese stemming), read_note to read a whole note with its links and backlinks, \
list_notes for the most recent notes or those under a tag or folder, and list_tags to \
see the tags. Refer to notes by their path.";

/// Builds the public routes: the protected `/mcp` endpoint and the OAuth routes.
pub fn router(cfg: &McpConfig, catalog: Arc<Catalog>, state: Arc<State>) -> Result<Router> {
    let url = Url::parse(&cfg.public_url).context("invalid MCP_PUBLIC_URL")?;
    let host = url.host_str().context("MCP_PUBLIC_URL has no host")?;
    // DNS rebinding protection: only the public hostname and the names used
    // inside the host (tests and health checks) are accepted.
    let mut hosts = vec![
        host.to_string(),
        "vault-core".into(),
        "localhost".into(),
        "127.0.0.1".into(),
    ];
    if let Some(port) = url.port() {
        hosts.push(format!("{host}:{port}"));
    }
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(hosts);
    let service: StreamableHttpService<VaultMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(VaultMcp::new(catalog.clone())),
        Default::default(),
        config,
    );

    let auth = Arc::new(Auth::new(cfg, state)?);
    let mcp = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(
            auth.clone(),
            oauth::require_bearer,
        ));
    Ok(mcp.merge(oauth::routes(auth)))
}

/// Serves the public routes. OAuth rate limiting needs each connection's
/// address, hence `connect_info`.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

#[derive(Clone)]
pub struct VaultMcp {
    catalog: Arc<Catalog>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Text to search for. Accent- and case-insensitive; terms are combined
    /// with AND. Supports "exact phrase", -term to exclude and title:word.
    pub query: String,
    /// Only notes with this tag, without `#`. Includes subtags: `project` matches `project/vault`.
    pub tag: Option<String>,
    /// Only notes inside this folder, e.g. `Work/Projects`.
    pub folder: Option<String>,
    /// Maximum number of results (default 10, up to 50).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadArgs {
    /// Note path (`Folder/Note.md`) or its name as in a [[link]] (`Note`).
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListArgs {
    /// Only notes with this tag, without `#`. Includes subtags.
    pub tag: Option<String>,
    /// Only notes inside this folder.
    pub folder: Option<String>,
    /// Maximum number of notes (default 20, up to 200).
    pub limit: Option<usize>,
}

#[tool_router]
impl VaultMcp {
    pub fn new(catalog: Arc<Catalog>) -> Self {
        Self {
            catalog,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Searches vault notes by content and title. Returns path, title, tags and a snippet with the matched terms.",
        annotations(read_only_hint = true)
    )]
    async fn search_notes(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<String, String> {
        if args.query.trim().is_empty() {
            return Err("query must not be empty".into());
        }
        let limit = args.limit.unwrap_or(10).clamp(1, 50);
        let filter = Filter {
            tag: args.tag,
            folder: args.folder,
        };
        let catalog = self.catalog.clone();
        let query = args.query.clone();
        let hits = blocking(move || catalog.search(&query, &filter, limit)).await?;
        Ok(format_search(&args.query, &hits))
    }

    #[tool(
        description = "Reads a whole note: frontmatter, tags, links (flagging broken ones), backlinks and the Markdown content.",
        annotations(read_only_hint = true)
    )]
    async fn read_note(&self, Parameters(args): Parameters<ReadArgs>) -> Result<String, String> {
        let catalog = self.catalog.clone();
        let path = args.path.clone();
        match blocking(move || catalog.note(&path)).await? {
            Some(note) => Ok(format_note(&note)),
            None => Err(format!(
                "Note not found: {}. Use search_notes to find its path.",
                args.path
            )),
        }
    }

    #[tool(
        description = "Lists notes from most to least recently modified, optionally filtered by tag or folder.",
        annotations(read_only_hint = true)
    )]
    async fn list_notes(&self, Parameters(args): Parameters<ListArgs>) -> String {
        let filter = Filter {
            tag: args.tag,
            folder: args.folder,
        };
        let notes = self
            .catalog
            .list(&filter, args.limit.unwrap_or(20).clamp(1, 200));
        format_list(&notes)
    }

    #[tool(
        description = "Lists the vault's tags and how many notes use each one.",
        annotations(read_only_hint = true)
    )]
    async fn list_tags(&self) -> String {
        format_tags(&self.catalog.tags())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for VaultMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("vault-sync", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// The catalog is synchronous (tantivy and disk I/O): keep it off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("task aborted: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

fn format_search(query: &str, hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return format!("No notes found for \"{query}\".");
    }
    let mut out = format!("{} note(s) for \"{query}\":\n", hits.len());
    for (i, hit) in hits.iter().enumerate() {
        let _ = write!(out, "\n{}. {} — `{}`", i + 1, hit.title, hit.path);
        if !hit.tags.is_empty() {
            let _ = write!(out, " {}", hashtags(&hit.tags));
        }
        let _ = writeln!(out, "\n   {}", hit.snippet);
    }
    out
}

fn format_note(note: &NoteDetail) -> String {
    let meta = &note.meta;
    let mut out = format!("Path: {}\nTitle: {}\n", meta.path, meta.title);
    if let Some(mtime) = meta.mtime {
        let _ = writeln!(out, "Modified: {}", date(mtime));
    }
    if !meta.tags.is_empty() {
        let _ = writeln!(out, "Tags: {}", hashtags(&meta.tags));
    }
    if !meta.aliases.is_empty() {
        let _ = writeln!(out, "Aliases: {}", meta.aliases.join(", "));
    }
    if !note.links.is_empty() {
        let links: Vec<String> = note
            .links
            .iter()
            .map(|l| match &l.path {
                Some(p) if *p == l.target => format!("[[{}]]", l.target),
                Some(p) => format!("[[{}]] → {p}", l.target),
                None => format!("[[{}]] (broken)", l.target),
            })
            .collect();
        let _ = writeln!(out, "Links: {}", links.join(", "));
    }
    if !note.backlinks.is_empty() {
        let _ = writeln!(out, "Backlinks: {}", note.backlinks.join(", "));
    }
    let _ = write!(out, "\n---\n\n{}", note.content);
    out
}

fn format_list(notes: &[NoteMeta]) -> String {
    if notes.is_empty() {
        return "No notes found.".into();
    }
    let mut out = String::new();
    for n in notes {
        let when = n.mtime.map(date).unwrap_or_else(|| "----------".into());
        let _ = write!(out, "- {when} `{}` — {}", n.path, n.title);
        if !n.tags.is_empty() {
            let _ = write!(out, " {}", hashtags(&n.tags));
        }
        out.push('\n');
    }
    out
}

fn format_tags(tags: &[TagCount]) -> String {
    if tags.is_empty() {
        return "The vault has no tags.".into();
    }
    tags.iter()
        .map(|t| format!("- #{} ({})\n", t.tag, t.count))
        .collect()
}

fn hashtags(tags: &[String]) -> String {
    tags.iter()
        .map(|t| format!("#{t}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// UTC date of an instant in ms since the Unix epoch, without a date crate.
fn date(ms: i64) -> String {
    // Howard Hinnant's civil_from_days algorithm.
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};

    use super::*;

    const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";
    const PASSWORD: &str = "a-long-test-password";
    const TOKEN: &str = "a-long-static-test-token";

    /// Starts the real MCP server on a local port, with a tiny vault.
    async fn spawn(token: bool, password: bool) -> (String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(vault.join("Work")).unwrap();
        std::fs::write(
            vault.join("Work/Roadmap.md"),
            "# Roadmap\nThe café meeting about [[Ideas]].",
        )
        .unwrap();
        std::fs::write(vault.join("Ideas.md"), "List of ideas #project").unwrap();
        let catalog = Arc::new(Catalog::new(vault).unwrap());
        catalog.rebuild().unwrap();
        let state = Arc::new(State::open(&dir.path().join("state.db")).unwrap());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = McpConfig {
            addr: String::new(),
            public_url: format!("http://localhost:{port}"),
            token: token.then(|| TOKEN.to_string()),
            password: password.then(|| PASSWORD.to_string()),
        };
        let app = router(&cfg, catalog, state).unwrap();
        tokio::spawn(serve(listener, app, std::future::pending()));
        (format!("http://127.0.0.1:{port}"), dir)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    /// JSON-RPC call to `/mcp`. Accepts a JSON or an SSE response.
    async fn rpc(base: &str, bearer: Option<&str>, method: &str, params: Value) -> (u16, Value) {
        let mut req = client()
            .post(format!("{base}/mcp"))
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-06-18")
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        let json_text = text
            .lines()
            .find_map(|l| l.strip_prefix("data:"))
            .unwrap_or(&text)
            .trim()
            .to_string();
        (
            status,
            serde_json::from_str(&json_text).unwrap_or(Value::String(text)),
        )
    }

    async fn initialize(base: &str, bearer: &str) -> Value {
        let params = json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1" }
        });
        let (status, body) = rpc(base, Some(bearer), "initialize", params).await;
        assert_eq!(status, 200, "{body}");
        body
    }

    async fn call_tool(base: &str, bearer: &str, name: &str, args: Value) -> (bool, String) {
        let params = json!({ "name": name, "arguments": args });
        let (status, body) = rpc(base, Some(bearer), "tools/call", params).await;
        assert_eq!(status, 200, "{body}");
        let result = &body["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        (result["isError"] == json!(true), text.to_string())
    }

    #[tokio::test]
    async fn tools_with_static_token() {
        let (base, _dir) = spawn(true, false).await;

        let (status, _) = rpc(&base, None, "initialize", json!({})).await;
        assert_eq!(status, 401);
        let (status, _) = rpc(&base, Some("wrong"), "initialize", json!({})).await;
        assert_eq!(status, 401);
        // Without a password there is no OAuth: the routes don't exist.
        let resp = client()
            .get(format!("{base}/.well-known/oauth-authorization-server"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let init = initialize(&base, TOKEN).await;
        assert_eq!(init["result"]["serverInfo"]["name"], "vault-sync");

        let (_, tools) = rpc(&base, Some(TOKEN), "tools/list", json!({})).await;
        let mut names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["list_notes", "list_tags", "read_note", "search_notes"]
        );

        let (err, text) = call_tool(&base, TOKEN, "search_notes", json!({ "query": "cafe" })).await;
        assert!(!err);
        assert!(text.contains("`Work/Roadmap.md`"), "{text}");
        assert!(text.contains("**café**"), "{text}");

        let (_, text) = call_tool(&base, TOKEN, "read_note", json!({ "path": "ideas" })).await;
        assert!(text.contains("Backlinks: Work/Roadmap.md"), "{text}");
        assert!(text.contains("List of ideas"), "{text}");

        let (err, text) = call_tool(&base, TOKEN, "read_note", json!({ "path": "nothing" })).await;
        assert!(err, "{text}");

        let (_, text) = call_tool(&base, TOKEN, "list_tags", json!({})).await;
        assert_eq!(text, "- #project (1)\n");
        let (_, text) = call_tool(&base, TOKEN, "list_notes", json!({ "folder": "Work" })).await;
        assert!(text.contains("`Work/Roadmap.md` — Roadmap"), "{text}");
    }

    #[tokio::test]
    async fn full_oauth_flow() {
        let (base, _dir) = spawn(false, true).await;
        let http = client();
        let public = base.replace("127.0.0.1", "localhost");

        // 1. No token: 401 pointing to the resource metadata.
        let resp = http.post(format!("{base}/mcp")).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        let challenge = resp.headers()["www-authenticate"].to_str().unwrap();
        assert_eq!(
            challenge,
            format!(
                "Bearer resource_metadata=\"{public}/.well-known/oauth-protected-resource/mcp\""
            )
        );

        // 2. Discovery.
        let get = |path: &str| http.get(format!("{base}{path}")).send();
        let resource: Value = get("/.well-known/oauth-protected-resource/mcp")
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resource["authorization_servers"][0], public);
        let server: Value = get("/.well-known/oauth-authorization-server")
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(server["token_endpoint"], format!("{public}/oauth/token"));
        assert_eq!(server["code_challenge_methods_supported"][0], "S256");

        // 3. Dynamic registration.
        let register = |uris: Value| {
            http.post(format!("{base}/oauth/register"))
                .json(&json!({ "client_name": "Claude <test>", "redirect_uris": uris }))
                .send()
        };
        let resp = register(json!(["http://example.com/cb"])).await.unwrap();
        assert_eq!(resp.status(), 400);
        let resp = register(json!([REDIRECT])).await.unwrap();
        assert_eq!(resp.status(), 201);
        let client_id = resp.json::<Value>().await.unwrap()["client_id"]
            .as_str()
            .unwrap()
            .to_string();

        // 4. Authorization page.
        let verifier = "v".repeat(64);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let params = |redirect: &str| {
            vec![
                ("response_type", "code".to_string()),
                ("client_id", client_id.clone()),
                ("redirect_uri", redirect.to_string()),
                ("code_challenge", challenge.clone()),
                ("code_challenge_method", "S256".to_string()),
                ("state", "xyz".to_string()),
            ]
        };
        let page = http
            .get(format!("{base}/oauth/authorize"))
            .query(&params(REDIRECT))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        let html = page.text().await.unwrap();
        assert!(
            html.contains("Claude &lt;test&gt;"),
            "client name escaped on the page"
        );
        let bad = http
            .get(format!("{base}/oauth/authorize"))
            .query(&params("https://other.com/cb"))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);

        // The test talks from 127.0.0.1, a private address, so X-Forwarded-For
        // stands in for the client IP the way a reverse proxy would set it.
        let authorize = |password: &str, ip: &str| {
            let mut form = params(REDIRECT);
            form.push(("password", password.to_string()));
            http.post(format!("{base}/oauth/authorize"))
                .header("X-Forwarded-For", ip)
                .form(&form)
                .send()
        };
        let attacker = "203.0.113.7";
        for _ in 0..10 {
            assert_eq!(authorize("wrong", attacker).await.unwrap().status(), 401);
        }
        // The attacker is locked out, even with the right password...
        assert_eq!(authorize(PASSWORD, attacker).await.unwrap().status(), 429);
        let code_of = |resp: reqwest::Response| {
            assert_eq!(resp.status(), 303);
            let location = Url::parse(resp.headers()["location"].to_str().unwrap()).unwrap();
            assert!(location.as_str().starts_with(REDIRECT));
            let pairs: std::collections::HashMap<_, _> =
                location.query_pairs().into_owned().collect();
            assert_eq!(pairs["state"], "xyz");
            pairs["code"].clone()
        };
        // ...but the owner, from another address, is not.
        let owner = "198.51.100.20";

        // 5. Code exchange: a wrong PKCE verifier fails and burns the code.
        let exchange = |code: String, verifier: String| {
            http.post(format!("{base}/oauth/token"))
                .form(&[
                    ("grant_type", "authorization_code".to_string()),
                    ("code", code),
                    ("redirect_uri", REDIRECT.to_string()),
                    ("client_id", client_id.clone()),
                    ("code_verifier", verifier),
                ])
                .send()
        };
        let code = code_of(authorize(PASSWORD, owner).await.unwrap());
        let resp = exchange(code.clone(), "w".repeat(64)).await.unwrap();
        assert_eq!(resp.status(), 400);
        assert_eq!(
            resp.json::<Value>().await.unwrap()["error"],
            "invalid_grant"
        );
        assert_eq!(
            exchange(code, verifier.clone()).await.unwrap().status(),
            400
        );

        let code = code_of(authorize(PASSWORD, owner).await.unwrap());
        let tokens: Value = exchange(code, verifier.clone())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let access = tokens["access_token"].as_str().unwrap();
        assert_eq!(tokens["token_type"], "Bearer");
        initialize(&base, access).await;

        // 6. Rotating refresh: the old one stops working.
        let refresh = |token: &str| {
            http.post(format!("{base}/oauth/token"))
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", token),
                    ("client_id", &client_id),
                ])
                .send()
        };
        let old = tokens["refresh_token"].as_str().unwrap();
        let renewed: Value = refresh(old).await.unwrap().json().await.unwrap();
        assert!(renewed["access_token"].is_string());
        assert_eq!(refresh(old).await.unwrap().status(), 400);
        initialize(&base, renewed["access_token"].as_str().unwrap()).await;
    }

    #[tokio::test]
    async fn registration_is_rate_limited_per_ip() {
        let (base, _dir) = spawn(false, true).await;
        let register = |ip: &'static str| {
            client()
                .post(format!("{base}/oauth/register"))
                .header("X-Forwarded-For", ip)
                .json(&json!({ "redirect_uris": [REDIRECT] }))
                .send()
        };
        for _ in 0..20 {
            assert_eq!(register("203.0.113.9").await.unwrap().status(), 201);
        }
        assert_eq!(register("203.0.113.9").await.unwrap().status(), 429);
        assert_eq!(register("198.51.100.20").await.unwrap().status(), 201);
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_760_000_000_000), "2025-10-09");
        assert_eq!(date(951_782_400_000), "2000-02-29");
        assert_eq!(date(-86_400_000), "1969-12-31");
    }
}
