//! Tests for the `num_bits` field on `POST /ns/{namespace}/index`.
//!
//! Two layers:
//!
//! - Synchronous-validation tests (no MinIO needed). Validate that
//!   bad `num_bits` payloads return 400 *before* the handler spawns
//!   the background index task, so the caller learns about the
//!   error directly rather than via a stray log entry behind a
//!   misleading 202.
//! - Happy-path test (gated `#[ignore]`, needs MinIO). Builds a real
//!   4-bit IVF_PQ index and confirms the indexed namespace still
//!   returns sane query results.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_index_num_bits -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use firnflow_api::router;
use firnflow_core::CoreMetrics;
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{test_state, test_state_offline, unique_namespace};

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

fn make_vector(seed: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| (((seed * 7919 + j * 31) as f32) * 0.001).sin())
        .collect()
}

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
async fn num_bits_seven_returns_400_synchronously() {
    let (state, _tmp) = test_state_offline().await;
    let app = router(state);
    let ns = unique_namespace("num-bits-bad");

    let body = json!({
        "kind": "ivf_pq",
        "num_bits": 7
    });
    let (status, response) = post_json(app, format!("/ns/{ns}/index"), body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unsupported num_bits must reject synchronously, not behind a 202"
    );
    let msg = response["error"].as_str().expect("error message");
    assert!(msg.contains("num_bits=7"), "missing offending value: {msg}");
    assert!(msg.contains("4 or 8"), "missing accepted values: {msg}");
}

#[tokio::test]
async fn num_bits_four_with_odd_sub_vectors_returns_400_synchronously() {
    let (state, _tmp) = test_state_offline().await;
    let app = router(state);
    let ns = unique_namespace("num-bits-odd");

    let body = json!({
        "kind": "ivf_pq",
        "num_bits": 4,
        "num_sub_vectors": 63
    });
    let (status, response) = post_json(app, format!("/ns/{ns}/index"), body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "4-bit PQ with odd num_sub_vectors must reject before spawn"
    );
    let msg = response["error"].as_str().expect("error message");
    assert!(msg.contains("num_bits=4"), "missing bit width: {msg}");
    assert!(msg.contains("even"), "missing parity constraint: {msg}");
    assert!(msg.contains("63"), "missing offending count: {msg}");
}

#[tokio::test]
#[ignore]
async fn index_build_with_num_bits_four_returns_202_and_completes() {
    let (state, _tmp) = test_state().await;
    let metrics = Arc::clone(&state.metrics);
    let app = router(state);
    let ns = unique_namespace("num-bits-4");

    let dim = 32;
    let rows: Vec<Value> = (0..256)
        .map(|i| {
            let v = make_vector(i, dim);
            json!({ "id": i, "vector": v })
        })
        .collect();
    let (status, _) = post_json(
        app.clone(),
        format!("/ns/{ns}/upsert"),
        json!({ "rows": rows }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "upsert must succeed");

    // num_sub_vectors=4 is even, which is the constraint 4-bit PQ
    // imposes. sub_dim = dim / num_sub_vectors = 32 / 4 = 8, well
    // clear of Lance 6's degenerate-codebook threshold.
    let index_body = json!({
        "kind": "ivf_pq",
        "num_partitions": 4,
        "num_sub_vectors": 4,
        "num_bits": 4
    });
    let (status, body) = post_json(app.clone(), format!("/ns/{ns}/index"), index_body).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        body["operation_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("op_")),
        "202 should carry an operation id: {body}"
    );
    assert_eq!(body["status"], "running");

    let count = wait_for_index_build(&metrics, &ns, 1, Duration::from_secs(60)).await;
    assert_eq!(count, 1, "4-bit index build must complete");

    let query_vector = make_vector(0, dim);
    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/query"),
        json!({ "vector": query_vector, "k": 3, "nprobes": 4 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3);
    // 4-bit PQ is intentionally lossier than the 8-bit default. With
    // 256 training rows and num_sub_vectors=4, each codebook holds
    // only 16 codes over an 8-dim sub-vector, so this toy corpus is
    // too small to make a stable recall assertion. The test above
    // already proves the background build completed; this query check
    // keeps the HTTP read path and row materialisation covered without
    // assuming a specific approximate top-k ordering.
    let top_ids: Vec<i64> = results
        .iter()
        .map(|r| r["id"].as_i64().expect("id must be integer"))
        .collect();
    assert!(
        top_ids.iter().all(|id| (0..256).contains(id)),
        "indexed query must return inserted row ids (got {top_ids:?})"
    );
    let mut unique_ids = top_ids.clone();
    unique_ids.sort_unstable();
    unique_ids.dedup();
    assert_eq!(
        unique_ids.len(),
        top_ids.len(),
        "duplicate ids: {top_ids:?}"
    );
    for result in results {
        let vector = result["vector"].as_array().expect("vector array");
        assert_eq!(vector.len(), dim, "returned vector must keep dimension");
    }
}
