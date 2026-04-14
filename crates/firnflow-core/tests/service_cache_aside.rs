//! Slice-1b integration test: `NamespaceService` cache-aside cycle.
//!
//! Proves the full loop where the three spike results meet:
//!
//! 1. upsert via the service → rows land in Lance, cache is invalidated
//! 2. query via the service → cache miss, manager runs the search,
//!    result is serialised into foyer
//! 3. query again → cache hit, no backend call, same bytes returned
//! 4. *side-channel* upsert directly through the manager, bypassing
//!    the service → the cache is intentionally not invalidated
//! 5. query via the service → still a cache hit; stale result
//!    returned (this is the whole point of step 4 — if it returned
//!    fresh data here the cache would be a no-op and the test
//!    wouldn't be testing invalidation at all)
//! 6. upsert via the service once more → bumps the generation
//! 7. query via the service → cache miss again, repopulates with
//!    the fresh post-step-4 rows
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
use firnflow_core::{NamespaceId, NamespaceManager, NamespaceService, QueryRequest, UpsertRow};

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
async fn service_cache_aside_invalidates_on_upsert() {
    // ---- setup ----
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let tmp = tempfile::tempdir().unwrap();
    let metrics = test_metrics();
    let manager = Arc::new(NamespaceManager::new(
        bucket,
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

    let ns = NamespaceId::new(unique_namespace("slice1b")).unwrap();
    let req = QueryRequest {
        vector: unit_vector(0),
        k: 10,
        nprobes: None,
        text: None,
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
    let r1 = service.query(&ns, &req).await.expect("query #1");
    assert_eq!(r1.results.len(), 2, "step 2: expected 2 cached hits");

    // ---- 3. second query: cache hit, same 2 hits ----
    let r2 = service.query(&ns, &req).await.expect("query #2");
    assert_eq!(r2.results.len(), 2, "step 3: expected same 2 cached hits");
    assert_eq!(
        r1.results[0].id, r2.results[0].id,
        "step 3: identical top hit across hit/miss"
    );

    // ---- 4. side-channel upsert via manager, bypassing the cache ----
    manager
        .upsert(&ns, vec![UpsertRow::from((3, unit_vector(2)))])
        .await
        .expect("manager.upsert bypass");

    // ---- 5. third query: still a cache hit, still 2 hits ----
    let r3 = service.query(&ns, &req).await.expect("query #3");
    assert_eq!(
        r3.results.len(),
        2,
        "step 5: cache must still serve the stale 2-result set, \
         not the fresh 3-row table. If this fails, the cache is \
         a no-op and the test is not exercising invalidation."
    );

    // ---- 6. upsert via service: bumps generation, evicts stale ----
    service
        .upsert(&ns, vec![UpsertRow::from((4, unit_vector(3)))])
        .await
        .expect("service.upsert invalidating");

    // ---- 7. fourth query: cache miss, repopulates with all 4 ----
    let r4 = service.query(&ns, &req).await.expect("query #4");
    assert_eq!(
        r4.results.len(),
        4,
        "step 7: after invalidation the cache must repopulate and \
         surface every row — the two from step 1, the bypass row \
         from step 4, and the new row from step 6."
    );
}
