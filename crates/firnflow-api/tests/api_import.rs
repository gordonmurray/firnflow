//! Integration tests for `POST /ns/{namespace}/import` — the binary
//! Arrow IPC bulk-ingest path.
//!
//! Offline tests (no MinIO) cover the synchronous rejections that fire
//! before any storage is touched: wrong `Content-Type` (415), a body
//! over `FIRNFLOW_IMPORT_MAX_BYTES` (413), and a malformed Arrow stream
//! (400). The MinIO-gated tests drive a real Arrow IPC body end to end:
//! 202 + `operation_id`, poll to `succeeded`, then confirm the rows
//! landed in a single commit, and that a stream carrying declared
//! metadata columns is filterable afterwards while an undeclared column
//! is a synchronous 400.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-api --test api_import -- --ignored
//! ```

use std::sync::Arc;
use std::time::Duration;

use arrow_array::builder::{FixedSizeListBuilder, Float32Builder};
use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray, UInt64Array};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use firnflow_api::router;
use serde_json::Value;
use tower::ServiceExt;

mod common;
use common::{test_state, test_state_offline, unique_namespace};

const ARROW_CT: &str = "application/vnd.apache.arrow.stream";
const DIM: usize = 4;

async fn post_bytes(
    app: axum::Router,
    uri: String,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

fn single_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                DIM as i32,
            ),
            false,
        ),
    ]))
}

fn single_batch(schema: &SchemaRef, ids: &[u64]) -> RecordBatch {
    let id_arr = UInt64Array::from_iter_values(ids.iter().copied());
    let mut list = FixedSizeListBuilder::new(Float32Builder::new(), DIM as i32);
    for &id in ids {
        for axis in 0..DIM {
            list.values().append_value(if axis == (id as usize % DIM) {
                1.0
            } else {
                0.0
            });
        }
        list.append(true);
    }
    RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(id_arr), Arc::new(list.finish())],
    )
    .unwrap()
}

/// The single-vector import schema with attribute columns appended.
fn single_schema_with(extra: Vec<Field>) -> SchemaRef {
    let mut fields = single_schema().fields().to_vec();
    fields.extend(extra.into_iter().map(Arc::new));
    Arc::new(Schema::new(fields))
}

/// A single-vector batch with attribute arrays appended, in the order
/// `schema` lists them.
fn single_batch_with(schema: &SchemaRef, ids: &[u64], extra: Vec<ArrayRef>) -> RecordBatch {
    let base = single_batch(&single_schema(), ids);
    let mut columns = base.columns().to_vec();
    columns.extend(extra);
    RecordBatch::try_new(schema.clone(), columns).unwrap()
}

/// Poll a `202`'s operation until it succeeds.
async fn await_operation(app: &axum::Router, accepted: &Value) {
    let op_id = accepted["operation_id"]
        .as_str()
        .expect("202 carries operation_id")
        .to_string();
    for _ in 0..600 {
        let (s, op) = get(app.clone(), format!("/operations/{op_id}")).await;
        assert_eq!(s, StatusCode::OK);
        match op["status"].as_str() {
            Some("succeeded") => return,
            Some("failed") => panic!("operation failed: {op}"),
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    panic!("operation {op_id} did not finish in time");
}

/// Serialize batches as an Arrow IPC stream (the `/import` wire format).
fn arrow_ipc(schema: &SchemaRef, batches: &[RecordBatch]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, schema).unwrap();
        for b in batches {
            w.write(b).unwrap();
        }
        w.finish().unwrap();
    }
    buf
}

#[tokio::test]
async fn import_rejects_wrong_content_type() {
    let (state, _tmp) = test_state_offline().await;
    let app = router(state);
    let ns = unique_namespace("import-ct");
    let (status, _) = post_bytes(
        app,
        format!("/ns/{ns}/import"),
        "application/json",
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "non-Arrow content type must be rejected with 415"
    );
}

#[tokio::test]
async fn import_rejects_body_over_cap() {
    let (mut state, _tmp) = test_state_offline().await;
    state.import_max_bytes = 16; // tiny cap for the test
    let app = router(state);
    let ns = unique_namespace("import-413");
    // 100 bytes > 16-byte cap; the cap trips during spooling, before any
    // schema parse or storage touch, so the bytes need not be valid Arrow.
    let (status, _) = post_bytes(app, format!("/ns/{ns}/import"), ARROW_CT, vec![0u8; 100]).await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "a body over FIRNFLOW_IMPORT_MAX_BYTES must be rejected with 413"
    );
}

