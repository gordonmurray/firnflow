//! Request handlers for the firnflow REST API.
//!
//! * `GET    /health`
//! * `POST   /ns/{namespace}/upsert`
//! * `POST   /ns/{namespace}/query`
//! * `GET    /ns/{namespace}/list`
//! * `GET    /ns/{namespace}`
//! * `DELETE /ns/{namespace}`
//! * `POST   /ns/{namespace}/warmup`
//! * `POST   /ns/{namespace}/index`
//! * `POST   /ns/{namespace}/fts-index`
//! * `POST   /ns/{namespace}/scalar-index`
//! * `POST   /ns/{namespace}/compact`
//! * `GET    /operations/{operation_id}`
//! * `GET    /metrics`

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use firnflow_core::{
    decode_list_cursor, FirnflowError, IndexRequest, ListOrder, ListPage, NamespaceId,
    NamespaceInfo, QueryRequest, UpsertRow as CoreUpsertRow, LIST_MAX_LIMIT,
};

use crate::error::ApiError;
use crate::operations::{OperationKind, OperationRecord, OperationStatus};
use crate::state::AppState;

const CACHE_SOURCE_REQUEST_HEADER: &str = "x-firn-debug-cache-source";
const CACHE_SOURCE_RESPONSE_HEADER: &str = "x-firn-cache-source";

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

/// Body of the `202 Accepted` returned by every endpoint that starts
/// background work. The `operation_id` is an opaque, pollable handle;
/// fetch its current state from `GET /operations/{operation_id}`.
#[derive(Debug, Serialize)]
pub struct OperationAccepted {
    /// Opaque handle for the background work; poll it for status.
    pub operation_id: String,
    /// What kind of operation was started.
    pub kind: OperationKind,
    /// Namespace the work targets.
    pub namespace: String,
    /// Lifecycle state at acceptance time (always `running` in v1).
    pub status: OperationStatus,
}

/// Warmup's `202` body: the standard operation handle plus the number
/// of queries the background task was asked to run (not how many had
/// completed by the time the response was sent).
#[derive(Debug, Serialize)]
pub struct WarmupAccepted {
    #[serde(flatten)]
    pub operation: OperationAccepted,
    /// Number of queries the background task was asked to run.
    pub queued: usize,
}

/// One row in an upsert request.
///
/// The payload field used depends on the namespace's vector kind:
/// - **Single-vector namespaces**: set [`vector`](Self::vector) to
///   a list of floats of length `dim`.
/// - **Multivector namespaces**: set [`vectors`](Self::vectors) to
///   a non-empty list of equal-length inner vectors.
///
/// At most one of the two fields may be populated; setting both
/// returns 400 with a per-row diagnostic. The first row of the
/// first upsert into a fresh namespace fixes the namespace's kind
/// for its lifetime — subsequent payloads in the wrong shape are
/// rejected at the API boundary.
#[derive(Debug, Deserialize)]
pub struct UpsertRow {
    /// Stable row identifier.
    pub id: u64,
    /// Single-vector payload. Length must match the namespace's
    /// dimension. Default empty for multivector rows.
    #[serde(default)]
    pub vector: Vec<f32>,
    /// Multivector payload. Each inner vector must have the
    /// namespace's inner sub-vector dimension; the outer list
    /// length is the per-row sub-vector count and may vary between
    /// rows. `None` for single-vector rows.
    #[serde(default)]
    pub vectors: Option<Vec<Vec<f32>>>,
    /// Optional text payload for full-text search.
    #[serde(default)]
    pub text: Option<String>,
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
    /// request — there is no per-row failure reporting yet.
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
    let rows: Vec<CoreUpsertRow> = req
        .rows
        .into_iter()
        .map(|r| CoreUpsertRow {
            id: r.id,
            vector: r.vector,
            vectors: r.vectors,
            text: r.text,
        })
        .collect();
    state.service.upsert(&ns, rows).await?;
    Ok(Json(UpsertResponse { upserted: count }))
}

