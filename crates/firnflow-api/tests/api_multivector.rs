//! API-level integration test for multivector namespaces.
//!
//! Drives the axum router via `tower::ServiceExt::oneshot` to
//! exercise the wire-shape validation that the handler adds on top
//! of the manager. Covers:
//!
//! 1. JSON `vectors: [[...], ...]` upsert + query round-trip through
//!    a multivector namespace.
//! 2. Shape mismatch — a multivector namespace receiving a
//!    `vector: [...]` payload (or vice versa) returns 400 with the
//!    expected shape spelled out.
//! 3. Empty inner list and mixed inner dim both fail at the
//!    handler/manager boundary before reaching Lance.
//! 4. `vectors` results omit the per-row bag — the response carries
//!    `vector: null` for multivector hits.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_multivector \
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

#[tokio::test]
#[ignore]
async fn upsert_and_query_round_trip_multivector() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("mv-smoke");

    // Multivector upsert — the JSON shape is `vectors: [[...], ...]`
    // with each inner array of equal length. The first row of the
    // first request establishes the namespace as multivector with
    // inner dim = 4.
    let upsert_body = json!({
        "rows": [
            {"id": 1, "vectors": [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]]},
            {"id": 2, "vectors": [[0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]]},
        ]
    });
    let (status, body) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK, "upsert body: {body}");
    assert_eq!(body["upserted"], 2);

    // Multivector query — same plural `vectors` shape on the wire.
    // Intentionally no `/index` build between upsert and query:
    // multivector queries work on an un-indexed namespace via
    // brute-force scan. The index would be needed for tractable
    // latency on a real corpus, not for correctness. The
    // manager-level `manager_multivector` test covers the indexed
    // path explicitly.
    let query_body = json!({
        "vectors": [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]],
        "k": 2
    });
    let (status, body) = post_json(app, format!("/ns/{ns}/query"), query_body).await;
    assert_eq!(status, StatusCode::OK, "query body: {body}");
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "expected both hits");
    assert_eq!(results[0]["id"], 1, "nearest hit must be id=1");

    // The response intentionally does not echo the per-row bag —
    // the bag can be hundreds of KB and is not useful to round-trip.
    // It is rendered as `null` (not `[]`) since the result vector
    // field became `Option<Vec<f32>>`.
    assert!(
        results[0]["vector"].is_null(),
        "multivector results must omit the bag; got {}",
        results[0]["vector"]
    );
}

#[tokio::test]
#[ignore]
async fn single_payload_on_multivector_namespace_returns_400() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("mv-mismatch");

    // Establish the namespace as multivector.
    let upsert_body = json!({
        "rows": [
            {"id": 1, "vectors": [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]]}
        ]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK);

    // A second upsert sending a single-vector payload must 400
    // with a clear message naming the expected shape.
    let bad_body = json!({
        "rows": [{"id": 2, "vector": [1.0, 0.0, 0.0, 0.0]}]
    });
    let (status, body) = post_json(app.clone(), format!("/ns/{ns}/upsert"), bad_body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "wrong-shape upsert must 400; body: {body}"
    );
    let err = body["error"].as_str().expect("error field");
    assert!(
        err.contains("multivector") && err.contains("vectors:"),
        "error must point at the multivector shape; got: {err}"
    );

    // Same story for queries.
    let bad_query = json!({
        "vector": [1.0, 0.0, 0.0, 0.0],
        "k": 1
    });
    let (status, body) = post_json(app, format!("/ns/{ns}/query"), bad_query).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "wrong-shape query body: {body}"
    );
}

#[tokio::test]
#[ignore]
async fn multi_payload_on_single_namespace_returns_400() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("single-mismatch");

    // Establish as single-vector.
    let upsert_body = json!({
        "rows": [{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0]}]
    });
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/upsert"), upsert_body).await;
    assert_eq!(status, StatusCode::OK);

    // Multivector payload must 400.
    let bad_body = json!({
        "rows": [{"id": 2, "vectors": [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]]}]
    });
    let (status, body) = post_json(app, format!("/ns/{ns}/upsert"), bad_body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "wrong-shape upsert must 400; body: {body}"
    );
    let err = body["error"].as_str().expect("error field");
    assert!(
        err.contains("single") && err.contains("vector:"),
        "error must point at the single-vector shape; got: {err}"
    );
}

#[tokio::test]
#[ignore]
async fn empty_inner_list_returns_400() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("mv-empty");

    let bad_body = json!({
        "rows": [{"id": 1, "vectors": [[]]}]
    });
    let (status, body) = post_json(app, format!("/ns/{ns}/upsert"), bad_body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "empty inner list body: {body}"
    );
    let err = body["error"].as_str().expect("error field");
    assert!(
        err.contains("empty"),
        "error must mention the empty sub-vector; got: {err}"
    );
}

#[tokio::test]
#[ignore]
async fn mixed_inner_dim_returns_400() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("mv-mixed");

    let bad_body = json!({
        "rows": [{"id": 1, "vectors": [[1.0, 0.0, 0.0, 0.0], [1.0, 0.0]]}]
    });
    let (status, body) = post_json(app, format!("/ns/{ns}/upsert"), bad_body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "mixed-dim body: {body}");
    let err = body["error"].as_str().expect("error field");
    assert!(
        err.contains("sub-vector"),
        "error must mention the sub-vector dimension mismatch; got: {err}"
    );
}

#[tokio::test]
#[ignore]
async fn both_vector_fields_set_returns_400() {
    let (app, _tmp) = build_app().await;
    let ns = unique_namespace("mv-both");

    let bad_body = json!({
        "rows": [{
            "id": 1,
            "vector": [1.0, 0.0, 0.0, 0.0],
            "vectors": [[1.0, 0.0, 0.0, 0.0]]
        }]
    });
    let (status, body) = post_json(app, format!("/ns/{ns}/upsert"), bad_body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "both-fields body: {body}");
    let err = body["error"].as_str().expect("error field");
    assert!(
        err.contains("exactly one"),
        "error must explain the mutual-exclusion rule; got: {err}"
    );
}
