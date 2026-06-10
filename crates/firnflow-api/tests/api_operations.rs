//! Integration test for background-operation tracking (issue #34).
//!
//! Exercises the `GET /operations/{operation_id}` status endpoint: a
//! scalar-index build returns an operation handle in its 202, which we
//! poll to a terminal state, and an unknown id returns 404. Polling the
//! operation status is the pattern that replaces inferring completion
//! from a Prometheus histogram.

use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use firnflow_api::router;
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
    response_json(app.oneshot(request).await.unwrap()).await
}

async fn post_empty(app: axum::Router, uri: String) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    response_json(app.oneshot(request).await.unwrap()).await
}

async fn get(app: axum::Router, uri: String) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    response_json(app.oneshot(request).await.unwrap()).await
}

async fn response_json(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

/// Poll `GET /operations/{id}` until it reaches a terminal state or the
/// deadline passes; returns the final record.
async fn poll_operation(app: &axum::Router, op_id: &str, timeout: Duration) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (status, body) = get(app.clone(), format!("/operations/{op_id}")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "operation should be retrievable: {body}"
        );
        if matches!(body["status"].as_str(), Some("succeeded") | Some("failed")) {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "operation {op_id} did not reach a terminal state in time: {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore]
async fn scalar_index_operation_reaches_succeeded() {
    let (state, _tmp) = test_state().await;
    let app = router(state);
    let ns = unique_namespace("op-scalar");

    // Seed a row so the namespace and its `_ingested_at` column exist.
    let (status, _) = post_json(
        app.clone(),
        format!("/ns/{ns}/upsert"),
        json!({"rows": [{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0]}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Kick off the scalar index build and capture the operation handle.
    let (status, accepted) = post_empty(app.clone(), format!("/ns/{ns}/scalar-index")).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(accepted["kind"], "scalar_index");
    assert_eq!(accepted["namespace"], ns);
    assert_eq!(accepted["status"], "running");
    let op_id = accepted["operation_id"]
        .as_str()
        .expect("202 carries an operation id")
        .to_string();
    assert!(op_id.starts_with("op_"));

    // Poll the operation to a terminal state; the build should succeed.
    let record = poll_operation(&app, &op_id, Duration::from_secs(60)).await;
    assert_eq!(record["status"], "succeeded", "record: {record}");
    assert_eq!(record["kind"], "scalar_index");
    assert_eq!(record["namespace"], ns);
    assert!(
        record["finished_at_ms"].as_u64().is_some(),
        "a finished timestamp is set on completion: {record}"
    );
    assert!(record["error"].is_null(), "no error on success: {record}");
}

/// No MinIO needed: the registry is in-memory, so an unknown id is a
/// pure 404. Runs in CI to cover the endpoint wiring and the mapping.
#[tokio::test]
async fn unknown_operation_is_404() {
    let (state, _tmp) = test_state_offline().await;
    let app = router(state);

    let (status, body) = get(app, "/operations/op_does-not-exist".to_string()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not found"),
        "expected a not-found message, got {body}"
    );
}
