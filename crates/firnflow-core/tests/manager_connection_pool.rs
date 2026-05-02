//! Issue-1 integration test: `NamespaceManager` connection pool.
//!
//! Proves three properties of the pool:
//!
//! 1. The handle for a namespace survives across calls. Two
//!    consecutive `upsert`s on the same namespace must use the
//!    cached connection + table — observable as a single pool
//!    entry and a warm-path latency below the cold path.
//! 2. `delete(ns)` evicts the handle. After a delete the pool no
//!    longer reports the namespace and the dim cache is cleared,
//!    so a follow-up upsert with a different dimension succeeds.
//! 3. `compact(ns)` evicts the handle. Compaction rewrites
//!    fragments; any cached `Table` view still pointing at the
//!    pre-compact file offsets is invalidated. The next query
//!    must reopen cleanly.
//!
//! The gauge is asserted via `CoreMetrics::cached_handles_value`,
//! which tracks the `firnflow_cached_handles` counter the API
//! exposes at `/metrics`.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test manager_connection_pool \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use firnflow_core::metrics::test_metrics;
use firnflow_core::{NamespaceId, NamespaceManager, UpsertRow};

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

/// Two orthogonal unit vectors of `dim` with ids 1 and 2.
fn seed_rows(dim: usize) -> Vec<UpsertRow> {
    let mut a = vec![0.0_f32; dim];
    a[0] = 1.0;
    let mut b = vec![0.0_f32; dim];
    b[1 % dim] = 1.0;
    vec![UpsertRow::from((1u64, a)), UpsertRow::from((2u64, b))]
}

#[tokio::test]
#[ignore]
async fn cached_handle_survives_across_operations() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let metrics = test_metrics();
    let manager = NamespaceManager::new(bucket, minio_options(), Arc::clone(&metrics));
    let ns = NamespaceId::new(unique_namespace("pool-reuse")).unwrap();

    assert_eq!(manager.pool_size(), 0, "pool starts empty");
    assert_eq!(metrics.cached_handles_value(), 0, "gauge starts at 0");

    // Cold upsert: opens a connection + creates the table, caches both.
    let t0 = Instant::now();
    manager
        .upsert(&ns, seed_rows(8))
        .await
        .expect("cold upsert");
    let cold_ms = t0.elapsed().as_millis();

    assert!(manager.is_pooled(&ns), "handle cached after first upsert");
    assert_eq!(manager.pool_size(), 1, "pool has exactly one entry");
    assert_eq!(
        metrics.cached_handles_value(),
        1,
        "gauge reflects the single cached handle"
    );

    // Warm upsert: must reuse the cached handle — no new connect(),
    // no new open_table().
    let t1 = Instant::now();
    manager
        .upsert(&ns, seed_rows(8))
        .await
        .expect("warm upsert");
    let warm_ms = t1.elapsed().as_millis();

    assert_eq!(
        manager.pool_size(),
        1,
        "pool size unchanged after warm upsert"
    );
    assert_eq!(
        metrics.cached_handles_value(),
        1,
        "gauge unchanged after warm upsert"
    );

    println!("cold upsert: {cold_ms}ms, warm upsert: {warm_ms}ms");
    assert!(
        warm_ms <= cold_ms,
        "warm upsert ({warm_ms}ms) should not be slower than cold ({cold_ms}ms)"
    );

    // A query on the same namespace should also stay on the fast
    // path — no pool growth, no eviction.
    let results = manager
        .query(
            &ns,
            vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            2,
            None,
            None,
        )
        .await
        .expect("query reuses pooled handle");
    assert_eq!(results.results.len(), 2, "query returns the two rows");
    assert_eq!(manager.pool_size(), 1, "query does not grow the pool");
}

