//! Slice-1a integration test: `NamespaceManager` round-trip.
//!
//! Upserts a handful of orthogonal unit vectors into a fresh
//! namespace, queries with one of them verbatim, and asserts that
//! the nearest neighbour is the row we stored with a distance at
//! or near zero. Proves the schema-plumbing, Arrow batch build,
//! and Lance query result projection all line up end-to-end.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test manager_upsert_query \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;

use firnflow_core::metrics::test_metrics;
use firnflow_core::{NamespaceId, NamespaceManager, UpsertRow};

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

fn unit_vector(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[axis] = 1.0;
    v
}

#[tokio::test]
#[ignore]
async fn upsert_then_query_returns_nearest_neighbor() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(bucket, minio_options(), test_metrics());
    let ns = NamespaceId::new(unique_namespace("upsert-query")).unwrap();

    // Four orthogonal unit vectors along the first four axes.
    let rows: Vec<UpsertRow> = vec![
        (1u64, unit_vector(0)).into(),
        (2u64, unit_vector(1)).into(),
        (3u64, unit_vector(2)).into(),
        (4u64, unit_vector(3)).into(),
    ];
    manager.upsert(&ns, rows).await.expect("upsert");

    // Query with the exact stored vector for id=1. It must come back
    // as the top hit with ~zero distance.
    let results = manager
        .query(&ns, unit_vector(0), 3, None, None)
        .await
        .expect("query");

    assert_eq!(results.results.len(), 3, "should return top-3 hits");
    let top = &results.results[0];
    assert_eq!(top.id, 1, "nearest neighbour of axis-0 must be id=1");
    assert!(
        top.score < 0.01,
        "self-distance should be ~0, got {}",
        top.score
    );
    assert_eq!(
        top.vector.len(),
        DIM,
        "returned vector width must match schema"
    );
    assert_eq!(
        top.vector[0], 1.0,
        "returned vector[0] must be the stored value"
    );
}

#[tokio::test]
#[ignore]
async fn upsert_validates_vector_dimension() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(bucket, minio_options(), test_metrics());
    let ns = NamespaceId::new(unique_namespace("dim-validation")).unwrap();

    // Establish the namespace dimension via a valid first upsert.
    let rows: Vec<UpsertRow> = vec![(1u64, unit_vector(0)).into()];
    manager.upsert(&ns, rows).await.expect("initial upsert");

    // Wrong-width vector — must fail validation before hitting the
    // backend now that the namespace dimension is established.
    let rows: Vec<UpsertRow> = vec![(2u64, vec![1.0_f32; DIM + 1]).into()];
    let err = manager
        .upsert(&ns, rows)
        .await
        .expect_err("upsert with wrong vector width must fail");
    let msg = format!("{err}");
    assert!(msg.contains("vector length"), "unexpected error: {msg}");
}