/// Run a vector nearest-neighbour query through the cache-aside path.
pub async fn query(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
    Json(req): Json<QueryRequest>,
) -> Result<Response, ApiError> {
    let ns = NamespaceId::new(namespace)?;
    let include_cache_source = headers
        .get(CACHE_SOURCE_REQUEST_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| matches!(value, "1" | "true" | "yes"));
    let outcome = state.service.query_with_cache_source(&ns, &req).await?;
    let cache_source = outcome.cache_source.as_str();
    let mut response = Json(outcome.result).into_response();
    if include_cache_source {
        response.headers_mut().insert(
            CACHE_SOURCE_RESPONSE_HEADER,
            HeaderValue::from_static(cache_source),
        );
    }
    Ok(response)
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
/// The warmup endpoint is non-blocking: it spawns an async task and
/// returns 202 immediately.
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
) -> Result<(StatusCode, Json<WarmupAccepted>), ApiError> {
    let ns = NamespaceId::new(namespace)?;
    let queued = req.queries.len();

    let operation_id = state
        .operations
        .start(OperationKind::Warmup, ns.to_string());

    let service = Arc::clone(&state.service);
    let operations = Arc::clone(&state.operations);
    let ns_owned = ns.clone();
    let queries = req.queries;
    let op_for_task = operation_id.clone();
    tokio::spawn(async move {
        let total = queries.len();
        let mut failures = 0usize;
        let mut first_error: Option<String> = None;
        for (idx, query) in queries.iter().enumerate() {
            if let Err(e) = service.query(&ns_owned, query).await {
                tracing::warn!(
                    namespace = %ns_owned,
                    query_index = idx,
                    error = %e,
                    "warmup query failed"
                );
                failures += 1;
                if first_error.is_none() {
                    first_error = Some(operation_error_message(&e));
                }
            }
        }
        // Every query is attempted regardless of individual failures, but
        // the operation only reports `succeeded` if all of them warmed. If
        // any failed it reports `failed` with a count and the first
        // message, so a poller does not read `succeeded` when nothing was
        // actually cached.
        if failures == 0 {
            operations.succeed(&op_for_task);
        } else {
            operations.fail(
                &op_for_task,
                format!(
                    "{failures} of {total} warmup queries failed; first error: {}",
                    first_error.unwrap_or_else(|| "operation failed".into())
                ),
            );
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(WarmupAccepted {
            operation: OperationAccepted {
                operation_id,
                kind: OperationKind::Warmup,
                namespace: ns.to_string(),
                status: OperationStatus::Running,
            },
            queued,
        }),
    ))
}

/// Explicit ANN index build.
///
/// Spawns a background task that builds an IVF_PQ index on the
/// namespace's vector column and returns `202 Accepted` with an
/// `operation_id`. Poll `GET /operations/{operation_id}` for
/// completion, or watch the `firnflow_index_build_duration_seconds`
/// histogram.
pub async fn create_index(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<IndexRequest>,
) -> Result<(StatusCode, Json<OperationAccepted>), ApiError> {
    let ns = NamespaceId::new(namespace)?;

    if req.kind != "ivf_pq" {
        return Err(ApiError::Core(
            firnflow_core::FirnflowError::InvalidRequest(format!(
                "unsupported index kind {:?}, only \"ivf_pq\" is supported",
                req.kind
            )),
        ));
    }

    // Validate PQ tuning options synchronously, before spawning the
    // background task. The manager performs the same check itself
    // (so direct callers stay protected), but doing it here as well
    // means a bad payload returns 400 instead of a misleading 202
    // followed by a log-only failure.
    firnflow_core::validate_ivf_pq_options(req.num_bits, req.num_sub_vectors)
        .map_err(ApiError::Core)?;

    let operation_id = state.operations.start(OperationKind::Index, ns.to_string());

    let service = Arc::clone(&state.service);
    let operations = Arc::clone(&state.operations);
    let ns_owned = ns.clone();
    let op_for_task = operation_id.clone();
    tokio::spawn(async move {
        match service
            .create_index(
                &ns_owned,
                req.num_partitions,
                req.num_sub_vectors,
                req.num_bits,
            )
            .await
        {
            Ok(()) => operations.succeed(&op_for_task),
            Err(e) => {
                tracing::error!(
                    namespace = %ns_owned,
                    error = %e,
                    "index build failed"
                );
                operations.fail(&op_for_task, operation_error_message(&e));
            }
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(OperationAccepted {
            operation_id,
            kind: OperationKind::Index,
            namespace: ns.to_string(),
            status: OperationStatus::Running,
        }),
    ))
}

/// Build a BM25 full-text search index on the namespace's `text`
/// column. Same 202-with-`operation_id` pattern as the vector index
/// build; poll `GET /operations/{operation_id}` for completion.
pub async fn create_fts_index(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Result<(StatusCode, Json<OperationAccepted>), ApiError> {
    let ns = NamespaceId::new(namespace)?;

    let operation_id = state
        .operations
        .start(OperationKind::FtsIndex, ns.to_string());

    let service = Arc::clone(&state.service);
    let operations = Arc::clone(&state.operations);
    let ns_owned = ns.clone();
    let op_for_task = operation_id.clone();
    tokio::spawn(async move {
        match service.create_fts_index(&ns_owned).await {
            Ok(()) => operations.succeed(&op_for_task),
            Err(e) => {
                tracing::error!(
                    namespace = %ns_owned,
                    error = %e,
                    "FTS index build failed"
                );
                operations.fail(&op_for_task, operation_error_message(&e));
            }
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(OperationAccepted {
            operation_id,
            kind: OperationKind::FtsIndex,
            namespace: ns.to_string(),
            status: OperationStatus::Running,
        }),
    ))
}

/// Build a BTree scalar index on `_ingested_at`. Same
/// 202-with-`operation_id` pattern as `/fts-index`; poll
/// `GET /operations/{operation_id}` for completion, or watch
/// `firnflow_index_build_duration_seconds{kind="scalar"}`.
///
/// v1 hardcodes the column to `_ingested_at` to mirror the same
/// constraint `/list` puts on `order_by`. Future user-column ordering
/// adds a body parameter at that point.
pub async fn create_scalar_index(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Result<(StatusCode, Json<OperationAccepted>), ApiError> {
    let ns = NamespaceId::new(namespace)?;

    let operation_id = state
        .operations
        .start(OperationKind::ScalarIndex, ns.to_string());

    let service = Arc::clone(&state.service);
    let operations = Arc::clone(&state.operations);
    let ns_owned = ns.clone();
    let op_for_task = operation_id.clone();
    tokio::spawn(async move {
        match service.create_scalar_index(&ns_owned, "_ingested_at").await {
            Ok(()) => operations.succeed(&op_for_task),
            Err(e) => {
                tracing::error!(
                    namespace = %ns_owned,
                    error = %e,
                    "scalar index build failed"
                );
                operations.fail(&op_for_task, operation_error_message(&e));
            }
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(OperationAccepted {
            operation_id,
            kind: OperationKind::ScalarIndex,
            namespace: ns.to_string(),
            status: OperationStatus::Running,
        }),
    ))
}

/// Explicit compaction.
///
/// Spawns a background task that merges small data files into fewer,
/// larger ones and returns `202 Accepted` with an `operation_id`. Poll
/// `GET /operations/{operation_id}` for completion, or watch the
/// `firnflow_compaction_duration_seconds` histogram.
pub async fn compact(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Result<(StatusCode, Json<OperationAccepted>), ApiError> {
    let ns = NamespaceId::new(namespace)?;

    let operation_id = state
        .operations
        .start(OperationKind::Compact, ns.to_string());

    let service = Arc::clone(&state.service);
    let operations = Arc::clone(&state.operations);
    let ns_owned = ns.clone();
    let op_for_task = operation_id.clone();
    tokio::spawn(async move {
        match service.compact(&ns_owned).await {
            Ok(result) => {
                tracing::info!(
                    namespace = %ns_owned,
                    fragments_removed = result.fragments_removed,
                    fragments_added = result.fragments_added,
                    "compaction complete"
                );
                operations.succeed(&op_for_task);
            }
            Err(e) => {
                tracing::error!(
                    namespace = %ns_owned,
                    error = %e,
                    "compaction failed"
                );
                operations.fail(&op_for_task, operation_error_message(&e));
            }
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(OperationAccepted {
            operation_id,
            kind: OperationKind::Compact,
            namespace: ns.to_string(),
            status: OperationStatus::Running,
        }),
    ))
}

const DEFAULT_LIST_LIMIT: usize = 50;
const LIST_ORDER_BY: &str = "_ingested_at";

/// Query parameters for `GET /ns/{namespace}/list`.
///
/// All fields are optional to keep simple "give me the latest"
/// clients to a bare path. Defaults: `order_by=_ingested_at`,
/// `order=desc`, `limit=50`, no cursor.
#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub order_by: Option<String>,
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// List rows in a namespace ordered by `_ingested_at`.
///
/// Deliberately **does not** go through `NamespaceService`: pagination
/// tails would pollute the foyer cache with cold one-shot entries. The
/// handler calls `state.manager.list(...)` directly.
pub async fn list(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListPage>, ApiError> {
    let ns = NamespaceId::new(namespace)?;

    // V1 only supports `_ingested_at`. User-column ordering is
    // gated behind scalar-index support, which is a separate issue.
    if let Some(col) = params.order_by.as_deref() {
        if col != LIST_ORDER_BY {
            return Err(ApiError::Core(FirnflowError::InvalidRequest(format!(
                "order_by must be {LIST_ORDER_BY:?} in v1, got {col:?}"
            ))));
        }
    }

    let order = match params.order.as_deref().unwrap_or("desc") {
        "desc" => ListOrder::Desc,
        "asc" => ListOrder::Asc,
        other => {
            return Err(ApiError::Core(FirnflowError::InvalidRequest(format!(
                "order must be \"asc\" or \"desc\", got {other:?}"
            ))));
        }
    };

    let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 {
        return Err(ApiError::Core(FirnflowError::InvalidRequest(
            "limit must be >= 1".into(),
        )));
    }
    if limit > LIST_MAX_LIMIT {
        return Err(ApiError::Core(FirnflowError::InvalidRequest(format!(
            "limit {limit} exceeds maximum {LIST_MAX_LIMIT}"
        ))));
    }

    let cursor = match params.cursor.as_deref() {
        Some(raw) if !raw.is_empty() => Some(decode_list_cursor(raw)?),
        _ => None,
    };

    let page = state.manager.list(&ns, limit, order, cursor).await?;
    Ok(Json(page))
}

/// Return operational metadata for a namespace: vector kind and
/// dimension, row count, fragment count, which index kinds are built,
/// and the current table version.
///
/// Like `/list`, this bypasses the foyer cache and calls
/// `state.manager.info(...)` directly — it is namespace state, not a
/// query result, so caching it would only risk staleness. Returns 404
/// when the namespace has never been written.
pub async fn info(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Result<Json<NamespaceInfo>, ApiError> {
    let ns = NamespaceId::new(namespace)?;
    match state.manager.info(&ns).await? {
        Some(info) => Ok(Json(info)),
        None => Err(ApiError::NotFound(format!("namespace {ns} does not exist"))),
    }
}

/// Return the current state of a background operation by its
/// `operation_id` (returned in the 202 from warmup, index, fts-index,
/// scalar-index, and compact). Returns 404 if the id is unknown or its
/// record has been evicted from the bounded in-memory registry.
pub async fn get_operation(
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
) -> Result<Json<OperationRecord>, ApiError> {
    match state.operations.get(&operation_id) {
        Some(record) => Ok(Json(record)),
        None => Err(ApiError::NotFound(format!(
            "operation {operation_id} not found"
        ))),
    }
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

/// Map a background-task error to a concise, client-facing message for
/// an operation record. This mirrors the synchronous API error policy
/// in [`crate::error`]: validation and capability errors carry a
/// caller-actionable message and are surfaced, while backend, cache,
/// I/O, and metrics failures can embed storage or provider internals
/// and are collapsed to a generic message. The full error is always
/// preserved in the server logs via `tracing` at the call site.
fn operation_error_message(err: &FirnflowError) -> String {
    match err {
        FirnflowError::InvalidNamespace(msg) => format!("invalid namespace: {msg}"),
        FirnflowError::InvalidRequest(msg) => format!("invalid request: {msg}"),
        FirnflowError::Unsupported(msg) => format!("unsupported: {msg}"),
        FirnflowError::Backend(_)
        | FirnflowError::Cache(_)
        | FirnflowError::Io(_)
        | FirnflowError::Metrics(_) => "operation failed".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_error_message_surfaces_caller_errors() {
        let msg = operation_error_message(&FirnflowError::Unsupported(
            "namespace predates the _ingested_at column".into(),
        ));
        assert!(
            msg.contains("namespace predates the _ingested_at column"),
            "capability errors should reach the caller, got: {msg}"
        );

        let msg = operation_error_message(&FirnflowError::InvalidRequest(
            "num_sub_vectors must divide the dimension".into(),
        ));
        assert!(msg.contains("num_sub_vectors must divide the dimension"));
    }

    #[test]
    fn operation_error_message_scrubs_backend_internals() {
        let leaky = FirnflowError::Backend(
            "s3://secret-bucket/ns: AccessDenied request-id 0xDEADBEEF".into(),
        );
        let msg = operation_error_message(&leaky);
        assert_eq!(msg, "operation failed");
        assert!(
            !msg.contains("secret-bucket"),
            "backend internals must not leak into the operation record"
        );
    }
}
