//! Synchronous-validation tests for the `kind` field on
//! `POST /ns/{namespace}/index`.
//!
//! These tests exercise the handler's early-return checks that run before
//! the background index task is spawned, so they do not need MinIO and
//! do not carry `#[ignore]`.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use firnflow_api::router;
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;
use common::{test_state_offline, unique_namespace};

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
async fn unknown_kind_returns_400_synchronously() {
    let (state, _tmp) = test_state_offline().await;
    let app = router(state);
    let ns = unique_namespace("index-kind-bad");

    let (status, response) = post_json(
        app,
        format!("/ns/{ns}/index"),
        json!({ "kind": "ivf_hnsw" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unknown kind must reject before spawning the background task: {response}"
    );
    let msg = response["error"].as_str().expect("error message");
    assert!(msg.contains("ivf_hnsw"), "missing offending kind: {msg}");
    assert!(
        msg.contains("ivf_pq") && msg.contains("ivf_rq"),
        "missing valid kinds in error: {msg}"
    );
}

#[tokio::test]
async fn ivf_rq_with_num_sub_vectors_returns_400_synchronously() {
    let (state, _tmp) = test_state_offline().await;
    let app = router(state);
    let ns = unique_namespace("index-rq-bad");

    let (status, response) = post_json(
        app,
        format!("/ns/{ns}/index"),
        json!({ "kind": "ivf_rq", "num_sub_vectors": 8 }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "ivf_rq with num_sub_vectors must reject synchronously: {response}"
    );
    let msg = response["error"].as_str().expect("error message");
    assert!(
        msg.contains("num_sub_vectors"),
        "error must name the rejected field: {msg}"
    );
}
