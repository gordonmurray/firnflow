//! Phase-7 integration test: full-text search and hybrid queries.
//!
//! Covers three query modes on the same namespace:
//! 1. Vector-only query returns nearest neighbours
//! 2. FTS-only query returns text-matching results
//! 3. Hybrid (vector + text) query returns combined results via RRF
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_fts -- --ignored --nocapture
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
use common::{test_state, unique_namespace};

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

async fn build_app_with_metrics() -> (axum::Router, tempfile::TempDir, Arc<CoreMetrics>) {
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

async fn wait_for_index(
    metrics: &Arc<CoreMetrics>,
    ns: &str,
    kind: &str,
    expected: u64,
    deadline: Duration,
) -> u64 {
    let start = std::time::Instant::now();
    let label = format!(r#"kind="{kind}",namespace="{ns}""#);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn vector_fts_and_hybrid_queries() {
    let (app, _tmp, metrics) = build_app_with_metrics().await;
    let ns = unique_namespace("phase7-fts");
    // 1. Upsert rows with both vectors and text.
    let upsert_body = json!({
        "rows": [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
             "text": "the quick brown fox jumps over the lazy dog"},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
             "text": "a fast red car drives through the city streets"},
            {"id": 3, "vector": [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
             "text": "the lazy cat sleeps on the warm windowsill"},
            {"id": 4, "vector": [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
             "text": "quick brown bread bakes in the old stone oven"},
        ]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK, "upsert must succeed");

    // 2. Build FTS index via the HTTP endpoint (202 async).
    let request = Request::builder()
        .method("POST")
        .uri(format!("/ns/{ns}/fts-index"))
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "fts-index must return 202"
    );

    // Wait for the FTS index build to complete via the metric.
    let count = wait_for_index(&metrics, &ns, "fts", 1, Duration::from_secs(30)).await;
    assert_eq!(count, 1, "FTS index build must complete");

    // 3. Vector-only query — nearest to [1,0,0,...] should be id=1.
    let query_body = json!({
        "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "k": 3
    });
    let (status, body) = post_json(app.clone(), format!("/ns/{ns}/query"), query_body).await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "vector query must return results");
    assert_eq!(
        results[0]["id"], 1,
        "vector nearest to [1,0,...] must be id=1"
    );
    // Vector results should include text.
    assert!(
        results[0]["text"].is_string(),
        "vector results should include stored text"
    );

    // 4. FTS-only query — search for "lazy" should find ids 1 and 3.
    let fts_body = json!({
        "text": "lazy",
        "k": 4
    });
    let (status, body) = post_json(app.clone(), format!("/ns/{ns}/query"), fts_body).await;
    assert_eq!(status, StatusCode::OK, "FTS query status");
    let results = body["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "FTS query for 'lazy' must return results"
    );
    let fts_ids: Vec<u64> = results.iter().map(|r| r["id"].as_u64().unwrap()).collect();
    assert!(
        fts_ids.contains(&1) && fts_ids.contains(&3),
        "FTS for 'lazy' should match ids 1 and 3, got {fts_ids:?}"
    );

    // 5. Hybrid query — vector close to id=1 + text "lazy".
    //    Should combine vector similarity and text relevance.
    let hybrid_body = json!({
        "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "text": "lazy",
        "k": 4
    });
    let (status, body) = post_json(app.clone(), format!("/ns/{ns}/query"), hybrid_body).await;
    assert_eq!(status, StatusCode::OK, "hybrid query status");
    let results = body["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "hybrid query must return results");
    // id=1 should rank highly — it's both the nearest vector and
    // contains "lazy".
    assert_eq!(
        results[0]["id"], 1,
        "hybrid query: id=1 is both nearest vector and matches 'lazy'"
    );

    // 6. Verify query type metrics recorded correctly.
    let body = metrics.encode().unwrap();
    let vector_count = metric_value(
        &body,
        "firnflow_query_duration_seconds_count",
        &format!(r#"namespace="{ns}",query_type="vector""#),
    )
    .unwrap_or(0.0);
    let fts_count = metric_value(
        &body,
        "firnflow_query_duration_seconds_count",
        &format!(r#"namespace="{ns}",query_type="fts""#),
    )
    .unwrap_or(0.0);
    let hybrid_count = metric_value(
        &body,
        "firnflow_query_duration_seconds_count",
        &format!(r#"namespace="{ns}",query_type="hybrid""#),
    )
    .unwrap_or(0.0);
    assert_eq!(vector_count, 1.0, "expected 1 vector query");
    assert_eq!(fts_count, 1.0, "expected 1 FTS query");
    assert_eq!(hybrid_count, 1.0, "expected 1 hybrid query");
}
