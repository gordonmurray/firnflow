//! Single happy-path smoke test through `NamespaceService` against
//! object storage. Acts as the upgrade-validation tripwire when the
//! lancedb / lance pins change: the broader existing suite (cache
//! invalidation, connection pool, scalar index, multivector, etc.)
//! still has to pass, but this slim test runs first and fails fast
//! if a public API call shape has shifted under the pins or if the
//! storage round-trip itself stops working.
//!
//! The shape is intentionally small: upsert four orthogonal unit
//! vectors, query verbatim with one of them, assert the top hit is
//! the row we wrote. No cache assertions, no index build, no compact
//! — those are covered in the dedicated tests.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test service_query_smoke \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use firnflow_core::cache::NamespaceCache;
use firnflow_core::metrics::test_metrics;
use firnflow_core::{
    NamespaceId, NamespaceManager, NamespaceService, QueryRequest, StorageRoot, UpsertRow,
};

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
async fn service_query_happy_path() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let tmp = tempfile::tempdir().unwrap();
    let metrics = test_metrics();
    let manager = Arc::new(NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        minio_options(),
        Arc::clone(&metrics),
    ));
    let cache = Arc::new(
        NamespaceCache::new(
            16 * 1024 * 1024,
            tmp.path(),
            64 * 1024 * 1024,
            Arc::clone(&metrics),
        )
        .await
        .expect("build cache"),
    );
    let service = NamespaceService::new(
        Arc::clone(&manager),
        Arc::clone(&cache),
        Arc::clone(&metrics),
    );

    let ns = NamespaceId::new(unique_namespace("query-smoke")).unwrap();

    service
        .upsert(
            &ns,
            (0..4)
                .map(|i| UpsertRow::from((i as u64, unit_vector(i))))
                .collect(),
        )
        .await
        .expect("upsert");

    let req = QueryRequest {
        vector: unit_vector(2),
        vectors: None,
        k: 4,
        nprobes: None,
        text: None,
        filter: None,
        include_vector: true,
        semantic_cache: None,
    };
    let res = service.query(&ns, &req).await.expect("query");
    assert_eq!(res.results.len(), 4, "expected 4 hits for k=4");
    assert_eq!(
        res.results[0].id, 2,
        "expected the row matching the query vector to be top hit",
    );
}
