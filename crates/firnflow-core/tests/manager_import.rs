//! Integration tests for the binary bulk-import path
//! (`NamespaceManager::import_arrow`, behind `POST /ns/{ns}/import`).
//!
//! These prove the behaviour the JSON `/upsert` path can't give a
//! large first load:
//!
//! 1. An Arrow stream appends insert-only and the rows are queryable,
//!    for both single-vector and multivector namespaces.
//! 2. A multi-batch stream lands in **one** Lance commit (the table
//!    version advances by exactly 1), i.e. no per-batch commit
//!    amplification — the whole point of the feature.
//! 3. Row-level validation rejects null ids, null float values inside a
//!    vector or sub-vector, and empty multivector rows as
//!    `InvalidRequest`.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test manager_import \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::builder::{FixedSizeListBuilder, Float32Builder, ListBuilder};
use arrow_array::{RecordBatch, RecordBatchIterator, RecordBatchReader, UInt64Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use firnflow_core::metrics::test_metrics;
use firnflow_core::{FirnflowError, NamespaceId, NamespaceManager, StorageRoot};

const DIM: usize = 8;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn unique_namespace(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{nanos}")
}

fn minio_options() -> HashMap<String, String> {
    HashMap::from([
        (
            "aws_access_key_id".into(),
            env_or("FIRNFLOW_S3_ACCESS_KEY", "minioadmin"),
        ),
        (
            "aws_secret_access_key".into(),
            env_or("FIRNFLOW_S3_SECRET_KEY", "minioadmin"),
        ),
        (
            "aws_endpoint".into(),
            env_or("FIRNFLOW_S3_ENDPOINT", "http://127.0.0.1:9000"),
        ),
        ("aws_region".into(), "us-east-1".into()),
        ("allow_http".into(), "true".into()),
        ("aws_virtual_hosted_style_request".into(), "false".into()),
    ])
}

fn manager() -> NamespaceManager {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        minio_options(),
        test_metrics(),
    )
}

fn float32_item() -> Arc<Field> {
    Arc::new(Field::new("item", DataType::Float32, true))
}

fn single_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(float32_item(), DIM as i32),
            false,
        ),
    ]))
}

fn multi_schema() -> SchemaRef {
    let inner = DataType::FixedSizeList(float32_item(), DIM as i32);
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, false),
        Field::new(
            "vectors",
            DataType::List(Arc::new(Field::new("item", inner, true))),
            false,
        ),
    ]))
}

/// A single-vector batch: each id gets a unit vector on axis `id % DIM`.
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

/// A multivector batch: each id gets `subs` sub-vectors.
fn multi_batch(schema: &SchemaRef, ids: &[u64], subs: usize) -> RecordBatch {
    let id_arr = UInt64Array::from_iter_values(ids.iter().copied());
    let inner = FixedSizeListBuilder::new(Float32Builder::new(), DIM as i32);
    let mut outer = ListBuilder::new(inner);
    for _ in ids {
        for s in 0..subs {
            for axis in 0..DIM {
                outer
                    .values()
                    .values()
                    .append_value(if axis == s % DIM { 1.0 } else { 0.0 });
            }
            outer.values().append(true);
        }
        outer.append(true);
    }
    RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(id_arr), Arc::new(outer.finish())],
    )
    .unwrap()
}

fn reader(schema: SchemaRef, batches: Vec<RecordBatch>) -> Box<dyn RecordBatchReader + Send> {
    Box::new(RecordBatchIterator::new(
        batches.into_iter().map(Ok),
        schema,
    ))
}

#[tokio::test]
#[ignore]
async fn import_single_vector_is_queryable() {
    let mgr = manager();
    let ns = NamespaceId::new(unique_namespace("import-single")).unwrap();
    let schema = single_schema();

    let batches = vec![
        single_batch(&schema, &[0, 1, 2, 3]),
        single_batch(&schema, &[4, 5, 6, 7]),
    ];
    let imported = mgr
        .import_arrow(&ns, reader(schema, batches))
        .await
        .expect("import");
    assert_eq!(imported, 8, "all rows across both batches appended");

    let info = mgr.info(&ns).await.unwrap().expect("namespace exists");
    assert_eq!(info.row_count, 8);

    // _ingested_at is server-set on import, so a query carries it back.
    let mut q = vec![0.0_f32; DIM];
    q[0] = 1.0;
    let results = mgr
        .query(&ns, q, None, 10, None, None, None, true)
        .await
        .expect("query")
        .results;
    assert!(!results.is_empty(), "imported rows are searchable");
    assert!(
        results.iter().all(|r| r.ingested_at_micros.is_some()),
        "import stamps _ingested_at"
    );
}

