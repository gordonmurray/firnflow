//! Slice-3b integration test: `POST /ns/{namespace}/warmup` returns
//! `202 Accepted` immediately, spawns a background task that runs
//! the queries through the cache-aside path, and the post-warmup
//! state of the cache should show cache hits for those queries.
//!
//! The background task is non-deterministic — after the handler
//! returns 202 the test has to wait for the spawned task to drain.
//! We use a short poll loop (up to 2 s) reading the metrics
//! registry for the expected `cache_misses_total` rise, which is
//! the observable side effect of the warmup actually running.
//!
//! Gated `#[ignore]`: needs MinIO up.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use firnflow_api::{router, AppState};
use firnflow_core::cache::NamespaceCache;
use firnflow_core::metrics::test_metrics;
use firnflow_core::{CoreMetrics, NamespaceManager, NamespaceService};
use serde_json::{json, Value};
use tower::ServiceExt;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn unique_namespace(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{nanos}")
}

fn minio_options() -> HashMap<String, String> {
    HashMap::from([
        (
            "aws_access_key_id".into(),
            env_or("FIRNFLOW_S3_ACCESS_KEY", "minioadmin"),
        ),
        (
            "aws_secret_access_key".into(),
            env_or("FIRNFLOW_S3_SECRET_KEY", "minioadmin"),
        ),
        (
            "aws_endpoint".into(),
            env_or("FIRNFLOW_S3_ENDPOINT", "http://127.0.0.1:9000"),
        ),
        ("aws_region".into(), "us-east-1".into()),
        ("allow_http".into(), "true".into()),
        ("aws_virtual_hosted_style_request".into(), "false".into()),
    ])
}

/// The warmup test needs to *observe* the metrics after the
/// spawned task runs, so it holds onto the metrics handle
/// directly instead of scraping `/metrics`.
async fn build_app_with_metrics() -> (axum::Router, tempfile::TempDir, Arc<CoreMetrics>) {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let tmp = tempfile::tempdir().unwrap();
    let metrics = test_metrics();
    let manager = Arc::new(NamespaceManager::new(
        bucket,
        minio_options(),
        Arc::clone(&metrics),
    ));
    let cache = Arc::new(
        NamespaceCache::new(
            16 * 1024 * 1024,
            tmp.path(),
            64 * 1024 * 1024,
            Arc::clone(&metrics),
        )
        .await
        .unwrap(),
    );
    let service = Arc::new(NamespaceService::new(manager, cache, Arc::clone(&metrics)));
    let app = router(AppState {
        service,
        metrics: Arc::clone(&metrics),
    });
    (app, tmp, metrics)
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

/// Poll the metrics registry until `cache_misses_total{ns}` reaches
/// `expected` or the deadline expires.
async fn wait_for_misses(
    metrics: &Arc<CoreMetrics>,
    ns: &str,
    expected: u64,
    deadline: Duration,
) -> u64 {
    let start = std::time::Instant::now();
    let label = format!(r#"namespace="{ns}""#);
    loop {
        let body = metrics.encode().unwrap();
        let misses = metric_value(&body, "firnflow_cache_misses_total", &label)
            .map(|v| v as u64)
            .unwrap_or(0);
        if misses >= expected || start.elapsed() >= deadline {
            return misses;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
#[ignore]
async fn warmup_returns_202_and_populates_cache() {
    let (app, _tmp, metrics) = build_app_with_metrics().await;
    let ns = unique_namespace("slice3b");

    // Upsert some rows so the warmup queries have something to find.
    let upsert_body = json!({
        "rows": [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
            {"id": 3, "vector": [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
        ]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK);

    // POST /warmup with three distinct queries.
    let warmup_body = json!({
        "queries": [
            {"vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], "k": 3},
            {"vector": [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], "k": 3},
            {"vector": [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0], "k": 3}
        ]
    });
    let request = Request::builder()
        .method("POST")
        .uri(format!("/ns/{ns}/warmup"))
        .header("content-type", "application/json")
        .body(Body::from(warmup_body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "warmup must return 202 Accepted synchronously"
    );
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["queued"], 3);

    // Wait for the background task to drain. We're looking for the
    // misses counter to reach 3 — one per warmup query — which proves
    // the spawned task actually ran and went through the cache-aside
    // path. The deadline is generous enough that CI flakiness on a
    // slow MinIO doesn't matter.
    let misses = wait_for_misses(&metrics, &ns, 3, Duration::from_secs(5)).await;
    assert_eq!(
        misses, 3,
        "warmup should have produced 3 cache misses (one per query)"
    );

    // After warmup, querying the same vectors must cache-hit. One
    // additional query through the HTTP path and check the hits
    // counter moved.
    let query_body = json!({
        "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "k": 3
    });
    let (status, _) = post_json(app, format!("/ns/{ns}/query"), query_body).await;
    assert_eq!(status, StatusCode::OK);

    let body = metrics.encode().unwrap();
    let label = format!(r#"namespace="{ns}""#);
    let hits = metric_value(&body, "firnflow_cache_hits_total", &label)
        .map(|v| v as u64)
        .unwrap_or(0);
    assert_eq!(
        hits, 1,
        "post-warmup query should have been served from cache — hits must have moved by 1"
    );
}
