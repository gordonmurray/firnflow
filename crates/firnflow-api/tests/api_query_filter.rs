//! `/query` filter integration tests over the HTTP router.
//!
//! Gated `#[ignore]`: needs MinIO up.

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

async fn seed(app: axum::Router, ns: &str) {
    let body = json!({
        "rows": [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "fox warning"},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0], "text": "fox dosing"},
            {"id": 3, "vector": [0.0, 0.0, 1.0, 0.0], "text": "dog warning"}
        ]
    });
    let (status, _) = post_json(app, format!("/ns/{ns}/upsert"), body).await;
    assert_eq!(status, StatusCode::OK, "upsert must succeed");
}

#[tokio::test]
#[ignore]
async fn query_filter_narrows_results_over_the_wire() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("query-filter");
    seed(app.clone(), &ns).await;

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/query"),
        json!({
            "vector": [1.0, 0.0, 0.0, 0.0],
            "k": 3,
            "filter": "id > 1",
            "include_vector": false
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<u64> = body["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|r| r["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![2, 3]);
    assert!(body["results"][0]["vector"].is_null());
}

#[tokio::test]
#[ignore]
async fn query_filter_malformed_predicate_returns_400() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("query-filter-bad");
    seed(app.clone(), &ns).await;

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/query"),
        json!({"vector": [1.0, 0.0, 0.0, 0.0], "k": 3, "filter": "id ="}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("filter"), "{body}");
}

#[tokio::test]
#[ignore]
async fn query_filter_with_semantic_cache_returns_400() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("query-filter-semantic");
    seed(app.clone(), &ns).await;

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/query"),
        json!({
            "vector": [1.0, 0.0, 0.0, 0.0],
            "k": 3,
            "filter": "id > 1",
            "semantic_cache": {"enabled": true}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("filter"), "{body}");
}
