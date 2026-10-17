//! MCP server with read-only vault tools, for any MCP client.
//!
//! It listens on its own port, separate from the internal API, and only
//! answers requests with a valid bearer token (`MCP_TOKEN` or one issued by
//! OAuth). The main endpoint, `/mcp`, speaks Streamable HTTP and is
//! stateless: every request stands alone, so a restart does not drop clients.
//! Older clients can use the legacy HTTP+SSE transport at `/mcp/sse`.
//!
//! Each tool returns Markdown text for the model and the same data as JSON
//! (`structuredContent`, described by an `outputSchema`) for programs. The
//! schemas avoid JSON Schema features some model APIs reject.

use std::fmt::Write as _;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use std::time::Duration;

use axum::http::{header, HeaderName, Method};
use axum::{middleware, Router};
use reqwest::Url;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::schema_for_output;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, JsonObject, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};

use crate::catalog::{Catalog, Filter, NoteDetail, NoteMeta, SearchHit, TagCount};
use crate::config::McpConfig;
use crate::oauth::{self, Auth};
use crate::sse;
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
        .with_allowed_hosts(hosts.clone());
    let streamable = catalog.clone();
    let service: StreamableHttpService<VaultMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(VaultMcp::new(streamable.clone())),
        Default::default(),
        config,
    );

    let auth = Arc::new(Auth::new(cfg, state)?);
    let mcp = Router::new()
        .route_service("/mcp", service.clone())
        .route_service("/mcp/", service)
        .merge(sse::routes(catalog, hosts))
        .layer(middleware::from_fn_with_state(
            auth.clone(),
            oauth::require_bearer,
        ));
    Ok(mcp.merge(oauth::routes(auth)).layer(cors()))
}

/// Browser-based clients (the MCP Inspector, web apps) call the endpoints
/// from another origin. Any origin may: no cookies are involved, and every
/// request needs a bearer token or a credential only the user knows.
fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            HeaderName::from_static("mcp-protocol-version"),
            HeaderName::from_static("mcp-session-id"),
            HeaderName::from_static("last-event-id"),
        ])
        .expose_headers([
            header::WWW_AUTHENTICATE,
            HeaderName::from_static("mcp-session-id"),
        ])
        .max_age(Duration::from_secs(24 * 60 * 60))
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

/// `search_notes` result.
#[derive(Serialize, schemars::JsonSchema)]
pub struct SearchResults {
    pub query: String,
    /// Best matches first.
    pub results: Vec<SearchHit>,
}

/// `list_notes` result.
#[derive(Serialize, schemars::JsonSchema)]
pub struct NoteList {
    /// Most recently modified first.
    pub notes: Vec<NoteMeta>,
}

/// `list_tags` result.
#[derive(Serialize, schemars::JsonSchema)]
pub struct TagList {
    /// Most used first.
    pub tags: Vec<TagCount>,
}

#[tool_router]
impl VaultMcp {
    pub fn new(catalog: Arc<Catalog>) -> Self {
        let mut tool_router = Self::tool_router();
        for route in tool_router.map.values_mut() {
            route.attr.input_schema = Arc::new(portable_input(&route.attr.input_schema));
            if let Some(output) = &route.attr.output_schema {
                route.attr.output_schema = Some(Arc::new(portable(output)));
            }
        }
        Self {
            catalog,
            tool_router,
        }
    }