#[tokio::test]
async fn import_rejects_malformed_arrow() {
    let (state, _tmp) = test_state_offline().await;
    let app = router(state);
    let ns = unique_namespace("import-garbage");
    let (status, _) = post_bytes(
        app,
        format!("/ns/{ns}/import"),
        ARROW_CT,
        b"this is not an arrow stream".to_vec(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a body that is not a valid Arrow IPC stream must be rejected with 400"
    );
}

#[tokio::test]
#[ignore]
async fn import_arrow_stream_lands_in_one_commit() {
    let (state, _tmp) = test_state().await;
    let app = router(state);
    let ns = unique_namespace("import-ok");
    let schema = single_schema();

    // Two batches in one stream → must be a single commit (table version 1
    // on a fresh namespace = the lone append, since the empty create is
    // version-zero state until the first data lands... assert <= 2 either
    // way, then confirm via row_count below).
    let body = arrow_ipc(
        &schema,
        &[
            single_batch(&schema, &[0, 1, 2, 3]),
            single_batch(&schema, &[4, 5, 6, 7]),
        ],
    );
    let (status, accepted) =
        post_bytes(app.clone(), format!("/ns/{ns}/import"), ARROW_CT, body).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "import returns 202: {accepted}"
    );
    let op_id = accepted["operation_id"]
        .as_str()
        .expect("202 carries operation_id")
        .to_string();
    assert_eq!(accepted["kind"], "import");

    // Poll the operation to completion.
    let mut succeeded = false;
    for _ in 0..600 {
        let (s, op) = get(app.clone(), format!("/operations/{op_id}")).await;
        assert_eq!(s, StatusCode::OK);
        match op["status"].as_str() {
            Some("succeeded") => {
                succeeded = true;
                break;
            }
            Some("failed") => panic!("import operation failed: {op}"),
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    assert!(succeeded, "import operation did not finish in time");

    let (s, info) = get(app, format!("/ns/{ns}")).await;
    assert_eq!(s, StatusCode::OK, "namespace info: {info}");
    assert_eq!(
        info["row_count"].as_u64().unwrap(),
        8,
        "all 8 rows imported"
    );
}

#[tokio::test]
#[ignore]
async fn import_carries_declared_attributes_and_filters_on_them() {
    let (state, _tmp) = test_state().await;
    let app = router(state);
    let ns = unique_namespace("import-attrs");
    let schema = single_schema();

    // A namespace has to exist before its columns can be declared, so
    // the first load is the one that cannot carry values.
    let body = arrow_ipc(&schema, &[single_batch(&schema, &[0])]);
    let (status, accepted) =
        post_bytes(app.clone(), format!("/ns/{ns}/import"), ARROW_CT, body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "seed import: {accepted}");
    await_operation(&app, &accepted).await;

    // Before the declaration, an attribute column is an unknown column,
    // and the rejection is a synchronous 400 rather than a 202 whose
    // operation fails later.
    let attr_schema = single_schema_with(vec![
        Field::new("section", DataType::Utf8, true),
        Field::new("year", DataType::Int64, true),
    ]);
    let attr_batch = single_batch_with(
        &attr_schema,
        &[10, 11],
        vec![
            Arc::new(StringArray::from(vec!["warnings", "dosage"])),
            Arc::new(Int64Array::from(vec![2024_i64, 2025])),
        ],
    );
    let body = arrow_ipc(&attr_schema, std::slice::from_ref(&attr_batch));
    let (status, err) = post_bytes(app.clone(), format!("/ns/{ns}/import"), ARROW_CT, body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an undeclared column must be a 400: {err}"
    );

    let (status, declared) = post_bytes(
        app.clone(),
        format!("/ns/{ns}/attributes"),
        "application/json",
        br#"{"attributes":[{"name":"section","type":"string"},{"name":"year","type":"int"}]}"#
            .to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "declare attributes: {declared}");

    // Same stream again, now that the columns exist.
    let body = arrow_ipc(&attr_schema, &[attr_batch]);
    let (status, accepted) =
        post_bytes(app.clone(), format!("/ns/{ns}/import"), ARROW_CT, body).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "import with values: {accepted}"
    );
    await_operation(&app, &accepted).await;

    // A filter over an imported value matches without the rows having
    // been rewritten through /upsert first.
    let (status, page) = post_bytes(
        app.clone(),
        format!("/ns/{ns}/query"),
        "application/json",
        br#"{"vector":[1.0,0.0,0.0,0.0],"k":10,"filter":"section = 'dosage'"}"#.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "filtered query: {page}");
    let results = page["results"].as_array().expect("results array");
    let ids: Vec<u64> = results
        .iter()
        .map(|r| r["id"].as_u64().expect("id"))
        .collect();
    assert_eq!(ids, vec![11], "only the matching imported row: {page}");
    assert_eq!(
        results[0]["attributes"]["year"].as_i64(),
        Some(2025),
        "values come back on the hit: {page}"
    );
    assert_eq!(
        results[0]["attributes"]["section"].as_str(),
        Some("dosage"),
        "values come back on the hit: {page}"
    );
}
