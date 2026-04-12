//! Slice-6a integration test: per-namespace vector dimensions.
//!
//! Upserts into three fresh namespaces with dim=4, dim=8, and
//! dim=16 using a single `NamespaceManager` instance. Queries each
//! one and verifies that dimensions are honoured independently:
//! the right vectors come back with the right widths, and a
//! wrong-width upsert into an established namespace is rejected.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test manager_per_namespace_dim \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;

use firnflow_core::{NamespaceId, NamespaceManager};

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

/// Unit vector of the given dimension with a 1.0 at `axis`.
fn unit_vector(dim: usize, axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; dim];
    v[axis % dim] = 1.0;
    v
}

#[tokio::test]
#[ignore]
async fn three_namespaces_with_different_dims() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(bucket, minio_options());

    let ns4 = NamespaceId::new(unique_namespace("slice6a-dim4")).unwrap();
    let ns8 = NamespaceId::new(unique_namespace("slice6a-dim8")).unwrap();
    let ns16 = NamespaceId::new(unique_namespace("slice6a-dim16")).unwrap();

    // ---- upsert into each namespace with its own dimension ----
    manager
        .upsert(&ns4, vec![(1, unit_vector(4, 0)), (2, unit_vector(4, 1))])
        .await
        .expect("upsert dim=4");

    manager
        .upsert(
            &ns8,
            vec![
                (1, unit_vector(8, 0)),
                (2, unit_vector(8, 1)),
                (3, unit_vector(8, 2)),
            ],
        )
        .await
        .expect("upsert dim=8");

    manager
        .upsert(
            &ns16,
            vec![(1, unit_vector(16, 0)), (2, unit_vector(16, 1))],
        )
        .await
        .expect("upsert dim=16");

    // ---- verify dimensions are cached ----
    assert_eq!(manager.dim_for(&ns4), Some(4), "dim cache for ns4");
    assert_eq!(manager.dim_for(&ns8), Some(8), "dim cache for ns8");
    assert_eq!(manager.dim_for(&ns16), Some(16), "dim cache for ns16");

    // ---- query each namespace independently ----
    let r4 = manager
        .query(&ns4, unit_vector(4, 0), 2)
        .await
        .expect("query dim=4");
    assert_eq!(r4.results.len(), 2, "ns4 should have 2 rows");
    assert_eq!(
        r4.results[0].vector.len(),
        4,
        "ns4 result vectors must be dim=4"
    );
    assert_eq!(r4.results[0].id, 1, "nearest neighbour in ns4 is id=1");

    let r8 = manager
        .query(&ns8, unit_vector(8, 0), 3)
        .await
        .expect("query dim=8");
    assert_eq!(r8.results.len(), 3, "ns8 should have 3 rows");
    assert_eq!(
        r8.results[0].vector.len(),
        8,
        "ns8 result vectors must be dim=8"
    );

    let r16 = manager
        .query(&ns16, unit_vector(16, 0), 2)
        .await
        .expect("query dim=16");
    assert_eq!(r16.results.len(), 2, "ns16 should have 2 rows");
    assert_eq!(
        r16.results[0].vector.len(),
        16,
        "ns16 result vectors must be dim=16"
    );

    // ---- wrong-width upsert into an established namespace ----
    let err = manager
        .upsert(&ns4, vec![(99, unit_vector(8, 0))])
        .await
        .expect_err("upsert dim=8 vector into dim=4 namespace must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("vector length 8, expected 4"),
        "unexpected error: {msg}"
    );

    // ---- wrong-width query against an established namespace ----
    let err = manager
        .query(&ns8, unit_vector(4, 0), 1)
        .await
        .expect_err("query dim=4 vector against dim=8 namespace must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("vector length 4, expected 8"),
        "unexpected error: {msg}"
    );
}

/// Verify that dimension inference uses row[0] and validates
/// subsequent rows in the same request against it.
#[tokio::test]
#[ignore]
async fn first_upsert_infers_dim_and_validates_remaining_rows() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(bucket, minio_options());
    let ns = NamespaceId::new(unique_namespace("slice6a-infer")).unwrap();

    // Row 0 has dim=4, row 1 has dim=6 — must fail with a clear error.
    let err = manager
        .upsert(&ns, vec![(1, vec![1.0; 4]), (2, vec![1.0; 6])])
        .await
        .expect_err("mixed-width upsert must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("vector length 6, expected 4"),
        "error should cite the mismatch: {msg}"
    );
}
