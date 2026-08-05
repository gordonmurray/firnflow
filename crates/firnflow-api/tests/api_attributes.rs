//! Integration test for `POST /ns/{namespace}/attributes` and the
//! attribute values that ride on `/upsert`, `/query`, and `/list`.
//!
//! Runs against local filesystem storage, so no MinIO is needed.
//! Drives the axum router through `tower::ServiceExt::oneshot`.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use firnflow_api::router;
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;
use common::{test_state_local, unique_namespace};

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

fn declaration() -> Value {
    json!({
        "attributes": [
            {"name": "section", "type": "string"},
            {"name": "year", "type": "int"},
            {"name": "score", "type": "float"},
            {"name": "archived", "type": "bool"}
        ]
    })
}

/// Create the namespace, declare the columns, then write rows with
/// values. Returns the router and the namespace name.
async fn seeded() -> (axum::Router, String, tempfile::TempDir) {
    let (state, tmp) = test_state_local().await;
    let app = router(state);
    let ns = unique_namespace("attrs");

    let (status, _) = post_json(
        app.clone(),
        format!("/ns/{ns}/upsert"),
        json!({"rows": [{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0]}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) =
        post_json(app.clone(), format!("/ns/{ns}/attributes"), declaration()).await;
    assert_eq!(status, StatusCode::OK, "declare: {body}");

    let (status, _) = post_json(
        app.clone(),
        format!("/ns/{ns}/upsert"),
        json!({"rows": [
            {
                "id": 1,
                "vector": [1.0, 0.0, 0.0, 0.0],
                "text": "fox warning",
                "attributes": {
                    "section": "warnings",
                    "year": 2024,
                    "score": 0.5,
                    "archived": false
                }
            },
            {
                "id": 2,
                "vector": [0.0, 1.0, 0.0, 0.0],
                "text": "fox dosing",
                "attributes": {"section": "dosage", "year": 2025}
            },
            {
                "id": 3,
                "vector": [0.0, 0.0, 1.0, 0.0],
                "text": "dog warning",
                "attributes": {"section": "warnings"}
            }
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    (app, ns, tmp)
}

#[tokio::test]
async fn declaring_returns_the_full_set_and_shows_up_on_namespace_info() {
    let (app, ns, _tmp) = seeded().await;

    let (status, body) = get(app.clone(), format!("/ns/{ns}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["attributes"], declaration()["attributes"]);

    // A second declaration returns everything, not just what it added.
    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/attributes"),
        json!({"attributes": [{"name": "route", "type": "string"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["section", "year", "score", "archived", "route"]);
}

#[tokio::test]
async fn declaring_on_a_namespace_that_does_not_exist_is_a_400() {
    let (state, _tmp) = test_state_local().await;
    let app = router(state);
    let ns = unique_namespace("attrs-missing");

    let (status, body) = post_json(app, format!("/ns/{ns}/attributes"), declaration()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("no data has been upserted yet")),
        "{body}"
    );
}

#[tokio::test]
async fn a_bad_declaration_is_a_400() {
    let (app, ns, _tmp) = seeded().await;

    for attributes in [
        json!([{"name": "Section", "type": "string"}]), // uppercase
        json!([{"name": "_hidden", "type": "string"}]), // system namespace
        json!([{"name": "id", "type": "int"}]),         // engine-owned
        json!([{"name": "year", "type": "string"}]),    // type change
        json!([]),                                      // nothing to declare
    ] {
        let (status, body) = post_json(
            app.clone(),
            format!("/ns/{ns}/attributes"),
            json!({ "attributes": attributes }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "declaration {attributes} should be rejected: {body}"
        );
    }
}

#[tokio::test]
async fn values_render_as_bare_json_scalars_on_a_query() {
    let (app, ns, _tmp) = seeded().await;

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/query"),
        json!({"vector": [1.0, 0.0, 0.0, 0.0], "k": 3, "include_vector": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let hits = body["results"].as_array().unwrap();
    let first = hits.iter().find(|h| h["id"] == 1).expect("row 1");
    assert_eq!(
        first["attributes"],
        json!({"section": "warnings", "year": 2024, "score": 0.5, "archived": false})
    );

    // A row that set one column carries only that column.
    let sparse = hits.iter().find(|h| h["id"] == 3).expect("row 3");
    assert_eq!(sparse["attributes"], json!({"section": "warnings"}));
}

/// A namespace with no declared attributes keeps exactly the response
/// shape it had before attribute columns existed: no empty object
/// appears on every hit.
#[tokio::test]
async fn a_namespace_without_attributes_gets_no_attributes_field() {
    let (state, tmp) = test_state_local().await;
    let app = router(state);
    let ns = unique_namespace("attrs-none");
    let _tmp = tmp;

    let (status, _) = post_json(
        app.clone(),
        format!("/ns/{ns}/upsert"),
        json!({"rows": [{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0]}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/query"),
        json!({"vector": [1.0, 0.0, 0.0, 0.0], "k": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["results"][0].get("attributes").is_none(),
        "an attribute-free namespace should not grow a field: {body}"
    );
}

#[tokio::test]
async fn a_filter_over_an_attribute_narrows_the_hits() {
    let (app, ns, _tmp) = seeded().await;

    let (status, body) = post_json(
        app.clone(),
        format!("/ns/{ns}/query"),
        json!({
            "vector": [1.0, 0.0, 0.0, 0.0],
            "k": 10,
            "include_vector": false,
            "filter": "section = 'warnings' AND year IS NULL"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<u64> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![3]);

    // The same predicate against a full-text query.
    let (status, _) = post_json(app.clone(), format!("/ns/{ns}/fts-index"), json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // The index build is asynchronous; poll until the text query stops
    // reporting the missing index.
    for _ in 0..50 {
        let (status, body) = post_json(
            app.clone(),
            format!("/ns/{ns}/query"),
            json!({"text": "fox", "k": 10, "filter": "section = 'dosage'"}),
        )
        .await;
        if status == StatusCode::OK {
            let ids: Vec<u64> = body["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["id"].as_u64().unwrap())
                .collect();
            assert_eq!(ids, vec![2], "filtered full-text search: {body}");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the full-text index never became available");
}

#[tokio::test]
async fn an_undeclared_attribute_on_upsert_is_a_400() {
    let (app, ns, _tmp) = seeded().await;

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/upsert"),
        json!({"rows": [{
            "id": 9,
            "vector": [0.0, 0.0, 0.0, 1.0],
            "attributes": {"route": "ORAL"}
        }]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("not declared")),
        "{body}"
    );
}

/// A null clears a value rather than setting one, but the name it
/// clears still has to exist. Otherwise a caller who misspells a
/// column while clearing it gets a 200 for a write that did nothing.
#[tokio::test]
async fn an_undeclared_name_with_a_null_value_is_a_400() {
    let (app, ns, _tmp) = seeded().await;

    let (status, body) = post_json(
        app.clone(),
        format!("/ns/{ns}/upsert"),
        json!({"rows": [{
            "id": 9,
            "vector": [0.0, 0.0, 0.0, 1.0],
            "attributes": {"rotue": null}
        }]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A declared name with a null value clears it.
    let (status, _) = post_json(
        app.clone(),
        format!("/ns/{ns}/upsert"),
        json!({"rows": [{
            "id": 1,
            "vector": [1.0, 0.0, 0.0, 0.0],
            "attributes": {"section": null, "year": 2024}
        }]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/query"),
        json!({"vector": [1.0, 0.0, 0.0, 0.0], "k": 1, "include_vector": false,
               "filter": "id = 1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"][0]["attributes"], json!({"year": 2024}));
}

#[tokio::test]
async fn a_composite_attribute_value_is_a_400() {
    let (app, ns, _tmp) = seeded().await;

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/upsert"),
        json!({"rows": [{
            "id": 9,
            "vector": [0.0, 0.0, 0.0, 1.0],
            "attributes": {"section": ["warnings", "dosage"]}
        }]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("string, number, or boolean")),
        "{body}"
    );
}

#[tokio::test]
async fn list_rows_carry_attributes() {
    let (app, ns, _tmp) = seeded().await;

    let (status, body) = get(app, format!("/ns/{ns}/list?limit=10&order=asc")).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["rows"].as_array().unwrap();
    let row = rows.iter().find(|r| r["id"] == 2).expect("row 2");
    assert_eq!(
        row["attributes"],
        json!({"section": "dosage", "year": 2025})
    );
}

#[tokio::test]
async fn a_scalar_index_can_target_a_declared_attribute() {
    let (app, ns, _tmp) = seeded().await;

    let (status, body) = post_json(
        app.clone(),
        format!("/ns/{ns}/scalar-index"),
        json!({"column": "section"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let (status, body) = post_json(
        app,
        format!("/ns/{ns}/scalar-index"),
        json!({"column": "route"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an undeclared column is not an index target: {body}"
    );
}
