//! Integration test for `POST /ns/{namespace}/scalar-index` (issue #24).
//!
//! Builds a BTree scalar index on `_ingested_at` asynchronously and
//! verifies:
//!
//! - the endpoint returns 202 Accepted with the expected status string,
//! - the background task runs to completion (the
//!   `firnflow_index_build_duration_seconds_count{kind="scalar"}`
//!   histogram counter ticks over),
//! - `/list` continues to return correctly-ordered, paginated results
//!   *with* the index in place,
//! - a parallel namespace that never built a scalar index returns
//!   identically-shaped, correct results — the fallback (no-index)
//!   path still works,
//! - calling `/scalar-index` a second time on the same namespace is
//!   idempotent (BTree builder defaults to `replace=true`),
//! - a fresh, never-upserted namespace returns a graceful failure
//!   inside the background task (logged, does not panic).
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_scalar_index \
//!     -- --ignored --nocapture
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

async fn post_empty(app: axum::Router, uri: String) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
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

/// Poll the metrics registry until
/// `firnflow_index_build_duration_seconds_count` for this
/// `(namespace, kind)` pair reaches `expected`, or the deadline
/// expires. Returns the last observed count.
///
/// Prometheus emits labels alphabetically (`kind` before `namespace`)
/// — see CLAUDE.md "Prometheus label ordering is alphabetical."
async fn wait_for_index_build(
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

/// Seed a namespace with `batches` batches of 4 rows each, sleeping
/// 5 ms between batches so each batch carries a distinct
/// `_ingested_at` microsecond timestamp. Within a batch all rows
/// share the same timestamp; the secondary `id` ordering applies.
async fn seed_namespace(app: &axum::Router, ns: &str, batches: usize) {
    for batch in 0..batches {
        let base = (batch * 4) as u64;
        let body = json!({
            "rows": [
                {"id": base,     "vector": [1.0, 0.0, 0.0, 0.0]},
                {"id": base + 1, "vector": [0.0, 1.0, 0.0, 0.0]},
                {"id": base + 2, "vector": [0.0, 0.0, 1.0, 0.0]},
                {"id": base + 3, "vector": [0.0, 0.0, 0.0, 1.0]},
            ]
        });
        let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), body).await;
        assert_eq!(status, StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Walk every page of `/list` and return the collected ids in the
/// order the server returned them.
async fn list_all_ids(app: &axum::Router, ns: &str, limit: usize) -> Vec<u64> {
    let mut ids: Vec<u64> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let uri = match &cursor {
            Some(c) => format!("/ns/{ns}/list?limit={limit}&cursor={c}"),
            None => format!("/ns/{ns}/list?limit={limit}"),
        };
        let (status, page) = get(app.clone(), uri).await;
        assert_eq!(status, StatusCode::OK, "list page: {page}");
        for row in page["rows"].as_array().unwrap() {
            ids.push(row["id"].as_u64().unwrap());
        }
        match page["next_cursor"].as_str() {
            Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
            _ => break,
        }
    }
    ids
}

#[tokio::test]
#[ignore]
async fn scalar_index_build_returns_202_and_list_still_works() {
    let (app, _tmp, metrics) = build_app_with_metrics().await;
    let ns_indexed = unique_namespace("issue24-indexed");
    let ns_unindexed = unique_namespace("issue24-unindexed");

    // Seed both namespaces with the same shape: 4 batches of 4 rows
    // (16 rows total). Multiple batches give multiple distinct
    // `_ingested_at` timestamps so the BTree has range structure
    // worth keeping.
    seed_namespace(&app, &ns_indexed, 4).await;
    seed_namespace(&app, &ns_unindexed, 4).await;

    // 1. POST /scalar-index — must return 202 immediately.
    let (status, body) = post_empty(app.clone(), format!("/ns/{ns_indexed}/scalar-index")).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "scalar-index endpoint must return 202"
    );
    assert_eq!(body["status"], "scalar index build queued");

    // 2. Wait for the background task to complete.
    let count =
        wait_for_index_build(&metrics, &ns_indexed, "scalar", 1, Duration::from_secs(60)).await;
    assert_eq!(
        count, 1,
        "index_build_duration_seconds_count{{kind=\"scalar\"}} must tick to 1"
    );

    // 3. The histogram _sum must be > 0.
    let body = metrics.encode().unwrap();
    let label = format!(r#"kind="scalar",namespace="{ns_indexed}""#);
    let sum =
        metric_value(&body, "firnflow_index_build_duration_seconds_sum", &label).unwrap_or(0.0);
    assert!(
        sum > 0.0,
        "index_build_duration_seconds_sum must be > 0, got {sum}"
    );

    // 4. List the indexed namespace — full pagination via the BTree
    //    path. Expected order: desc by (ingested_at, id), so the
    //    last-batch ids 12..15 come first (15, 14, 13, 12), then
    //    11..8, then 7..4, then 3..0.
    let indexed_ids = list_all_ids(&app, &ns_indexed, 5).await;
    let expected: Vec<u64> = (0..16).rev().collect();
    assert_eq!(
        indexed_ids, expected,
        "indexed namespace pagination order broken"
    );

    // 5. Same shape against the namespace with no scalar index —
    //    proves the fallback (full-scan) path still returns correct
    //    results. This is the issue's "list still returns correct
    //    results in both cases" verification bullet.
    let unindexed_ids = list_all_ids(&app, &ns_unindexed, 5).await;
    assert_eq!(
        unindexed_ids, expected,
        "unindexed namespace must return identical results"
    );

    // 6. Idempotency: a second POST must also succeed and tick the
    //    histogram a second time. lancedb's IndexBuilder defaults to
    //    `replace=true`, so this rebuilds in place.
    let (status, _) = post_empty(app.clone(), format!("/ns/{ns_indexed}/scalar-index")).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let count =
        wait_for_index_build(&metrics, &ns_indexed, "scalar", 2, Duration::from_secs(60)).await;
    assert_eq!(count, 2, "second build must increment the counter");

    // 7. Post-rebuild list must still return the same 16 rows in the
    //    same order — the index swap must not corrupt the read path.
    let after_rebuild = list_all_ids(&app, &ns_indexed, 5).await;
    assert_eq!(after_rebuild, expected);
}

#[tokio::test]
#[ignore]
async fn scalar_index_on_empty_namespace_does_not_panic() {
    // POSTing scalar-index to a namespace with no upserted rows
    // must not panic the worker — the manager surfaces an
    // `InvalidRequest`, the handler logs it, the test's job is to
    // confirm no 5xx leaks back through the HTTP layer (the spawn
    // returns 202 regardless) and that subsequent operations on
    // the same namespace still work.
    let (app, _tmp, metrics) = build_app_with_metrics().await;
    let ns = unique_namespace("issue24-empty");

    let (status, body) = post_empty(app.clone(), format!("/ns/{ns}/scalar-index")).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "scalar index build queued");

    // The build will have failed inside the background task. Confirm
    // the histogram for this namespace does *not* tick, then upsert
    // and try again to prove the namespace is still usable.
    let count = wait_for_index_build(&metrics, &ns, "scalar", 1, Duration::from_secs(2)).await;
    assert_eq!(
        count, 0,
        "build on an empty namespace must not record a duration sample"
    );

    let upsert_body = json!({
        "rows": [{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0]}]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = post_empty(app.clone(), format!("/ns/{ns}/scalar-index")).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let count = wait_for_index_build(&metrics, &ns, "scalar", 1, Duration::from_secs(60)).await;
    assert_eq!(
        count, 1,
        "second attempt (with rows) must complete successfully"
    );
}
