//! Slice-6b integration test: `POST /ns/{namespace}/index` builds
//! an IVF_PQ index asynchronously, and the
//! `firnflow_index_build_duration_seconds` histogram records the
//! build time.
//!
//! The test upserts enough rows to make index creation meaningful,
//! fires the index endpoint, polls the metrics registry until the
//! `_count` on the histogram ticks over (proving the background
//! task ran to completion), then issues a query through the HTTP
//! path to confirm the indexed namespace still returns correct
//! results.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_index -- --ignored --nocapture
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

/// Generate a deterministic vector of the given dimension.
fn make_vector(seed: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| (((seed * 7919 + j * 31) as f32) * 0.001).sin())
        .collect()
}

/// Poll the metrics registry until `index_build_duration_seconds_count`
/// for this namespace reaches `expected`, or the deadline expires.
async fn wait_for_index_build(
    metrics: &Arc<CoreMetrics>,
    ns: &str,
    expected: u64,
    deadline: Duration,
) -> u64 {
    let start = std::time::Instant::now();
    let label = format!(r#"namespace="{ns}""#);
    loop {
        let body = metrics.encode().unwrap();
        let count = metric_value(&body, "firnflow_index_build_duration_seconds_count", &label)
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
async fn index_build_returns_202_and_records_metric() {
    let (app, _tmp, metrics) = build_app_with_metrics().await;
    let ns = unique_namespace("slice6b");

    // 1. Upsert 256 rows at dim=32. IVF_PQ needs enough data to
    //    form at least a few partitions — 256 rows is the minimum
    //    that won't degenerate into a single-partition index.
    let dim = 32;
    let rows: Vec<Value> = (0..256)
        .map(|i| {
            let v = make_vector(i, dim);
            json!({"id": i, "vector": v})
        })
        .collect();
    let upsert_body = json!({ "rows": rows });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK, "upsert must succeed");

    // 2. POST /index — must return 202 immediately.
    let index_body = json!({
        "kind": "ivf_pq",
        "num_partitions": 4,
        "num_sub_vectors": 4
    });
    let (status, body) = post_json(app.clone(), format!("/ns/{ns}/index"), index_body).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "index endpoint must return 202"
    );
    assert_eq!(body["status"], "index build queued");

    // 3. Wait for the background task to complete by polling the
    //    index_build_duration histogram count.
    let count = wait_for_index_build(&metrics, &ns, 1, Duration::from_secs(60)).await;
    assert_eq!(
        count, 1,
        "index_build_duration_seconds_count must tick to 1"
    );

    // 4. Verify the index_build_duration is actually recorded
    //    (the _sum should be > 0).
    let body = metrics.encode().unwrap();
    let ns_label = format!(r#"namespace="{ns}""#);
    let sum = metric_value(
        &body,
        "firnflow_index_build_duration_seconds_sum",
        &ns_label,
    )
    .unwrap_or(0.0);
    assert!(
        sum > 0.0,
        "index_build_duration_seconds_sum must be > 0, got {sum}"
    );

    // 5. Query the indexed namespace — results must still be correct.
    let query_vector = make_vector(0, dim);
    let query_body = json!({
        "vector": query_vector,
        "k": 3,
        "nprobes": 4
    });
    let (status, body) = post_json(app.clone(), format!("/ns/{ns}/query"), query_body).await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3, "expected top-3 results");
    assert_eq!(
        results[0]["id"], 0,
        "nearest neighbour of vector 0 must be id=0"
    );

    // 6. Verify unsupported index kind returns 400.
    let bad_body = json!({"kind": "hnsw"});
    let (status, body) = post_json(app, format!("/ns/{ns}/index"), bad_body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("unsupported index kind"),
        "unexpected error: {body}"
    );
}
