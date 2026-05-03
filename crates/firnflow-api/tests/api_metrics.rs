//! Slice-2a integration test: `GET /metrics` exposition plus
//! assertions that the counters moved in response to real traffic
//! through the service.
//!
//! Traffic shape: 1 upsert, 1 query (cache miss), 1 identical query
//! (cache hit). Expected metric state after:
//!
//! | metric                                      | value |
//! | ------------------------------------------- | ----- |
//! | `cache_misses_total{ns}`                    | 1     |
//! | `cache_hits_total{ns}`                      | 1     |
//! | `s3_requests_total{ns, op=upsert}`          | 1     |
//! | `s3_requests_total{ns, op=query}`           | 1 (miss only; the hit must NOT count) |
//! | `active_namespaces`                         | 1     |
//! | `query_duration_seconds_count{ns, vector}` | 2     |
//! | `write_duration_seconds_count{ns}`          | 1     |
//!
//! The cache-hit asymmetry on `s3_requests_total` is the load-bearing
//! assertion — that gap is the whole point of the metric.
//!
//! Gated `#[ignore]`: needs MinIO up.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use firnflow_api::router;
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{test_state, unique_namespace};

async fn build_app() -> (axum::Router, tempfile::TempDir) {
    let (state, tmp) = test_state().await;
    (router(state), tmp)
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

/// Scan the Prometheus exposition text for the first line that
/// starts with `metric_name{…labels…}` and return the numeric value
/// at the end. Returns `None` if the line is absent — caller
/// interprets that as "counter is still at zero" because the
/// prometheus crate only emits lines for counters with observations.
fn metric_value(body: &str, metric: &str, label_needle: &str) -> Option<f64> {
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        if !line.starts_with(metric) {
            continue;
        }
        if !label_needle.is_empty() && !line.contains(label_needle) {
            continue;
        }
        // line is "name{labels} value" — rsplit once on whitespace.
        let value = line.rsplit_once(char::is_whitespace)?.1;
        return value.parse().ok();
    }
    None
}

#[tokio::test]
#[ignore]
async fn metrics_reflect_cache_hits_misses_and_s3_asymmetry() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("slice2a");

    // 1. Upsert (records s3_requests{op=upsert} and write_duration)
    let upsert_body = json!({
        "rows": [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
        ]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK);

    // 2. First query (records cache_miss and s3_requests{op=query})
    let query_body = json!({
        "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "k": 3
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/query"), query_body.clone()).await;
    assert_eq!(status, StatusCode::OK);

    // 3. Second query — same request, must cache-hit.
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/query"), query_body).await;
    assert_eq!(status, StatusCode::OK);

    // 4. Fetch /metrics and assert.
    let body = fetch_metrics(app).await;

    // Sanity: all four metric families present in the help/type block.
    assert!(
        body.contains("firnflow_cache_hits_total"),
        "help block for cache_hits_total not emitted"
    );
    assert!(body.contains("firnflow_cache_misses_total"));
    assert!(body.contains("firnflow_query_duration_seconds"));
    assert!(body.contains("firnflow_write_duration_seconds"));
    assert!(body.contains("firnflow_active_namespaces"));
    assert!(body.contains("firnflow_s3_requests_total"));

    let ns_label = format!(r#"namespace="{ns}""#);

    assert_eq!(
        metric_value(&body, "firnflow_cache_hits_total", &ns_label),
        Some(1.0),
        "expected exactly 1 cache hit for {ns}"
    );
    assert_eq!(
        metric_value(&body, "firnflow_cache_misses_total", &ns_label),
        Some(1.0),
        "expected exactly 1 cache miss for {ns}"
    );

    // s3_requests_total has a second label — grep for both.
    let upsert_label = format!(r#"namespace="{ns}",operation="upsert""#);
    let query_label = format!(r#"namespace="{ns}",operation="query""#);
    assert_eq!(
        metric_value(&body, "firnflow_s3_requests_total", &upsert_label),
        Some(1.0),
        "expected exactly 1 upsert-kind S3 request for {ns}"
    );
    assert_eq!(
        metric_value(&body, "firnflow_s3_requests_total", &query_label),
        Some(1.0),
        "expected exactly 1 query-kind S3 request — the cache hit must NOT count"
    );

    // Histograms expose `_count` as the total sample count.
    assert_eq!(
        metric_value(&body, "firnflow_query_duration_seconds_count", &ns_label),
        Some(2.0),
        "query_duration histogram should have observed both queries"
    );
    assert_eq!(
        metric_value(&body, "firnflow_write_duration_seconds_count", &ns_label),
        Some(1.0),
        "write_duration histogram should have observed the one upsert"
    );

    // active_namespaces is a gauge with no labels.
    assert_eq!(
        metric_value(&body, "firnflow_active_namespaces", ""),
        Some(1.0),
        "active_namespaces should be exactly 1"
    );
}
