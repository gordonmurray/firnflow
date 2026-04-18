//! Integration test for `GET /ns/{namespace}/list` (issue #22).
//!
//! Drives the axum router via `tower::ServiceExt::oneshot`, so no TCP
//! listener is required. Covers:
//!
//! - happy path: upsert rows, list them back, assert order + shape,
//! - pagination via `?limit` + `?cursor`,
//! - 400 on bogus `order_by` / malformed cursor / over-limit,
//! - cache bypass: hit + miss counters do not advance across list
//!   calls (the foyer cache is owned by `NamespaceService`, and
//!   `/list` goes through the manager directly).
//!
//! Gated `#[ignore]`: needs MinIO up.

use std::collections::HashMap;
use std::sync::Arc;

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

async fn build_app() -> (axum::Router, Arc<CoreMetrics>, tempfile::TempDir) {
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
    let service = Arc::new(NamespaceService::new(
        Arc::clone(&manager),
        cache,
        Arc::clone(&metrics),
    ));
    let app = router(AppState {
        service,
        manager,
        metrics: Arc::clone(&metrics),
    });
    (app, metrics, tmp)
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

async fn get_metrics_text(app: axum::Router) -> String {
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

fn sum_counter(metrics_text: &str, metric: &str) -> u64 {
    metrics_text
        .lines()
        .filter(|l| l.starts_with(metric) && !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().last())
        .filter_map(|v| v.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}

#[tokio::test]
#[ignore]
async fn list_happy_path_desc_and_pagination() {
    let (app, _metrics, _tmp) = build_app().await;
    let ns = unique_namespace("issue22-api");

    // Seed two batches with a sleep so their _ingested_at timestamps
    // differ. Within a batch, order is decided by id.
    for batch in 0..2 {
        let base = batch * 4;
        let body = json!({
            "rows": [
                {"id": base, "vector": [1.0, 0.0, 0.0, 0.0]},
                {"id": base + 1, "vector": [0.0, 1.0, 0.0, 0.0]},
                {"id": base + 2, "vector": [0.0, 0.0, 1.0, 0.0]},
                {"id": base + 3, "vector": [0.0, 0.0, 0.0, 1.0]},
            ]
        });
        let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), body).await;
        assert_eq!(status, StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // First page — limit=3 forces a cursor.
    let (status, page1) = get(app.clone(), format!("/ns/{ns}/list?limit=3")).await;
    assert_eq!(status, StatusCode::OK, "first page: {page1}");
    let rows1 = page1["rows"].as_array().expect("rows array");
    assert_eq!(rows1.len(), 3, "first page returns `limit` rows");
    let cursor = page1["next_cursor"]
        .as_str()
        .expect("cursor present mid-stream")
        .to_string();

    // Ids on page 1 must be the three most-recent. Batch 2 (ids 4..7)
    // shares a timestamp, so desc order within it is 7, 6, 5.
    let page1_ids: Vec<u64> = rows1.iter().map(|r| r["id"].as_u64().unwrap()).collect();
    assert_eq!(page1_ids, vec![7, 6, 5]);

    // Second page — continue via cursor, pull the rest.
    let (status, page2) = get(
        app.clone(),
        format!("/ns/{ns}/list?limit=10&cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows2 = page2["rows"].as_array().unwrap();
    let page2_ids: Vec<u64> = rows2.iter().map(|r| r["id"].as_u64().unwrap()).collect();
    assert_eq!(
        page2_ids,
        vec![4, 3, 2, 1, 0],
        "remaining rows in desc order"
    );
    assert!(
        page2["next_cursor"].is_null(),
        "final page must omit next_cursor"
    );

    // Shape spot-check on the first row: must carry ingested_at_micros.
    let first = &rows1[0];
    assert!(
        first["ingested_at_micros"].as_i64().unwrap() > 0,
        "ingested_at_micros must be populated"
    );
    assert_eq!(first["vector"].as_array().unwrap().len(), 4);
}

#[tokio::test]
#[ignore]
async fn list_rejects_unsupported_order_by() {
    let (app, _metrics, _tmp) = build_app().await;
    let ns = unique_namespace("issue22-order-by");

    let body = json!({
        "rows": [{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0]}]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), body).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(app, format!("/ns/{ns}/list?order_by=created_at")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("order_by"),
        "expected order_by error, got {err}"
    );
}

#[tokio::test]
#[ignore]
async fn list_rejects_malformed_cursor() {
    let (app, _metrics, _tmp) = build_app().await;
    let ns = unique_namespace("issue22-cursor");

    let body = json!({
        "rows": [{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0]}]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), body).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = get(app, format!("/ns/{ns}/list?cursor=not-hex")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore]
async fn list_rejects_over_limit() {
    let (app, _metrics, _tmp) = build_app().await;
    // Namespace does not need to exist — limit validation runs first.
    let (status, body) = get(app, "/ns/anything/list?limit=10000".to_string()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap_or_default();
    assert!(err.contains("limit"), "expected limit error, got {err}");
}

#[tokio::test]
#[ignore]
async fn list_bypasses_foyer_cache() {
    let (app, _metrics, _tmp) = build_app().await;
    let ns = unique_namespace("issue22-cache");

    // Seed a row.
    let upsert_body = json!({
        "rows": [{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0]}]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK);

    // Baseline cache counters *before* any list call. Counters are
    // captured after the upsert so any cache eviction from the write
    // has already happened.
    let text0 = get_metrics_text(app.clone()).await;
    let hits0 = sum_counter(&text0, "firnflow_cache_hits_total");
    let misses0 = sum_counter(&text0, "firnflow_cache_misses_total");

    // Call list twice — both must skip the foyer cache.
    for _ in 0..2 {
        let (status, _) = get(app.clone(), format!("/ns/{ns}/list?limit=10")).await;
        assert_eq!(status, StatusCode::OK);
    }

    let text1 = get_metrics_text(app).await;
    let hits1 = sum_counter(&text1, "firnflow_cache_hits_total");
    let misses1 = sum_counter(&text1, "firnflow_cache_misses_total");

    assert_eq!(
        hits1, hits0,
        "cache hit counter advanced across list calls: {hits0} -> {hits1}"
    );
    assert_eq!(
        misses1, misses0,
        "cache miss counter advanced across list calls: {misses0} -> {misses1}"
    );
}