#[tokio::test]
#[ignore]
async fn handle_evicted_on_namespace_delete() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let metrics = test_metrics();
    let manager = NamespaceManager::new(bucket, minio_options(), Arc::clone(&metrics));
    let ns = NamespaceId::new(unique_namespace("pool-delete")).unwrap();

    // Establish the namespace at dim=8.
    manager
        .upsert(&ns, seed_rows(8))
        .await
        .expect("seed upsert");
    assert!(manager.is_pooled(&ns));
    assert_eq!(manager.dim_for(&ns), Some(8));
    assert_eq!(metrics.cached_handles_value(), 1);

    manager.delete(&ns).await.expect("delete namespace");

    assert!(
        !manager.is_pooled(&ns),
        "delete must evict the pooled handle"
    );
    assert_eq!(
        metrics.cached_handles_value(),
        0,
        "gauge decremented on eviction"
    );
    assert_eq!(
        manager.dim_for(&ns),
        None,
        "dim cache cleared so the ns can be re-created at a new width"
    );

    // Reseed at a different dimension — only possible if the stale
    // pool entry (which held a dim=8 Table) is really gone.
    manager
        .upsert(&ns, seed_rows(16))
        .await
        .expect("reseed at dim=16 after delete");
    assert_eq!(manager.dim_for(&ns), Some(16));
    assert!(manager.is_pooled(&ns), "fresh handle cached after reseed");
}

#[tokio::test]
#[ignore]
async fn handle_evicted_after_compaction() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let metrics = test_metrics();
    let manager = NamespaceManager::new(bucket, minio_options(), Arc::clone(&metrics));
    let ns = NamespaceId::new(unique_namespace("pool-compact")).unwrap();

    // Write enough fragments to make compaction meaningful.
    for _ in 0..5 {
        manager
            .upsert(&ns, seed_rows(8))
            .await
            .expect("seed upsert");
    }
    assert!(manager.is_pooled(&ns), "pool populated after seeding");
    assert_eq!(metrics.cached_handles_value(), 1);

    manager.compact(&ns).await.expect("compact");
    assert!(
        !manager.is_pooled(&ns),
        "compaction must evict the pooled handle (fragment offsets change)"
    );
    assert_eq!(
        metrics.cached_handles_value(),
        0,
        "gauge decremented by compaction eviction"
    );

    // Query path must still succeed — the next call reopens a
    // fresh Table against the post-compact manifest.
    let results = manager
        .query(
            &ns,
            vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            2,
            None,
            None,
        )
        .await
        .expect("query after compaction reopens the table");
    assert_eq!(
        results.results.len(),
        2,
        "compacted data is still queryable"
    );
    assert!(
        manager.is_pooled(&ns),
        "query re-populated the pool after the compact-triggered eviction"
    );
    assert_eq!(metrics.cached_handles_value(), 1);
}

#[tokio::test]
#[ignore]
async fn handle_evicted_after_scalar_index_build() {
    // Issue #24: building a BTree scalar index bumps the table
    // manifest the same way `create_index` and `create_fts_index`
    // do, so the cached handle must be evicted to force the next
    // operation to open a fresh Table view.
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let metrics = test_metrics();
    let manager = NamespaceManager::new(bucket, minio_options(), Arc::clone(&metrics));
    let ns = NamespaceId::new(unique_namespace("pool-scalar-index")).unwrap();

    manager
        .upsert(&ns, seed_rows(8))
        .await
        .expect("seed upsert");
    assert!(manager.is_pooled(&ns), "pool populated after upsert");
    assert_eq!(metrics.cached_handles_value(), 1);

    manager
        .create_scalar_index(&ns, "_ingested_at")
        .await
        .expect("scalar index build");
    assert!(
        !manager.is_pooled(&ns),
        "scalar index build must evict the pooled handle"
    );
    assert_eq!(
        metrics.cached_handles_value(),
        0,
        "gauge decremented by scalar-index eviction"
    );

    // Reject unsupported columns — the validation lives in
    // `NamespaceManager::create_scalar_index`, not the API layer.
    let err = manager
        .create_scalar_index(&ns, "id")
        .await
        .expect_err("must reject non-whitelisted column");
    let msg = err.to_string();
    assert!(
        msg.contains("not supported"),
        "expected 'not supported' in error, got {msg}"
    );
}
