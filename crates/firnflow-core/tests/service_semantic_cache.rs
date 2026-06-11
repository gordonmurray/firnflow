//! `NamespaceService` opt-in semantic-cache integration test.
//!
//! Proves the end-to-end shape:
//!
//! 1. opt-in repeat-query exact hit still wins (semantic layer not consulted)
//! 2. opt-in near-duplicate query: exact miss → semantic hit
//! 3. opt-out near-duplicate query: exact miss → semantic miss (counter still ticks)
//! 4. write invalidates both exact and semantic layers
//! 5. semantic-rejection counter fires for ineligible request shapes
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test service_semantic_cache \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use firnflow_core::cache::NamespaceCache;
use firnflow_core::metrics::test_metrics;
use firnflow_core::{
    NamespaceId, NamespaceManager, NamespaceService, QueryRequest, SemanticCacheRequest,
    StorageRoot, UpsertRow,
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

fn near_unit_vector(axis: usize, drift: f32) -> Vec<f32> {
    // Tilt slightly off-axis so the cosine similarity is high but
    // not exactly 1.0. Picking a second axis keeps the resulting
    // vector inside the same DIM-wide column.
    let other = (axis + 1) % DIM;
    let main = (1.0_f32 - drift * drift).sqrt();
    let mut v = vec![0.0_f32; DIM];
    v[axis] = main;
    v[other] = drift;
    v
}

fn semantic_request(vector: Vec<f32>, threshold: Option<f32>) -> QueryRequest {
    QueryRequest {
        vector,
        vectors: None,
        k: 5,
        nprobes: None,
        text: None,
        include_vector: true,
        semantic_cache: Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: threshold,
        }),
    }
}

fn plain_request(vector: Vec<f32>) -> QueryRequest {
    QueryRequest {
        vector,
        vectors: None,
        k: 5,
        nprobes: None,
        text: None,
        include_vector: true,
        semantic_cache: None,
    }
}

async fn build_service() -> (
    Arc<NamespaceService>,
    NamespaceId,
    Arc<firnflow_core::CoreMetrics>,
) {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let tmp = Box::leak(Box::new(tempfile::tempdir().unwrap()));
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
    let service = Arc::new(NamespaceService::new(
        Arc::clone(&manager),
        cache,
        Arc::clone(&metrics),
    ));
    let ns = NamespaceId::new(unique_namespace("semantic")).unwrap();

    // Seed the namespace with a handful of basis rows so queries
    // can return results without hitting the empty-namespace
    // short-circuit.
    let rows = (0..DIM)
        .map(|i| UpsertRow::from((i as u64, unit_vector(i))))
        .collect::<Vec<_>>();
    service.upsert(&ns, rows).await.expect("seed upsert");
    (service, ns, metrics)
}

#[tokio::test]
#[ignore]
async fn exact_repeat_short_circuits_before_semantic_layer() {
    let (service, ns, metrics) = build_service().await;
    let req = semantic_request(unit_vector(0), Some(0.99));

    // First call: exact miss, backend runs, both layers populated.
    let r1 = service.query(&ns, &req).await.expect("query #1");
    assert!(!r1.results.is_empty());
    assert_eq!(metrics.semantic_cache_hits_value(&ns), 0);
    // First call should have rejected with `empty_index` (semantic
    // sidecar starts empty for this generation) before populating.
    assert_eq!(
        metrics.semantic_cache_rejections_value(&ns, "empty_index"),
        1,
        "first eligible request should record one empty-index rejection",
    );

    // Second call, identical request: should be an exact hit.
    // Semantic layer must not be consulted — its rejection counter
    // stays at 1 and the hit counter at 0.
    let r2 = service.query(&ns, &req).await.expect("query #2");
    assert_eq!(r1, r2);
    assert_eq!(metrics.semantic_cache_hits_value(&ns), 0);
    assert_eq!(
        metrics.semantic_cache_rejections_value(&ns, "empty_index"),
        1,
    );
}