#[tokio::test]
#[ignore]
async fn import_multibatch_is_a_single_commit() {
    let mgr = manager();
    let ns = NamespaceId::new(unique_namespace("import-onecommit")).unwrap();
    let schema = single_schema();

    // Seed the namespace (creates the table + one append commit).
    mgr.import_arrow(
        &ns,
        reader(schema.clone(), vec![single_batch(&schema, &[0, 1])]),
    )
    .await
    .expect("seed import");
    let before = mgr.info(&ns).await.unwrap().unwrap();

    // A second import carrying FOUR batches must advance the table
    // version by exactly 1 — proving the whole stream is one commit, not
    // one-commit-per-batch (the commit-amplification the feature fixes).
    let four = vec![
        single_batch(&schema, &[2, 3]),
        single_batch(&schema, &[4, 5]),
        single_batch(&schema, &[6, 7]),
        single_batch(&schema, &[8, 9]),
    ];
    let imported = mgr
        .import_arrow(&ns, reader(schema, four))
        .await
        .expect("multi-batch import");
    assert_eq!(imported, 8);

    let after = mgr.info(&ns).await.unwrap().unwrap();
    assert_eq!(
        after.table_version,
        before.table_version + 1,
        "a 4-batch import must be exactly one commit (version +1), got {} -> {}",
        before.table_version,
        after.table_version
    );
    assert_eq!(after.row_count, 10, "2 seeded + 8 imported");
}

#[tokio::test]
#[ignore]
async fn import_multivector_is_queryable() {
    let mgr = manager();
    let ns = NamespaceId::new(unique_namespace("import-multi")).unwrap();
    let schema = multi_schema();

    let imported = mgr
        .import_arrow(
            &ns,
            reader(schema.clone(), vec![multi_batch(&schema, &[0, 1, 2], 3)]),
        )
        .await
        .expect("multivector import");
    assert_eq!(imported, 3);

    let info = mgr.info(&ns).await.unwrap().unwrap();
    assert_eq!(info.row_count, 3);
}

#[tokio::test]
#[ignore]
async fn import_rejects_null_id() {
    let mgr = manager();
    let ns = NamespaceId::new(unique_namespace("import-nullid")).unwrap();

    // id column is nullable here so the batch builds; the row-level
    // check inside the import must still reject the null.
    let schema: SchemaRef = Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, true),
        Field::new(
            "vector",
            DataType::FixedSizeList(float32_item(), DIM as i32),
            false,
        ),
    ]));
    let id_arr = UInt64Array::from(vec![Some(1u64), None]);
    let mut list = FixedSizeListBuilder::new(Float32Builder::new(), DIM as i32);
    for _ in 0..2 {
        for _ in 0..DIM {
            list.values().append_value(0.0);
        }
        list.append(true);
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(id_arr), Arc::new(list.finish())],
    )
    .unwrap();

    let err = mgr
        .import_arrow(&ns, reader(schema, vec![batch]))
        .await
        .expect_err("null id must be rejected");
    assert!(
        matches!(err, FirnflowError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

#[tokio::test]
#[ignore]
async fn import_rejects_empty_multivector_row() {
    let mgr = manager();
    let ns = NamespaceId::new(unique_namespace("import-emptymv")).unwrap();
    let schema = multi_schema();

    // Two ids; the second row has zero sub-vectors (empty list).
    let id_arr = UInt64Array::from_iter_values([1u64, 2]);
    let inner = FixedSizeListBuilder::new(Float32Builder::new(), DIM as i32);
    let mut outer = ListBuilder::new(inner);
    // row 1: one sub-vector.
    for _ in 0..DIM {
        outer.values().values().append_value(0.0);
    }
    outer.values().append(true);
    outer.append(true);
    // row 2: empty.
    outer.append(true);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(id_arr), Arc::new(outer.finish())],
    )
    .unwrap();

    let err = mgr
        .import_arrow(&ns, reader(schema, vec![batch]))
        .await
        .expect_err("empty multivector row must be rejected");
    assert!(
        matches!(err, FirnflowError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

#[tokio::test]
#[ignore]
async fn import_rejects_null_float_in_single_vector() {
    let mgr = manager();
    let ns = NamespaceId::new(unique_namespace("import-nullfloat")).unwrap();
    let schema = single_schema();

    // Two non-null rows; the second carries a null float child value,
    // which a `FixedSizeList<Float32>` allows but the JSON path can't make.
    let id_arr = UInt64Array::from_iter_values([1u64, 2]);
    let mut list = FixedSizeListBuilder::new(Float32Builder::new(), DIM as i32);
    for _ in 0..DIM {
        list.values().append_value(0.0);
    }
    list.append(true);
    list.values().append_null();
    for _ in 1..DIM {
        list.values().append_value(0.0);
    }
    list.append(true);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(id_arr), Arc::new(list.finish())],
    )
    .unwrap();

    let err = mgr
        .import_arrow(&ns, reader(schema, vec![batch]))
        .await
        .expect_err("null float in a vector must be rejected");
    assert!(
        matches!(err, FirnflowError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

#[tokio::test]
#[ignore]
async fn import_rejects_null_float_in_multivector() {
    let mgr = manager();
    let ns = NamespaceId::new(unique_namespace("import-mvnullfloat")).unwrap();
    let schema = multi_schema();

    // One row, one sub-vector whose first float is null.
    let id_arr = UInt64Array::from_iter_values([1u64]);
    let inner = FixedSizeListBuilder::new(Float32Builder::new(), DIM as i32);
    let mut outer = ListBuilder::new(inner);
    outer.values().values().append_null();
    for _ in 1..DIM {
        outer.values().values().append_value(0.0);
    }
    outer.values().append(true);
    outer.append(true);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(id_arr), Arc::new(outer.finish())],
    )
    .unwrap();

    let err = mgr
        .import_arrow(&ns, reader(schema, vec![batch]))
        .await
        .expect_err("null float in a sub-vector must be rejected");
    assert!(
        matches!(err, FirnflowError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}
