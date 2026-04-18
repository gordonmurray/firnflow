//! Slice-3a integration test: `DELETE /ns/{namespace}` removes
//! every S3 object under the namespace prefix and invalidates the
//! cache, so that a subsequent query against the same namespace
//! sees an empty table and counts as a cache miss (not a stale hit).
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_delete -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use firnflow_api::{router, AppState};
use firnflow_core::cache::NamespaceCache;
use firnflow_core::metrics::test_metrics;
use firnflow_core::{NamespaceManager, NamespaceService};
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

async fn build_app() -> (axum::Router, tempfile::TempDir) {
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
        metrics,
    });
    (app, tmp)
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

#[tokio::test]
#[ignore]
async fn delete_removes_namespace_and_invalidates_cache() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("slice3a");

    // 1. Upsert three rows.
    let upsert_body = json!({
        "rows": [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
            {"id": 3, "vector": [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
        ]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK);

    // 2. Query once to populate the cache with a 3-result payload.
    let query_body = json!({
        "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "k": 10
    });
    let (status, body) =
        post_json(app.clone(), format!("/ns/{ns}/query"), query_body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["results"].as_array().unwrap().len(),
        3,
        "pre-delete query must see all three rows"
    );

    // 3. DELETE the namespace.
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/ns/{ns}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let delete_body: Value = serde_json::from_slice(&bytes).unwrap();
    let deleted = delete_body["objects_deleted"].as_u64().unwrap();
    assert!(
        deleted > 0,
        "expected to delete at least one S3 object, got {deleted}"
    );

    // 4. Query again. This is the load-bearing assertion: if the
    //    cache weren't invalidated, we'd get the pre-delete 3-result
    //    set back as a stale hit. If the delete didn't actually remove
    //    the underlying Lance files, the manager would re-open the
    //    existing table and still see 3 rows. Getting 0 rows back
    //    means both sides of the story are correct.
    let (status, body) = post_json(app, format!("/ns/{ns}/query"), query_body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["results"].as_array().unwrap().len(),
        0,
        "post-delete query must see an empty namespace — no stale cache, no leftover S3 state"
    );
}
