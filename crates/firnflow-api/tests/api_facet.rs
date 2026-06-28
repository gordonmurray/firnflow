//! `/facet` integration tests over the HTTP router.
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
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "attributes": {"section": "warnings", "route": "oral"}},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0], "attributes": {"section": "dosage", "route": "oral"}},
            {"id": 3, "vector": [0.0, 0.0, 1.0, 0.0], "attributes": {"section": "warnings"}}
        ]
    });
    let (status, body) = post_json(app, format!("/ns/{ns}/upsert"), body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
#[ignore]
async fn facet_counts_over_the_wire() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("facet");
    seed(app.clone(), &ns).await;

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/facet"),
        json!({"fields": ["section"], "filter": "id >= 1", "top": 10}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["facets"][0]["field"], "section");
    assert_eq!(body["facets"][0]["buckets"][0]["value"], "warnings");
    assert_eq!(body["facets"][0]["buckets"][0]["count"], 2);
}

#[tokio::test]
#[ignore]
async fn facet_unknown_field_returns_400() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("facet-unknown");
    seed(app.clone(), &ns).await;

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/facet"),
        json!({"fields": ["missing"]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("missing"), "{body}");
}

#[tokio::test]
#[ignore]
async fn facet_malformed_filter_returns_400() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("facet-bad-filter");
    seed(app.clone(), &ns).await;

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/facet"),
        json!({"fields": ["section"], "filter": "section ="}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("filter"), "{body}");
}
