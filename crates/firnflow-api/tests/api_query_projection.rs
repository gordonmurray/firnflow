//! `/query` response-shape integration test: the `include_vector`
//! request flag and the `ingested_at_micros` result field, as seen
//! on the wire.
//!
//! Covers:
//!
//! 1. Default queries return `vector` as an array and carry
//!    `ingested_at_micros` matching what `/list` reports for the
//!    same row.
//! 2. `include_vector: false` renders `vector` as `null` while id,
//!    score, and text are unchanged.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_query_projection \
//!     -- --ignored --nocapture
//! ```

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use firnflow_api::router;
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{test_state, unique_namespace};

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

async fn get_json(app: axum::Router, uri: String) -> (StatusCode, Value) {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

#[tokio::test]
#[ignore]
async fn query_response_shape_follows_include_vector() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("projection-test");

    let upsert_body = json!({
        "rows": [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "first row"},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0], "text": "second row"},
        ]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK, "upsert must succeed");

    // Default shape: vector present, timestamp populated.
    let (status, body) = post_json(
        app.clone(),
        format!("/ns/{ns}/query"),
        json!({"vector": [1.0, 0.0, 0.0, 0.0], "k": 2}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], 1);
    assert_eq!(
        results[0]["vector"].as_array().map(|v| v.len()),
        Some(4),
        "default response must carry the stored vector"
    );
    let queried_ts = results[0]["ingested_at_micros"]
        .as_i64()
        .expect("ingested_at_micros must be a number on query hits");
    assert!(queried_ts > 0, "timestamp must be populated");

    // Parity with /list: same row, same timestamp.
    let (status, list_body) = get_json(app.clone(), format!("/ns/{ns}/list?limit=10")).await;
    assert_eq!(status, StatusCode::OK, "list must succeed");
    let listed_ts = list_body["rows"]
        .as_array()
        .expect("rows array")
        .iter()
        .find(|r| r["id"] == 1)
        .expect("row 1 in list")["ingested_at_micros"]
        .as_i64()
        .expect("list timestamp");
    assert_eq!(
        queried_ts, listed_ts,
        "/query and /list must report the same _ingested_at for a row"
    );

    // Vector-light shape: vector is null, the rest is unchanged.
    let (status, body) = post_json(
        app.clone(),
        format!("/ns/{ns}/query"),
        json!({"vector": [1.0, 0.0, 0.0, 0.0], "k": 2, "include_vector": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], 1);
    assert!(
        results[0]["vector"].is_null(),
        "include_vector=false must render vector as null, got {}",
        results[0]["vector"]
    );
    assert_eq!(results[0]["text"], "first row");
    assert!(
        results[0]["ingested_at_micros"].as_i64().is_some(),
        "timestamp survives the vector-light shape"
    );
}
