//! Index-cache reachability probe.
//!
//! Installs a no-op [`NoOpCacheBackend`] on a `lance::session::Session`,
//! threads the session through `NamespaceManager`, runs a real
//! query against MinIO, and confirms Lance actually dispatches
//! cache-backend traffic during the query.
//!
//! The probe builds a small IVF_PQ index (the cache holds
//! IVF/PQ centroids, HNSW graphs, FTS postings, etc.) so the
//! indexed query has something to cache. Asserts that at least
//! one of `get`, `insert`, `get_or_insert` counters moved during
//! the query — without that signal we have no proof the custom
//! backend is on the hot path, regardless of whether
//! `ConnectBuilder::session(...)` accepted the install.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test cachebackend_session_probe \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use firnflow_core::cache::NamespaceCache;
use firnflow_core::cachebackend::{CacheBackendCounters, NoOpCacheBackend};
use firnflow_core::metrics::test_metrics;
use firnflow_core::{
    NamespaceId, NamespaceManager, NamespaceService, QueryRequest, StorageRoot, UpsertRow,
};
use lance_core::cache::CacheBackend;
use lancedb::{ObjectStoreRegistry, Session};

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
async fn no_op_cachebackend_receives_calls_during_indexed_query() {
    // ---- assemble a session carrying the no-op CacheBackend ----
    let counters = Arc::new(CacheBackendCounters::default());
    let backend: Arc<dyn CacheBackend> = Arc::new(NoOpCacheBackend::new(Arc::clone(&counters)));
    let registry = Arc::new(ObjectStoreRegistry::default());
    // metadata_cache_size of 8 MB is the moka default-class
    // sizing; the index cache replaces our backend, so this only
    // affects metadata caching which we are not measuring here.
    let session = Arc::new(Session::with_index_cache_backend(
        Arc::clone(&backend),
        8 * 1024 * 1024,
        Arc::clone(&registry),
    ));

    // ---- assemble manager + service with the session attached ----
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let tmp = tempfile::tempdir().unwrap();
    let metrics = test_metrics();
    let manager = Arc::new(
        NamespaceManager::new(
            StorageRoot::s3_bucket(&bucket).unwrap(),
            minio_options(),
            Arc::clone(&metrics),
        )
        .with_session(Arc::clone(&session)),
    );
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
    let ns = NamespaceId::new(unique_namespace("cb-probe")).unwrap();

    // ---- seed corpus large enough to train IVF_PQ ----
    //
    // 260 rows × 1 vector each is just above Lance's 256 PQ
    // training floor. Each vector points along a single axis
    // cycling through `0..DIM`; the deterministic distribution
    // gives the IVF clusterer mass at every centroid.
    let rows: Vec<UpsertRow> = (0..260u64)
        .map(|i| UpsertRow::from((i, unit_vector((i as usize) % DIM))))
        .collect();
    service.upsert(&ns, rows).await.expect("seed upsert");

    // ---- build the vector index ----
    //
    // `num_sub_vectors = 2` so each PQ codebook spans `DIM/2 = 4`
    // dimensions, avoiding the degenerate tiny-PQ shape observed
    // with Lance 6 when `num_sub_vectors = 1` on very small
    // vectors. The same constraint applies to single-vector PQ
    // at small dims. `create_index` evicts the manager's pooled
    // handle, so the *next* query path will re-`connect` through
    // the same session and therefore the same `CacheBackend`.
    manager
        .create_index(&ns, Some(4), Some(2))
        .await
        .expect("index build");

    let before = counters.snapshot();
    eprintln!("counters before query: {before:?}");

    // ---- run the indexed query ----
    let req = QueryRequest {
        vector: unit_vector(0),
        vectors: None,
        k: 5,
        nprobes: None,
        text: None,
    };
    let res = service.query(&ns, &req).await.expect("indexed query");
    assert!(!res.results.is_empty(), "indexed query returned no hits");

    let after = counters.snapshot();
    eprintln!("counters after query:  {after:?}");

    // ---- assert backend traffic landed ----
    //
    // The exact distribution between `get` / `insert` /
    // `get_or_insert` depends on Lance's internals (and may
    // shift between minor versions). What we require is that
    // *some* call landed in the cache layer during the indexed
    // query — that proves `ConnectBuilder::session(...)` carried
    // our backend all the way down to where Lance opens its
    // IVF/PQ index. The probe deliberately keeps the assertion
    // shape coarse so it does not pin a particular Lance
    // implementation detail.
    let any_before = before.get + before.insert + before.get_or_insert;
    let any_after = after.get + after.insert + after.get_or_insert;
    assert!(
        any_after > any_before,
        "indexed query did not dispatch into the custom CacheBackend; \
         before={before:?} after={after:?}"
    );
}