    #[tool(
        description = "Searches vault notes by content and title. Returns path, title, tags and a snippet with the matched terms.",
        annotations(read_only_hint = true),
        output_schema = schema_for_output::<SearchResults>()
    )]
    async fn search_notes(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, String> {
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
        let text = format_search(&args.query, &hits);
        structured(
            text,
            &SearchResults {
                query: args.query,
                results: hits,
            },
        )
    }

    #[tool(
        description = "Reads a whole note: frontmatter, tags, links (flagging broken ones), backlinks and the Markdown content.",
        annotations(read_only_hint = true),
        output_schema = schema_for_output::<NoteDetail>()
    )]
    async fn read_note(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, String> {
        let catalog = self.catalog.clone();
        let path = args.path.clone();
        match blocking(move || catalog.note(&path)).await? {
            Some(note) => structured(format_note(&note), &note),
            None => Err(format!(
                "Note not found: {}. Use search_notes to find its path.",
                args.path
            )),
        }
    }

    #[tool(
        description = "Lists notes from most to least recently modified, optionally filtered by tag or folder.",
        annotations(read_only_hint = true),
        output_schema = schema_for_output::<NoteList>()
    )]
    async fn list_notes(
        &self,
        Parameters(args): Parameters<ListArgs>,
    ) -> Result<CallToolResult, String> {
        let filter = Filter {
            tag: args.tag,
            folder: args.folder,
        };
        let notes = self
            .catalog
            .list(&filter, args.limit.unwrap_or(20).clamp(1, 200));
        structured(format_list(&notes), &NoteList { notes })
    }

    #[tool(
        description = "Lists the vault's tags and how many notes use each one.",
        annotations(read_only_hint = true),
        output_schema = schema_for_output::<TagList>()
    )]
    async fn list_tags(&self) -> Result<CallToolResult, String> {
        let tags = self.catalog.tags();
        structured(format_tags(&tags), &TagList { tags })
    }
}

/// A tool result with both forms: Markdown for the model in `content`, and
/// the data as JSON in `structuredContent`, matching the tool's output schema.
fn structured(text: String, data: &impl Serialize) -> Result<CallToolResult, String> {
    let value = serde_json::to_value(data).map_err(|e| format!("serialization failed: {e}"))?;
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(value);
    Ok(result)
}

/// An input schema every major model API accepts. Optional arguments become
/// plain types: they are already optional by being absent from `required`, and
/// `"type": ["string", "null"]` is rejected by some APIs (Gemini among them).
fn portable_input(schema: &JsonObject) -> JsonObject {
    let mut schema = portable(schema);
    if let Some(Value::Object(props)) = schema.get_mut("properties") {
        for prop in props.values_mut() {
            if let Some(Value::Array(types)) = prop.get("type") {
                let types: Vec<Value> = types.iter().filter(|t| *t != "null").cloned().collect();
                if let [only] = types.as_slice() {
                    prop["type"] = only.clone();
                }
            }
        }
    }
    schema
}

