//! `NamespaceService` cache-aside cycle integration test.
//!
//! The cache generation is the Lance table version, so invalidation is
//! intrinsic to the data: every committed write advances the version,
//! and the next read derives a new cache key. There is no service-level
//! "bump" to bypass, which means the classic "write through the manager
//! to observe a stale read" trick no longer produces staleness — a
//! manager-level write advances the version just the same. The cache is
//! instead proven to be in the hot path via the reported
//! [`QueryCacheSource`].
//!
//! The loop:
//!
//! 1. upsert via the service → rows land in Lance (version advances)
//! 2. query → cache miss (`Backend`), manager runs the search, result
//!    serialised into foyer
//! 3. query again → cache hit (`ExactCache`), no backend call, same hits
//! 4. *side-channel* upsert directly through the manager → advances the
//!    table version
//! 5. query → cache miss (`Backend`) again, because the version moved;
//!    the fresh row is reflected (no stale read, unlike the old design)
//! 6. upsert via the service once more → version advances again
//! 7. query → cache miss (`Backend`), repopulates with every row
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test service_cache_aside \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use firnflow_core::cache::NamespaceCache;
use firnflow_core::metrics::test_metrics;
use firnflow_core::{
    NamespaceId, NamespaceManager, NamespaceService, QueryCacheSource, QueryRequest, StorageRoot,
    UpsertRow,
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
async fn service_cache_aside_follows_table_version() {
    // ---- setup ----
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

    let ns = NamespaceId::new(unique_namespace("cache-aside")).unwrap();
    let req = QueryRequest {
        vector: unit_vector(0),
        vectors: None,
        k: 10,
        nprobes: None,
        text: None,
        semantic_cache: None,
    };

    // ---- 1. upsert via service: two rows ----
    service
        .upsert(
            &ns,
            vec![
                UpsertRow::from((1, unit_vector(0))),
                UpsertRow::from((2, unit_vector(1))),
            ],
        )
        .await
        .expect("service.upsert initial");

    // ---- 2. first query: cache miss, populates with 2 hits ----
    let r1 = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("query #1");
    assert_eq!(r1.cache_source, QueryCacheSource::Backend, "step 2: miss");
    assert_eq!(r1.result.results.len(), 2, "step 2: expected 2 hits");

    // ---- 3. second query: cache hit, same 2 hits ----
    let r2 = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("query #2");
    assert_eq!(
        r2.cache_source,
        QueryCacheSource::ExactCache,
        "step 3: identical repeat must be served from the exact cache — \
         this is what proves the cache is in the hot path"
    );
    assert_eq!(r2.result.results.len(), 2, "step 3: same 2 cached hits");
    assert_eq!(
        r1.result.results[0].id, r2.result.results[0].id,
        "step 3: identical top hit across miss/hit"
    );

    // ---- 4. side-channel upsert via manager, bypassing the service ----
    manager
        .upsert(&ns, vec![UpsertRow::from((3, unit_vector(2)))])
        .await
        .expect("manager.upsert bypass");

    // ---- 5. third query: cache miss again — the manager write advanced
    //         the table version, so the generation changed and the fresh
    //         row is reflected. Under the old generation-counter design
    //         this returned the stale 2-result set; version-derived
    //         generation makes the bypass write visible. ----
    let r3 = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("query #3");
    assert_eq!(
        r3.cache_source,
        QueryCacheSource::Backend,
        "step 5: a manager-level write advances the table version, so the \
         next read misses the cache and reflects the new row"
    );
    assert_eq!(
        r3.result.results.len(),
        3,
        "step 5: the bypass row is visible — no stale read"
    );

    // ---- 6. upsert via service: advances the version again ----
    service
        .upsert(&ns, vec![UpsertRow::from((4, unit_vector(3)))])
        .await
        .expect("service.upsert invalidating");

    // ---- 7. fourth query: cache miss, repopulates with all 4 ----
    let r4 = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("query #4");
    assert_eq!(r4.cache_source, QueryCacheSource::Backend, "step 7: miss");
    assert_eq!(
        r4.result.results.len(),
        4,
        "step 7: after the version advanced the cache repopulates and \
         surfaces every row — the two from step 1, the bypass row from \
         step 4, and the new row from step 6"
    );
}

/// Deleting a namespace and recreating it under the same name must not
/// serve the deleted incarnation's cached results, even when the new
/// incarnation reaches the same Lance version. The generation folds in
/// the manifest commit timestamp, so the two incarnations key
/// differently. Regression for the delete/recreate collision.
#[tokio::test]
#[ignore]
async fn delete_recreate_does_not_serve_old_incarnation() {
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

    let ns = NamespaceId::new(unique_namespace("recreate")).unwrap();
    let req = QueryRequest {
        vector: unit_vector(0),
        vectors: None,
        k: 10,
        nprobes: None,
        text: None,
        semantic_cache: None,
    };

    // --- Incarnation A: one row, then cache the query result. ---
    service
        .upsert(&ns, vec![UpsertRow::from((1, unit_vector(0)))])
        .await
        .expect("upsert A");
    let a = service.query(&ns, &req).await.expect("query A");
    assert_eq!(a.results.len(), 1, "incarnation A has one row");
    let a_hit = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("query A repeat");
    assert_eq!(
        a_hit.cache_source,
        QueryCacheSource::ExactCache,
        "incarnation A's result is cached before the delete"
    );

    // --- Delete, then recreate under the same name with different data.
    //     Incarnation B reaches the same Lance version as A (create +
    //     one upsert = version 2) but holds two different rows. ---
    service.delete(&ns).await.expect("delete");
    service
        .upsert(
            &ns,
            vec![
                UpsertRow::from((2, unit_vector(1))),
                UpsertRow::from((3, unit_vector(2))),
            ],
        )
        .await
        .expect("upsert B");

    // The same query must miss: the recreated incarnation keys
    // differently from the deleted one despite the identical version.
    let b = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("query B");
    assert_eq!(
        b.cache_source,
        QueryCacheSource::Backend,
        "recreated namespace must miss the cache, not serve the deleted \
         incarnation's bytes"
    );
    assert_eq!(
        b.result.results.len(),
        2,
        "must reflect incarnation B's two rows, not incarnation A's one row"
    );
}
