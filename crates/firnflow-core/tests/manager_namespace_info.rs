//! Integration test for `NamespaceManager::info` (issue #14).
//!
//! Builds indexes synchronously at the manager layer so the index
//! flags can be asserted without racing a background task. Covers the
//! "no table yet" (None / 404 at the API) case, the metadata shape
//! after an upsert, and the scalar + FTS index flags flipping to true
//! after a build. Vector-index classification is exercised by the unit
//! test on `classify_index_types`.
//!
//! Gated `#[ignore]`: needs MinIO up.

use std::collections::HashMap;

use firnflow_core::metrics::test_metrics;
use firnflow_core::{NamespaceId, NamespaceManager, StorageRoot, UpsertRow, VectorKind};

const DIM: usize = 4;

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

fn row(id: u64, axis: usize, text: &str) -> UpsertRow {
    let mut vector = vec![0.0_f32; DIM];
    vector[axis] = 1.0;
    UpsertRow {
        id,
        vector,
        vectors: None,
        text: Some(text.to_string()),
    }
}

#[tokio::test]
#[ignore]
async fn info_reports_namespace_state_and_index_flags() {
    let mgr = manager();
    let ns = NamespaceId::new(unique_namespace("info")).unwrap();

    // A namespace with no table yet has no info — the API maps this to 404.
    assert!(
        mgr.info(&ns).await.unwrap().is_none(),
        "namespace with no data should report no info"
    );

    mgr.upsert(
        &ns,
        vec![row(1, 0, "alpha"), row(2, 1, "beta"), row(3, 2, "gamma")],
    )
    .await
    .expect("upsert");

    let info = mgr
        .info(&ns)
        .await
        .unwrap()
        .expect("namespace exists after upsert");
    assert_eq!(info.namespace, ns.to_string());
    assert_eq!(info.kind, VectorKind::Single);
    assert_eq!(info.vector_dim, DIM);
    assert_eq!(info.row_count, 3);
    assert!(info.fragment_count >= 1, "at least one data fragment");
    assert!(!info.has_vector_index);
    assert!(!info.has_fts_index);
    // The first upsert auto-builds a BTree on `id`, so the scalar-index
    // flag is set even before any explicit index build.
    assert!(
        info.has_scalar_index,
        "first upsert auto-builds the id index"
    );
    assert!(info.table_version >= 1, "version advances on commits");

    // Both builds are synchronous at the manager layer.
    mgr.create_scalar_index(&ns, "_ingested_at")
        .await
        .expect("scalar index");
    mgr.create_fts_index(&ns).await.expect("fts index");

    let after = mgr.info(&ns).await.unwrap().expect("still exists");
    assert!(after.has_scalar_index, "scalar index should now be present");
    assert!(after.has_fts_index, "fts index should now be present");
    assert_eq!(after.row_count, 3, "index builds do not change row count");
    assert!(
        after.table_version > info.table_version,
        "each index build is a commit that advances the version"
    );
}
