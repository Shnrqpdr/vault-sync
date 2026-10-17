//! Legacy HTTP+SSE transport (MCP 2024-11-05), for clients that predate
//! Streamable HTTP. The client opens `GET /mcp/sse` and receives an `endpoint`
//! event with the address to POST its messages to; every reply arrives on the
//! open stream. Each stream is a session, and it ends when the client
//! disconnects. Authentication is the same as for `/mcp`, applied by the
//! caller's middleware.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Bytes;
use axum::extract::{Query, State as AxState};
use axum::http::uri::Authority;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::channel::mpsc;
use futures::{SinkExt, Stream, StreamExt};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::ServiceExt;
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::catalog::Catalog;
use crate::mcp::VaultMcp;
use crate::oauth::random_token;

/// Open streams allowed at once. Only authenticated clients get here, so this
/// is a guard against leaks rather than against attackers.
const MAX_SESSIONS: usize = 32;
/// Messages buffered per session in each direction.
const BUFFER: usize = 32;

struct Sessions {
    catalog: Arc<Catalog>,
    /// Accepted `Host` values, as for the Streamable HTTP endpoint.
    allowed_hosts: Vec<String>,
    /// Each session's inbox: what the client POSTs goes to its server task.
    inboxes: Mutex<HashMap<String, mpsc::Sender<ClientJsonRpcMessage>>>,
}

impl Sessions {
    fn inboxes(&self) -> MutexGuard<'_, HashMap<String, mpsc::Sender<ClientJsonRpcMessage>>> {
        self.inboxes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// DNS rebinding protection, with the Streamable HTTP endpoint's rule: an
    /// allowed entry without a port accepts any port.
    fn host_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(host) = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.parse::<Authority>().ok())
        else {
            return false;
        };
        self.allowed_hosts.iter().any(|allowed| {
            let Ok(allowed) = allowed.parse::<Authority>() else {
                return false;
            };
            allowed.host().eq_ignore_ascii_case(host.host())
                && allowed
                    .port_u16()
                    .is_none_or(|p| host.port_u16() == Some(p))
        })
    }
}

/// Drops the session when the client's stream goes away. Dropping the inbox
/// ends the server task, which in turn closes the outgoing channel.
struct SessionGuard {
    id: String,
    sessions: Arc<Sessions>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.sessions.inboxes().remove(&self.id);
        debug!(session = %self.id, "legacy SSE session closed");
    }
}

pub fn routes(catalog: Arc<Catalog>, allowed_hosts: Vec<String>) -> Router {
    let sessions = Arc::new(Sessions {
        catalog,
        allowed_hosts,
        inboxes: Mutex::default(),
    });
    Router::new()
        .route("/mcp/sse", get(open))
        .route("/mcp/messages", post(message))
        .with_state(sessions)
}

async fn open(AxState(sessions): AxState<Arc<Sessions>>, headers: HeaderMap) -> Response {
    if !sessions.host_allowed(&headers) {
        return error(StatusCode::FORBIDDEN, "host not allowed");
    }
    let id = random_token();
    let (inbox, incoming) = mpsc::channel(BUFFER);
    {
        let mut inboxes = sessions.inboxes();
        if inboxes.len() >= MAX_SESSIONS {
            warn!("too many legacy SSE sessions");
            return error(StatusCode::SERVICE_UNAVAILABLE, "too many open sessions");
        }
        inboxes.insert(id.clone(), inbox);
    }
    let (outgoing, replies) = mpsc::channel(BUFFER);
    let server = VaultMcp::new(sessions.catalog.clone());
    tokio::spawn(async move {
        match server.serve((outgoing, incoming)).await {
            Ok(running) => {
                let _ = running.waiting().await;
            }
            Err(e) => debug!(error = %e, "legacy SSE session ended before initializing"),
        }
    });
    debug!(session = %id, "legacy SSE session opened");

    let endpoint = Event::default()
        .event("endpoint")
        .data(format!("/mcp/messages?sessionId={id}"));
    let guard = SessionGuard { id, sessions };
    let events =
        futures::stream::once(async { Ok(endpoint) }).chain(replies_stream(replies, guard));
    (
        // Proxies such as nginx buffer responses unless told otherwise.
        [("x-accel-buffering", "no")],
        Sse::new(events).keep_alive(KeepAlive::default()),
    )
        .into_response()
}

/// The server's messages as `message` events. The stream owns the guard, so
/// the session lives exactly as long as the client keeps the stream open.
fn replies_stream(
    replies: mpsc::Receiver<rmcp::model::ServerJsonRpcMessage>,
    guard: SessionGuard,
) -> impl Stream<Item = Result<Event, Infallible>> {
    replies.filter_map(move |msg| {
        let _session = &guard;
        let event = match serde_json::to_string(&msg) {
            Ok(data) => Some(Ok(Event::default().event("message").data(data))),
            Err(e) => {
                warn!(error = %e, "could not serialize an MCP message");
                None
            }
        };
        async move { event }
    })
}

#[derive(Deserialize)]
struct MessageQuery {
    #[serde(rename = "sessionId")]
    session_id: String,
}

async fn message(
    AxState(sessions): AxState<Arc<Sessions>>,
    Query(q): Query<MessageQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !sessions.host_allowed(&headers) {
        return error(StatusCode::FORBIDDEN, "host not allowed");
    }
    let Some(mut inbox) = sessions.inboxes().get(&q.session_id).cloned() else {
        return error(
            StatusCode::NOT_FOUND,
            "session not found; open /mcp/sse again",
        );
    };
    let msg: ClientJsonRpcMessage = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON-RPC message: {e}"),
            )
        }
    };
    if inbox.send(msg).await.is_err() {
        return error(StatusCode::NOT_FOUND, "session closed; open /mcp/sse again");
    }
    StatusCode::ACCEPTED.into_response()
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
