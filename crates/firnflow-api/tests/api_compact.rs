//! Slice-6c integration test: `POST /ns/{namespace}/compact` merges
//! small data files into fewer, larger ones.
//!
//! The test creates fragment explosion by upserting 200 rows across
//! 50 separate upsert calls (each call appends a new data file),
//! then fires the compact endpoint and verifies that the
//! `firnflow_compaction_duration_seconds` histogram recorded the
//! compaction. Data integrity is verified by querying after
//! compaction.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_compact -- --ignored --nocapture
//! ```

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

/// Poll until `compaction_duration_seconds_count` for `ns` reaches
/// `expected` or the deadline expires.
async fn wait_for_compaction(
    metrics: &Arc<CoreMetrics>,
    ns: &str,
    expected: u64,
    deadline: Duration,
) -> u64 {
    let start = std::time::Instant::now();
    let label = format!(r#"namespace="{ns}""#);
    loop {
        let body = metrics.encode().unwrap();
        let count = metric_value(&body, "firnflow_compaction_duration_seconds_count", &label)
            .map(|v| v as u64)
            .unwrap_or(0);
        if count >= expected || start.elapsed() >= deadline {
            return count;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore]
async fn compact_reduces_fragments_after_many_upserts() {
    let (app, _tmp, metrics) = build_app_with_metrics().await;
    let ns = unique_namespace("slice6c");
    let dim = 8;

    // 1. Create fragment explosion: 50 separate upsert calls of
    //    4 rows each = 200 rows across 50 data files.
    println!("upserting 200 rows in 50 batches...");
    for batch in 0..50 {
        let rows: Vec<Value> = (0..4)
            .map(|i| {
                let id = batch * 4 + i;
                let v: Vec<f32> = (0..dim)
                    .map(|j| (((id * 7919 + j * 31) as f32) * 0.001).sin())
                    .collect();
                json!({"id": id, "vector": v})
            })
            .collect();
        let (status, _) = post_json(
            app.clone(),
            format!("/ns/{ns}/upsert"),
            json!({ "rows": rows }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "upsert batch {batch} failed");
    }

    // 2. Verify all rows are queryable pre-compaction.
    let query_vec: Vec<f32> = (0..dim)
        .map(|j| (((0 * 7919 + j * 31) as f32) * 0.001).sin())
        .collect();
    let (status, body) = post_json(
        app.clone(),
        format!("/ns/{ns}/query"),
        json!({"vector": query_vec, "k": 5}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["results"].as_array().unwrap().len(),
        5,
        "pre-compact query should find rows"
    );

    // 3. POST /compact — must return 202 immediately.
    let request = Request::builder()
        .method("POST")
        .uri(format!("/ns/{ns}/compact"))
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "compact must return 202"
    );
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let compact_body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(compact_body["status"], "compaction queued");

    // 4. Wait for the background task to complete.
    let count = wait_for_compaction(&metrics, &ns, 1, Duration::from_secs(60)).await;
    assert_eq!(count, 1, "compaction_duration_seconds_count must tick to 1");

    // 5. Verify the compaction_duration is recorded (_sum > 0).
    let body = metrics.encode().unwrap();
    let ns_label = format!(r#"namespace="{ns}""#);
    let sum =
        metric_value(&body, "firnflow_compaction_duration_seconds_sum", &ns_label).unwrap_or(0.0);
    assert!(
        sum > 0.0,
        "compaction_duration_seconds_sum must be > 0, got {sum}"
    );

    // 6. Verify data integrity: query still returns results after
    //    compaction. The cache was invalidated by the service, so
    //    this goes through the compacted data files.
    let query_vec: Vec<f32> = (0..dim)
        .map(|j| (((0 * 7919 + j * 31) as f32) * 0.001).sin())
        .collect();
    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/query"),
        json!({"vector": query_vec, "k": 5}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        5,
        "post-compact query must still find rows — data integrity preserved"
    );
    assert_eq!(
        results[0]["id"], 0,
        "nearest neighbour of vector 0 must still be id=0"
    );
}
