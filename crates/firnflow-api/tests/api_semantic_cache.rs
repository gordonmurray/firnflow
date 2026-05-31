//! API-level integration test for the opt-in semantic cache.
//!
//! Covers:
//!
//! - `POST /ns/{ns}/query` accepts a `semantic_cache` JSON block and
//!   surfaces it through the service's eligibility validation.
//! - A near-duplicate opt-in query reuses the original top-k bytes;
//!   the `firnflow_semantic_cache_hits_total` counter ticks.
//! - An ineligible shape (semantic + text → hybrid) returns HTTP 400
//!   with the expected error string.
//! - Existing requests with no `semantic_cache` field continue to
//!   round-trip unchanged.
//!
//! Gated `#[ignore]`: needs MinIO up via `docker compose up -d minio
//! minio-init`.

use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, Request, StatusCode};
use firnflow_api::router;
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{test_state, unique_namespace};

const DIM: usize = 8;

async fn build_app() -> (axum::Router, firnflow_api::AppState, tempfile::TempDir) {
    let (state, tmp) = test_state().await;
    (router(state.clone()), state, tmp)
}

async fn post_json(app: axum::Router, uri: String, body: Value) -> (StatusCode, Value) {
    let (status, _, json) = post_json_debug(app, uri, body).await;
    (status, json)
}

async fn post_json_debug(
    app: axum::Router,
    uri: String,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-firn-debug-cache-source", "true")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, headers, json)
}

async fn get_metrics_body(app: axum::Router) -> String {
    let req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "/metrics must respond 200"
    );
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn unit_axis(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[axis] = 1.0;
    v
}

fn near_axis(axis: usize, drift: f32) -> Vec<f32> {
    let other = (axis + 1) % DIM;
    let main = (1.0_f32 - drift * drift).sqrt();
    let mut v = vec![0.0_f32; DIM];
    v[axis] = main;
    v[other] = drift;
    v
}

#[tokio::test]
#[ignore]
async fn api_semantic_cache_near_duplicate_hits_and_metric_ticks() {
    let (app, _state, _tmp) = build_app().await;
    let ns = unique_namespace("api-semantic");

    // Seed
    let rows: Vec<Value> = (0..DIM)
        .map(|i| json!({"id": i as u64, "vector": unit_axis(i)}))
        .collect();
    let (status, _) = post_json(
        app.clone(),
        format!("/ns/{ns}/upsert"),
        json!({"rows": rows}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Opt-in seed query
    let seed = json!({
        "vector": unit_axis(0),
        "k": 4,
        "semantic_cache": {"enabled": true, "min_similarity": 0.95},
    });
    let (status, headers, first) =
        post_json_debug(app.clone(), format!("/ns/{ns}/query"), seed.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("x-firn-cache-source")
            .and_then(|h| h.to_str().ok()),
        Some("backend")
    );
    assert_eq!(first["results"].as_array().unwrap().len(), 4);

    // Identical repeat should short-circuit through the exact cache,
    // and the debug header should report that source without changing
    // the JSON body.
    let (status, headers, repeat) =
        post_json_debug(app.clone(), format!("/ns/{ns}/query"), seed).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("x-firn-cache-source")
            .and_then(|h| h.to_str().ok()),
        Some("exact_cache")
    );
    assert_eq!(repeat["results"], first["results"]);

    // Near-duplicate opt-in query — should reuse the cached top-k
    // and tick the semantic-hit metric.
    let probe = json!({
        "vector": near_axis(0, 0.05),
        "k": 4,
        "semantic_cache": {"enabled": true, "min_similarity": 0.95},
    });
    let (status, headers, reuse) =
        post_json_debug(app.clone(), format!("/ns/{ns}/query"), probe).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("x-firn-cache-source")
            .and_then(|h| h.to_str().ok()),
        Some("semantic_cache")
    );
    assert_eq!(
        reuse["results"], first["results"],
        "semantic hit should reuse the seed query's top-k bytes",
    );

    let body = get_metrics_body(app).await;
    let needle = format!("firnflow_semantic_cache_hits_total{{namespace=\"{ns}\"}} 1");
    assert!(
        body.contains(&needle),
        "metrics body missing {needle:?}; body was:\n{body}",
    );
}

#[tokio::test]
#[ignore]
async fn api_semantic_cache_hybrid_returns_400() {
    let (app, _state, _tmp) = build_app().await;
    let ns = unique_namespace("api-semantic-hybrid");

    let body = json!({
        "vector": unit_axis(0),
        "k": 4,
        "text": "anything",
        "semantic_cache": {"enabled": true},
    });
    let (status, payload) = post_json(app, format!("/ns/{ns}/query"), body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = payload["error"].as_str().unwrap();
    assert!(
        err.contains("FTS") || err.contains("hybrid"),
        "unexpected error payload: {payload}",
    );
}

#[tokio::test]
#[ignore]
async fn api_semantic_cache_threshold_range_returns_400() {
    let (app, _state, _tmp) = build_app().await;
    let ns = unique_namespace("api-semantic-threshold");

    let body = json!({
        "vector": unit_axis(0),
        "k": 4,
        "semantic_cache": {"enabled": true, "min_similarity": 1.5},
    });
    let (status, payload) = post_json(app, format!("/ns/{ns}/query"), body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(payload["error"]
        .as_str()
        .unwrap()
        .contains("min_similarity"));
}

#[tokio::test]
#[ignore]
async fn api_query_without_semantic_field_round_trips() {
    let (app, _state, _tmp) = build_app().await;
    let ns = unique_namespace("api-semantic-default");

    let rows: Vec<Value> = (0..DIM)
        .map(|i| json!({"id": i as u64, "vector": unit_axis(i)}))
        .collect();
    let (status, _) = post_json(
        app.clone(),
        format!("/ns/{ns}/upsert"),
        json!({"rows": rows}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let body = json!({"vector": unit_axis(0), "k": 4});
    let (status, payload) = post_json(app, format!("/ns/{ns}/query"), body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["results"].as_array().unwrap().len(), 4);
}