/// Drops what strict validators and model APIs choke on: the `$schema`
/// declaration, the non-standard numeric formats schemars emits (`uint`,
/// `int64`, `float`...) and boolean schemas for properties, which the official
/// TypeScript SDK rejects. Ranges stay in `minimum`/`maximum`.
fn portable(schema: &JsonObject) -> JsonObject {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("$schema");
                if let Some(Value::Object(props)) = map.get_mut("properties") {
                    for prop in props.values_mut() {
                        match prop {
                            Value::Bool(true) => *prop = json!({}),
                            Value::Bool(false) => *prop = json!({ "not": {} }),
                            _ => {}
                        }
                    }
                }
                let numeric = map.get("format").and_then(Value::as_str).is_some_and(|f| {
                    f.starts_with("int") || f.starts_with("uint") || f == "float" || f == "double"
                });
                if numeric {
                    map.remove("format");
                }
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut value = Value::Object(schema.clone());
    strip(&mut value);
    match value {
        Value::Object(map) => map,
        _ => unreachable!("an object stays an object"),
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

    const REDIRECT: &str = "https://client.example/oauth/callback";
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

        // Every tool also returns JSON, described by its output schema.
        let (_, body) = rpc(
            &base,
            Some(TOKEN),
            "tools/call",
            json!({ "name": "search_notes", "arguments": { "query": "cafe" } }),
        )
        .await;
        let data = &body["result"]["structuredContent"];
        assert_eq!(data["query"], "cafe");
        assert_eq!(data["results"][0]["path"], "Work/Roadmap.md");
        let (_, body) = rpc(
            &base,
            Some(TOKEN),
            "tools/call",
            json!({ "name": "read_note", "arguments": { "path": "Ideas" } }),
        )
        .await;
        let data = &body["result"]["structuredContent"];
        assert_eq!(data["path"], "Ideas.md");
        assert_eq!(data["backlinks"], json!(["Work/Roadmap.md"]));
        assert_eq!(data["tags"], json!(["project"]));

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
                .json(&json!({ "client_name": "Client <test>", "redirect_uris": uris }))
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
            html.contains("Client &lt;test&gt;"),
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
    async fn schemas_are_portable() {
        let (base, _dir) = spawn(true, false).await;
        let (_, body) = rpc(&base, Some(TOKEN), "tools/list", json!({})).await;
        let tools = body["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);
        for tool in tools {
            let name = &tool["name"];
            assert!(
                tool["outputSchema"].is_object(),
                "{name} has no output schema"
            );
            let input = tool["inputSchema"].to_string();
            assert!(!input.contains("$schema"), "{name}: {input}");
            assert!(!input.contains("\"uint\""), "{name}: {input}");
            for (arg, prop) in tool["inputSchema"]["properties"].as_object().unwrap() {
                assert!(prop["type"].is_string(), "{name}.{arg}: {prop}");
            }
            let output = tool["outputSchema"].to_string();
            assert!(!output.contains("\"format\":\"uint"), "{name}: {output}");
            for schema in [&tool["inputSchema"], &tool["outputSchema"]] {
                for (prop, s) in schema["properties"].as_object().unwrap() {
                    assert!(s.is_object(), "{name}.{prop}: boolean schema");
                }
            }
        }
        let search = tools.iter().find(|t| t["name"] == "search_notes").unwrap();
        assert_eq!(
            search["inputSchema"]["properties"]["limit"]["type"],
            "integer"
        );
        assert_eq!(search["inputSchema"]["required"], json!(["query"]));
    }

    #[tokio::test]
    async fn browser_clients_get_cors() {
        let (base, _dir) = spawn(true, true).await;
        let preflight = client()
            .request(reqwest::Method::OPTIONS, format!("{base}/mcp"))
            .header("Origin", "http://localhost:6274")
            .header("Access-Control-Request-Method", "POST")
            .header(
                "Access-Control-Request-Headers",
                "authorization,content-type,mcp-protocol-version",
            )
            .send()
            .await
            .unwrap();
        assert!(preflight.status().is_success(), "{}", preflight.status());
        let h = preflight.headers();
        assert_eq!(h["access-control-allow-origin"], "*");
        assert!(h["access-control-allow-headers"]
            .to_str()
            .unwrap()
            .contains("authorization"));

        // The 401 challenge must be readable from a browser to start OAuth.
        let resp = client()
            .post(format!("{base}/mcp"))
            .header("Origin", "http://localhost:6274")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(resp.headers()["access-control-allow-origin"], "*");
        assert!(resp.headers()["access-control-expose-headers"]
            .to_str()
            .unwrap()
            .contains("www-authenticate"));
    }

    /// Registers a client. Returns its client_id and, if confidential, its secret.
    async fn register_client(
        base: &str,
        redirect: &str,
        method: Option<&str>,
    ) -> (String, Option<String>) {
        let mut meta = json!({ "client_name": "App", "redirect_uris": [redirect] });
        if let Some(m) = method {
            meta["token_endpoint_auth_method"] = json!(m);
        }
        let resp = client()
            .post(format!("{base}/oauth/register"))
            .json(&meta)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let reg: Value = resp.json().await.unwrap();
        assert_eq!(reg["token_endpoint_auth_method"], method.unwrap_or("none"));
        let id = reg["client_id"].as_str().unwrap().to_string();
        (id, reg["client_secret"].as_str().map(str::to_string))
    }

    /// Submits the right password on the authorization page; returns the code.
    async fn authorize(base: &str, client_id: &str, redirect: &str, verifier: &str) -> String {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let resp = client()
            .post(format!("{base}/oauth/authorize"))
            .header("X-Forwarded-For", "198.51.100.30")
            .form(&[
                ("response_type", "code"),
                ("client_id", client_id),
                ("redirect_uri", redirect),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("password", PASSWORD),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 303, "{}", resp.text().await.unwrap());
        let location = resp.headers()["location"].to_str().unwrap();
        assert!(location.starts_with(redirect), "{location}");
        let code = Url::parse(location)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .into_owned();
        code
    }

    #[tokio::test]
    async fn native_app_redirects() {
        let (base, _dir) = spawn(false, true).await;
        let verifier = "n".repeat(64);
        // An app's own scheme, as Cursor uses.
        let scheme = "cursor://anysphere.cursor-retrieval/oauth/vault/callback";
        let (id, _) = register_client(&base, scheme, None).await;
        let code = authorize(&base, &id, scheme, &verifier).await;
        let resp = client()
            .post(format!("{base}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", scheme),
                ("client_id", &id),
                ("code_verifier", &verifier),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // A loopback redirect on a port picked at request time.
        let (id, _) = register_client(&base, "http://127.0.0.1:33418/callback", None).await;
        let code = authorize(&base, &id, "http://127.0.0.1:51004/callback", &verifier).await;
        let resp = client()
            .post(format!("{base}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", "http://127.0.0.1:51004/callback"),
                ("client_id", &id),
                ("code_verifier", &verifier),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn confidential_clients_need_their_secret() {
        let (base, _dir) = spawn(false, true).await;
        let http = client();
        let verifier = "c".repeat(64);
        let server: Value = http
            .get(format!("{base}/.well-known/oauth-authorization-server"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            server["token_endpoint_auth_methods_supported"],
            json!(["none", "client_secret_post", "client_secret_basic"])
        );
        let bad = http
            .post(format!("{base}/oauth/register"))
            .json(&json!({ "redirect_uris": [REDIRECT], "token_endpoint_auth_method": "private_key_jwt" }))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);

        let exchange = |id: String, secret: Option<String>, code: String, basic: bool| {
            let mut form = vec![
                ("grant_type", "authorization_code".to_string()),
                ("code", code),
                ("redirect_uri", REDIRECT.to_string()),
                ("code_verifier", verifier.clone()),
            ];
            let mut req = http.post(format!("{base}/oauth/token"));
            if basic {
                req = req.basic_auth(id, secret);
            } else {
                form.push(("client_id", id));
                if let Some(s) = secret {
                    form.push(("client_secret", s));
                }
            }
            req.form(&form).send()
        };

        // client_secret_post: no secret or a wrong one fail without burning the
        // code. Leaving out the client_id fails too, and that does burn it.
        let (id, secret) = register_client(&base, REDIRECT, Some("client_secret_post")).await;
        let secret = secret.expect("a confidential client gets a secret");
        let code = authorize(&base, &id, REDIRECT, &verifier).await;
        let resp = exchange(id.clone(), None, code.clone(), false)
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(
            resp.json::<Value>().await.unwrap()["error"],
            "invalid_client"
        );
        let resp = exchange(id.clone(), Some("wrong".into()), code.clone(), false)
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let anonymous = http
            .post(format!("{base}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", &verifier),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status(), 401);
        // The anonymous attempt consumed the code; get a new one.
        let code = authorize(&base, &id, REDIRECT, &verifier).await;
        let resp = exchange(id.clone(), Some(secret.clone()), code, false)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let tokens: Value = resp.json().await.unwrap();

        // Refreshing needs the secret too.
        let refresh = |secret: Option<&str>| {
            let mut form = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
                ("client_id", &id),
            ];
            if let Some(s) = secret {
                form.push(("client_secret", s));
            }
            http.post(format!("{base}/oauth/token")).form(&form).send()
        };
        assert_eq!(refresh(None).await.unwrap().status(), 401);
        assert_eq!(refresh(Some(&secret)).await.unwrap().status(), 200);

        // client_secret_basic.
        let (id, secret) = register_client(&base, REDIRECT, Some("client_secret_basic")).await;
        let code = authorize(&base, &id, REDIRECT, &verifier).await;
        let resp = exchange(id, secret, code, true).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    /// Reads SSE events from a response until `count` have arrived.
    async fn sse_events(
        resp: &mut reqwest::Response,
        buf: &mut String,
        count: usize,
    ) -> Vec<(String, String)> {
        let mut events = Vec::new();
        loop {
            while let Some(end) = buf.find("\n\n") {
                let block: String = buf.drain(..end + 2).collect();
                let mut event = String::from("message");
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim_start());
                    }
                }
                if !data.is_empty() {
                    events.push((event, data));
                }
                if events.len() == count {
                    return events;
                }
            }
            let chunk = tokio::time::timeout(Duration::from_secs(5), resp.chunk())
                .await
                .expect("SSE event timed out")
                .unwrap()
                .expect("SSE stream ended");
            buf.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }

    #[tokio::test]
    async fn legacy_sse_transport() {
        let (base, _dir) = spawn(true, true).await;
        let http = client();
        let public = base.replace("127.0.0.1", "localhost");

        // Same authentication as /mcp, with its own resource metadata.
        let resp = http.get(format!("{base}/mcp/sse")).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(
            resp.headers()["www-authenticate"],
            format!("Bearer resource_metadata=\"{public}/.well-known/oauth-protected-resource/mcp/sse\"").as_str()
        );
        let resource: Value = http
            .get(format!(
                "{base}/.well-known/oauth-protected-resource/mcp/sse"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resource["resource"], format!("{public}/mcp/sse"));

        let mut stream = http
            .get(format!("{base}/mcp/sse"))
            .bearer_auth(TOKEN)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(stream.status(), 200);
        let mut buf = String::new();
        let endpoint = sse_events(&mut stream, &mut buf, 1).await.remove(0);
        assert_eq!(endpoint.0, "endpoint");
        assert!(
            endpoint.1.starts_with("/mcp/messages?sessionId="),
            "{}",
            endpoint.1
        );
        let post = |msg: Value| {
            http.post(format!("{base}{}", endpoint.1))
                .bearer_auth(TOKEN)
                .json(&msg)
                .send()
        };

        let resp = post(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "legacy", "version": "1" }
            }
        }))
        .await
        .unwrap();
        assert_eq!(resp.status(), 202);
        let (event, data) = sse_events(&mut stream, &mut buf, 1).await.remove(0);
        assert_eq!(event, "message");
        let init: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(init["result"]["serverInfo"]["name"], "vault-sync");

        let resp = post(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        post(json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "list_tags", "arguments": {} }
        }))
        .await
        .unwrap();
        let (_, data) = sse_events(&mut stream, &mut buf, 1).await.remove(0);
        let call: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(call["id"], 2);
        assert_eq!(call["result"]["content"][0]["text"], "- #project (1)\n");

        // Unknown sessions and garbage are rejected; closing the stream ends the session.
        let resp = http
            .post(format!("{base}/mcp/messages?sessionId=nope"))
            .bearer_auth(TOKEN)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let resp = http
            .post(format!("{base}{}", endpoint.1))
            .bearer_auth(TOKEN)
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        drop(stream);
        let mut gone = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let resp = post(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .await
                .unwrap();
            if resp.status() == 404 {
                gone = true;
                break;
            }
        }
        assert!(gone, "the session outlived its stream");
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
