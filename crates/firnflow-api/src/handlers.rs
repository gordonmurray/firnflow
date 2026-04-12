//! Request handlers for the three slice-1c endpoints.
//!
//! * `GET  /health`
//! * `POST /ns/{namespace}/upsert`
//! * `POST /ns/{namespace}/query`

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use firnflow_core::{NamespaceId, QueryRequest, QueryResultSet};

use crate::error::ApiError;
use crate::state::AppState;

/// Body of a successful delete response.
#[derive(Debug, Serialize)]
pub struct DeleteResponse {
    /// Number of S3 objects removed during the delete.
    pub objects_deleted: usize,
}

/// Body of `POST /ns/{namespace}/warmup`. A list of query
/// parameters the operator wants pre-populated in the cache.
#[derive(Debug, Deserialize)]
pub struct WarmupRequest {
    /// Queries to run through the cache-aside path as a background
    /// task. The handler accepts the request and spawns a task
    /// that iterates through this list; per-query failures are
    /// logged via `tracing::warn!` and do not abort the warmup.
    pub queries: Vec<QueryRequest>,
}

/// Body of a successful warmup response (HTTP 202 Accepted). The
/// number is how many queries the background task was *asked*
/// to run, not how many actually succeeded by the time the
/// response is returned — the task runs after the response is
/// sent.
#[derive(Debug, Serialize)]
pub struct WarmupResponse {
    pub queued: usize,
}

/// One row in an upsert request.
#[derive(Debug, Deserialize)]
pub struct UpsertRow {
    pub id: u64,
    pub vector: Vec<f32>,
}

/// Body of `POST /ns/{namespace}/upsert`.
#[derive(Debug, Deserialize)]
pub struct UpsertRequest {
    pub rows: Vec<UpsertRow>,
}

/// Body of a successful upsert response.
#[derive(Debug, Serialize)]
pub struct UpsertResponse {
    /// Number of rows accepted for append. Matches `rows.len()` on the
    /// request — there is no per-row failure reporting in slice 1c.
    pub upserted: usize,
}

/// Liveness probe. Returns HTTP 200 with body `ok`.
pub async fn health() -> &'static str {
    "ok"
}

/// Append rows to a namespace and invalidate its cached query results.
pub async fn upsert(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<UpsertRequest>,
) -> Result<Json<UpsertResponse>, ApiError> {
    let ns = NamespaceId::new(namespace)?;
    let count = req.rows.len();
    let rows: Vec<(u64, Vec<f32>)> = req.rows.into_iter().map(|r| (r.id, r.vector)).collect();
    state.service.upsert(&ns, rows).await?;
    Ok(Json(UpsertResponse { upserted: count }))
}

/// Run a vector nearest-neighbour query through the cache-aside path.
pub async fn query(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResultSet>, ApiError> {
    let ns = NamespaceId::new(namespace)?;
    let result = state.service.query(&ns, &req).await?;
    Ok(Json(result))
}

/// Delete a namespace: remove every S3 object under its prefix and
/// evict every cached query result for it. Returns the count of
/// S3 objects the manager actually deleted.
pub async fn delete(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Result<Json<DeleteResponse>, ApiError> {
    let ns = NamespaceId::new(namespace)?;
    let objects_deleted = state.service.delete(&ns).await?;
    Ok(Json(DeleteResponse { objects_deleted }))
}

/// Async cache-warmup hint.
///
/// CLAUDE.md: *"the warmup endpoint must be non-blocking: it
/// spawns an async task and returns 202 immediately"*.
///
/// The handler validates the namespace, spawns a `tokio::task`
/// that runs each query from the request body through
/// [`NamespaceService::query`] (populating the cache as it
/// goes), and returns `202 Accepted` with the number of queries
/// queued. Failures inside the background task are logged via
/// `tracing::warn!` — they do not affect the HTTP response or
/// abort the rest of the warmup batch.
pub async fn warmup(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<WarmupRequest>,
) -> Result<(StatusCode, Json<WarmupResponse>), ApiError> {
    let ns = NamespaceId::new(namespace)?;
    let queued = req.queries.len();

    let service = Arc::clone(&state.service);
    let ns_owned = ns.clone();
    let queries = req.queries;
    tokio::spawn(async move {
        for (idx, query) in queries.iter().enumerate() {
            if let Err(e) = service.query(&ns_owned, query).await {
                tracing::warn!(
                    namespace = %ns_owned,
                    query_index = idx,
                    error = %e,
                    "warmup query failed"
                );
            }
        }
    });

    Ok((StatusCode::ACCEPTED, Json(WarmupResponse { queued })))
}

/// Prometheus scrape endpoint. Serialises the process-wide
/// [`CoreMetrics`] registry into the Prometheus text exposition
/// format with a `text/plain; version=0.0.4` content type.
pub async fn metrics(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let body = state.metrics.encode()?;
    Ok((
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    ))
}
