//! Issue-1 API integration test: connection pool observable via
//! `/metrics`.
//!
//! Drives the full HTTP path for one namespace — upsert → several
//! queries — and verifies that:
//!
//! 1. `firnflow_cached_handles` ticks to exactly `1` after the
//!    first upsert. That's the direct pool-gauge assertion.
//! 2. `firnflow_s3_requests_total{operation="query"}` stays at
//!    `1` for the whole run: the first query is a cache miss, the
//!    remaining four are foyer-cache hits that never touch the
//!    backend. Pool reuse amplifies the foyer cache's win — even
//!    the single miss skips the connect() + open_table() cost
//!    thanks to the pooled handle captured at upsert time.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_pool \
//!     -- --ignored --nocapture
//! ```

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use firnflow_api::router;
use firnflow_core::CoreMetrics;
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{test_state, unique_namespace};

async fn build_app() -> (axum::Router, tempfile::TempDir, Arc<CoreMetrics>) {
    let (state, tmp) = test_state().await;
    let metrics = Arc::clone(&state.metrics);
    (router(state), tmp, metrics)
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

async fn fetch_metrics(app: axum::Router) -> String {
    let request = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn metric_value(body: &str, metric: &str, label_needle: &str) -> Option<f64> {
    for line in body.lines() {
        if line.starts_with('#') || !line.starts_with(metric) {
            continue;
        }
        if !label_needle.is_empty() && !line.contains(label_needle) {
            continue;
        }
        let value = line.rsplit_once(char::is_whitespace)?.1;
        return value.parse().ok();
    }
    None
}

#[tokio::test]
#[ignore]
async fn repeated_queries_reuse_connections() {
    let (app, _tmp, metrics) = build_app().await;
    let ns = unique_namespace("pool-api");

    assert_eq!(
        metrics.cached_handles_value(),
        0,
        "gauge is 0 before any traffic"
    );

    // Upsert once — first request for this namespace, so the pool
    // populates on this call.
    let upsert_body = json!({
        "rows": [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]}
        ]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        metrics.cached_handles_value(),
        1,
        "pool populated by the upsert"
    );

    // Query 5 times — first is a foyer-cache miss (hits the pooled
    // handle), the remaining four are foyer-cache hits and never
    // reach the backend at all.
    let query_body = json!({
        "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "k": 1
    });
    for _ in 0..5 {
        let (status, _) =
            post_json(app.clone(), format!("/ns/{ns}/query"), query_body.clone()).await;
        assert_eq!(status, StatusCode::OK);
    }

    // Pool size must not have grown: every query on this namespace
    // reused the handle captured by the initial upsert.
    assert_eq!(
        metrics.cached_handles_value(),
        1,
        "pool size unchanged across repeated queries on the same ns"
    );

    // And the `/metrics` endpoint exposes the same value.
    let body = fetch_metrics(app.clone()).await;
    assert_eq!(
        metric_value(&body, "firnflow_cached_handles", ""),
        Some(1.0),
        "cached_handles gauge reflected in /metrics output"
    );

    let query_label = format!(r#"namespace="{ns}",operation="query""#);
    assert_eq!(
        metric_value(&body, "firnflow_s3_requests_total", &query_label),
        Some(1.0),
        "exactly one query-kind S3 request — four cache hits must not count"
    );
}
