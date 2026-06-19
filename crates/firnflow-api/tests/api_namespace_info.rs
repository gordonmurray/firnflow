//! Integration test for `GET /ns/{namespace}` (issue #14).
//!
//! Drives the axum router via `tower::ServiceExt::oneshot`. Covers the
//! happy path (metadata shape after an upsert) and the 404 for a
//! namespace that has never been written. Index-flag transitions are
//! covered at the manager layer (`tests/manager_namespace_info.rs`),
//! where index builds are synchronous; the API index endpoints return
//! 202 and build in the background, which would race a read here.
//!
//! Gated `#[ignore]`: needs MinIO up.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use firnflow_api::router;
use firnflow_core::CoreMetrics;
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{test_state, unique_namespace};

async fn build_app() -> (axum::Router, Arc<CoreMetrics>, tempfile::TempDir) {
    let (state, tmp) = test_state().await;
    let metrics = Arc::clone(&state.metrics);
    (router(state), metrics, tmp)
}

async fn post_json(app: axum::Router, uri: String, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

async fn get(app: axum::Router, uri: String) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

async fn metrics_text(app: axum::Router) -> String {
    let request = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
#[ignore]
async fn info_returns_namespace_metadata() {
    let (app, _metrics, _tmp) = build_app().await;
    let ns = unique_namespace("api-info");

    let body = json!({
        "rows": [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0]},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0]},
            {"id": 3, "vector": [0.0, 0.0, 1.0, 0.0]},
        ]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), body).await;
    assert_eq!(status, StatusCode::OK);

    let (status, info) = get(app.clone(), format!("/ns/{ns}")).await;
    assert_eq!(status, StatusCode::OK, "info body: {info}");
    assert_eq!(info["namespace"].as_str().unwrap(), ns);
    assert_eq!(info["kind"].as_str().unwrap(), "single");
    assert_eq!(info["vector_dim"].as_u64().unwrap(), 4);
    assert_eq!(info["row_count"].as_u64().unwrap(), 3);
    assert!(info["fragment_count"].as_u64().unwrap() >= 1);
    assert_eq!(info["has_vector_index"], json!(false));
    assert_eq!(info["has_fts_index"], json!(false));
    // The first upsert auto-builds a BTree on `id`, so the scalar-index
    // flag is set even before any explicit index build.
    assert_eq!(info["has_scalar_index"], json!(true));
    assert!(
        info["table_version"].as_u64().unwrap() >= 1,
        "table version advances on commits"
    );

    // The metadata read bypasses NamespaceService, so the manager
    // records the backend hit itself; confirm it lands on the cost
    // counter under operation="info".
    let metrics = metrics_text(app).await;
    let info_ticks: u64 = metrics
        .lines()
        .filter(|l| l.starts_with("firnflow_s3_requests_total") && l.contains("operation=\"info\""))
        .filter_map(|l| l.split_whitespace().last())
        .filter_map(|v| v.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum();
    assert!(
        info_ticks >= 1,
        "GET /ns/{{ns}} should record an s3_requests_total{{operation=\"info\"}} tick"
    );
}

#[tokio::test]
#[ignore]
async fn info_404_for_unknown_namespace() {
    let (app, _metrics, _tmp) = build_app().await;
    let ns = unique_namespace("api-info-missing");

    let (status, body) = get(app, format!("/ns/{ns}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("does not exist"),
        "expected a not-found message, got {body}"
    );
}
