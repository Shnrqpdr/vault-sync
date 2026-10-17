//! Internal HTTP API: sync health, search and note reading. It has no
//! authentication, so it must never be exposed through the reverse proxy.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::catalog::{Catalog, Filter, NoteDetail, NoteMeta, SearchHit, TagCount};
use crate::sync::{SharedStatus, Status};

#[derive(Clone)]
pub struct AppState {
    pub status: SharedStatus,
    pub catalog: Arc<Catalog>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/search", get(search))
        .route("/notes", get(list))
        .route("/notes/{*path}", get(note))
        .route("/tags", get(tags))
        .with_state(state)
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let msg = format!("{e:#}");
        warn!(error = %msg, "API error");
        Self(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

#[derive(Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    tag: Option<String>,
    folder: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ListParams {
    tag: Option<String>,
    folder: Option<String>,
    limit: Option<usize>,
}

async fn health(State(app): State<AppState>) -> Json<Status> {
    Json(app.status.read().await.clone())
}

/// `GET /search?q=...&tag=...&folder=...&limit=20`
async fn search(
    State(app): State<AppState>,
    Query(p): Query<SearchParams>,
) -> ApiResult<Vec<SearchHit>> {
    if p.q.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "the q parameter is required".into(),
        ));
    }
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    let filter = Filter {
        tag: p.tag,
        folder: p.folder,
    };
    let hits = blocking(move || app.catalog.search(&p.q, &filter, limit)).await?;
    Ok(Json(hits))
}

/// `GET /notes?tag=...&folder=...&limit=50`, most recent first.
async fn list(State(app): State<AppState>, Query(p): Query<ListParams>) -> Json<Vec<NoteMeta>> {
    let filter = Filter {
        tag: p.tag,
        folder: p.folder,
    };
    Json(
        app.catalog
            .list(&filter, p.limit.unwrap_or(50).clamp(1, 1000)),
    )
}

/// `GET /notes/<path or name>`: content, frontmatter, links and backlinks.
async fn note(State(app): State<AppState>, Path(path): Path<String>) -> ApiResult<NoteDetail> {
    let wanted = path.clone();
    match blocking(move || app.catalog.note(&wanted)).await? {
        Some(note) => Ok(Json(note)),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("note not found: {path}"),
        )),
    }
}

async fn tags(State(app): State<AppState>) -> Json<Vec<TagCount>> {
    Json(app.catalog.tags())
}

/// Tantivy and disk reads are synchronous: keep them off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("task aborted: {e}"))?
        .map_err(ApiError::from)
}
