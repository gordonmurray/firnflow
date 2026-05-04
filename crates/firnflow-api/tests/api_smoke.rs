//! Slice-1c integration test: drive the axum router end-to-end via
//! `tower::ServiceExt::oneshot`, no real TCP listener.
//!
//! Covers:
//!
//! 1. `GET /health` → 200 `ok`
//! 2. `POST /ns/{ns}/upsert` accepts a JSON batch and reports the
//!    count
//! 3. `POST /ns/{ns}/query` returns the nearest neighbour of the
//!    upserted vector with ~zero distance
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_smoke -- --ignored --nocapture
//! ```

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use firnflow_api::router;
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{test_state, unique_namespace};

const DIM: usize = 8;

async fn build_app() -> (axum::Router, tempfile::TempDir) {
    let (state, tmp) = test_state().await;
    (router(state), tmp)
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
async fn health_returns_ok() {
    let (app, _tmp) = build_app().await;
    let request = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(&bytes[..], b"ok");
}

#[tokio::test]
#[ignore]
async fn upsert_and_query_round_trip() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("smoke-test");

    // upsert
    let upsert_body = json!({
        "rows": [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
            {"id": 3, "vector": [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
            {"id": 4, "vector": [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]},
        ]
    });
    let (status, body) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["upserted"], 4);

    // query
    let query_body = json!({
        "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "k": 3
    });
    let (status, body) = post_json(app, format!("/ns/{ns}/query"), query_body).await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3, "expected top-3");
    assert_eq!(results[0]["id"], 1, "nearest neighbour must be id=1");
    let score = results[0]["score"].as_f64().expect("score is a float");
    assert!(score < 0.01, "self-distance should be ~0, got {score}");
}

#[tokio::test]
#[ignore]
async fn invalid_namespace_returns_400() {
    let (app, _tmp) = build_app().await;

    // `NOT_LOWERCASE` fails NamespaceId validation — manager/service
    // never get invoked, the error short-circuits in the handler.
    let zero_vector: Vec<f32> = vec![0.0; DIM];
    let body = json!({ "vector": zero_vector, "k": 1 });
    let (status, payload) = post_json(app, "/ns/NOT_LOWERCASE/query".to_string(), body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        payload["error"]
            .as_str()
            .unwrap()
            .contains("invalid namespace"),
        "unexpected error payload: {payload}"
    );
}