#[tokio::test]
#[ignore]
async fn near_duplicate_opt_in_hits_semantic_cache() {
    let (service, ns, metrics) = build_service().await;

    let seed = semantic_request(unit_vector(0), Some(0.95));
    let first = service.query(&ns, &seed).await.expect("seed query");

    // Slightly different vector — same dominant axis with a small
    // tilt. Cosine sim ≈ sqrt(1 - drift^2) ≈ 0.9987 at drift=0.05.
    let probe = semantic_request(near_unit_vector(0, 0.05), Some(0.95));
    let reuse = service.query(&ns, &probe).await.expect("near-duplicate");

    assert_eq!(
        reuse.results, first.results,
        "semantic hit should reuse the cached top-k bytes verbatim",
    );
    assert_eq!(metrics.semantic_cache_hits_value(&ns), 1);
}

#[tokio::test]
#[ignore]
async fn opt_out_near_duplicate_runs_backend() {
    let (service, ns, metrics) = build_service().await;

    // Opt-in to populate the sidecar.
    let seed = semantic_request(unit_vector(0), Some(0.95));
    service.query(&ns, &seed).await.expect("seed query");

    // Identical-shape query but with opt-out: must skip the sidecar
    // entirely. No hit, no miss, no rejection counter movement —
    // the semantic layer is bypassed.
    let opt_out = plain_request(near_unit_vector(0, 0.05));
    let _ = service.query(&ns, &opt_out).await.expect("opt-out query");
    assert_eq!(metrics.semantic_cache_hits_value(&ns), 0);
    assert_eq!(metrics.semantic_cache_misses_value(&ns), 0);
}

#[tokio::test]
#[ignore]
async fn write_invalidates_semantic_layer() {
    let (service, ns, metrics) = build_service().await;

    let seed = semantic_request(unit_vector(0), Some(0.95));
    service.query(&ns, &seed).await.expect("seed query");
    // First call always records one empty-index rejection.
    assert_eq!(
        metrics.semantic_cache_rejections_value(&ns, "empty_index"),
        1,
    );

    // Write — bumps the generation; the sidecar must drop its list.
    service
        .upsert(&ns, vec![UpsertRow::from((99, unit_vector(7)))])
        .await
        .expect("invalidating upsert");

    // Probe a near-duplicate of the seed query. Pre-write this
    // would have been a semantic hit; post-write the sidecar is
    // empty so it should fall through to the backend and increment
    // the `empty_index` rejection counter for a second time.
    let probe = semantic_request(near_unit_vector(0, 0.05), Some(0.95));
    let _ = service.query(&ns, &probe).await.expect("post-write probe");
    assert_eq!(
        metrics.semantic_cache_rejections_value(&ns, "empty_index"),
        2,
        "post-write probe should see an empty semantic index",
    );
    assert_eq!(metrics.semantic_cache_hits_value(&ns), 0);
}

#[tokio::test]
#[ignore]
async fn k_mismatch_does_not_reuse() {
    let (service, ns, metrics) = build_service().await;

    let seed = QueryRequest {
        vector: unit_vector(0),
        vectors: None,
        k: 5,
        nprobes: None,
        text: None,
        include_vector: true,
        semantic_cache: Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: Some(0.95),
        }),
    };
    service.query(&ns, &seed).await.expect("seed query");

    // Identical vector, different k — the sidecar candidate's k=5
    // must be rejected when k=10 is requested.
    let probe = QueryRequest {
        vector: unit_vector(0),
        vectors: None,
        k: 10,
        nprobes: None,
        text: None,
        include_vector: true,
        semantic_cache: Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: Some(0.95),
        }),
    };
    let _ = service.query(&ns, &probe).await.expect("k-mismatch query");
    assert_eq!(metrics.semantic_cache_hits_value(&ns), 0);
    assert!(metrics.semantic_cache_misses_value(&ns) >= 1);
}

#[tokio::test]
#[ignore]
async fn ineligible_request_returns_400_at_service_boundary() {
    let (service, ns, _metrics) = build_service().await;

    // Opt-in semantic + FTS query → InvalidRequest, never reaches
    // the cache.
    let req = QueryRequest {
        vector: unit_vector(0),
        vectors: None,
        k: 5,
        nprobes: None,
        text: Some("anything".into()),
        include_vector: true,
        semantic_cache: Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: None,
        }),
    };
    let err = service.query(&ns, &req).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("FTS") || msg.contains("hybrid"),
        "expected FTS/hybrid rejection, got {msg}",
    );
}
