//! Result-shape controls on the query read path.
//!
//! Covers:
//!
//! 1. Default queries still return the stored vector and now carry
//!    the row's `_ingested_at` timestamp (parity with `/list`).
//! 2. `include_vector: false` omits the stored vector while id,
//!    score, text, and timestamp survive — and the flag splits the
//!    exact-cache key, so full and vector-light payloads never
//!    collide and both hit independently on repeat.
//! 3. The semantic sidecar never answers a request with bytes whose
//!    payload shape differs from what the caller asked for.
//! 4. A cached payload this build cannot decode (the pre-upgrade
//!    wire format recovered from the NVMe tier) degrades to a cache
//!    miss that self-heals on the next call — not a 500.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test service_result_projection \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use firnflow_core::cache::NamespaceCache;
use firnflow_core::metrics::test_metrics;
use firnflow_core::service::hash_query_for_cache;
use firnflow_core::{
    NamespaceId, NamespaceManager, NamespaceService, QueryCacheSource, QueryRequest,
    SemanticCacheRequest, StorageRoot, UpsertRow,
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
    let other = (axis + 1) % DIM;
    let main = (1.0_f32 - drift * drift).sqrt();
    let mut v = vec![0.0_f32; DIM];
    v[axis] = main;
    v[other] = drift;
    v
}

fn request(vector: Vec<f32>, include_vector: bool) -> QueryRequest {
    QueryRequest {
        vector,
        vectors: None,
        k: 10,
        nprobes: None,
        text: None,
        filter: None,
        include_vector,
        semantic_cache: None,
    }
}

fn semantic_request(vector: Vec<f32>, include_vector: bool) -> QueryRequest {
    QueryRequest {
        semantic_cache: Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: Some(0.9),
        }),
        ..request(vector, include_vector)
    }
}

async fn build_service() -> (
    Arc<NamespaceService>,
    Arc<NamespaceManager>,
    Arc<NamespaceCache>,
    NamespaceId,
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
        Arc::clone(&cache),
        Arc::clone(&metrics),
    ));
    let ns = NamespaceId::new(unique_namespace("projection")).unwrap();

    service
        .upsert(
            &ns,
            (0..3)
                .map(|i| UpsertRow {
                    id: i as u64 + 1,
                    vector: unit_vector(i),
                    vectors: None,
                    text: Some(format!("row {}", i + 1)),
                })
                .collect(),
        )
        .await
        .expect("seed upsert");

    (service, manager, cache, ns)
}

#[tokio::test]
#[ignore]
async fn include_vector_splits_cache_key_and_omits_payload() {
    let (service, _manager, _cache, ns) = build_service().await;

    // ---- 1. default shape: vector + timestamp present ----
    let full_req = request(unit_vector(0), true);
    let full = service
        .query_with_cache_source(&ns, &full_req)
        .await
        .expect("full query");
    assert_eq!(full.cache_source, QueryCacheSource::Backend);
    assert_eq!(full.result.results.len(), 3);
    let top = &full.result.results[0];
    assert_eq!(top.id, 1);
    assert_eq!(
        top.vector.as_ref().map(|v| v.len()),
        Some(DIM),
        "default queries must return the stored vector"
    );
    assert_eq!(top.text.as_deref(), Some("row 1"));
    assert!(
        top.ingested_at_micros.is_some_and(|t| t > 0),
        "query hits must carry the row's _ingested_at timestamp"
    );

    let full_repeat = service
        .query_with_cache_source(&ns, &full_req)
        .await
        .expect("full repeat");
    assert_eq!(full_repeat.cache_source, QueryCacheSource::ExactCache);

    // ---- 2. vector-light shape: separate key, no vectors ----
    let light_req = request(unit_vector(0), false);
    let light = service
        .query_with_cache_source(&ns, &light_req)
        .await
        .expect("light query");
    assert_eq!(
        light.cache_source,
        QueryCacheSource::Backend,
        "include_vector must participate in the cache key — the light \
         request cannot be served from the full request's entry"
    );
    assert_eq!(light.result.results.len(), 3);
    for hit in &light.result.results {
        assert!(
            hit.vector.is_none(),
            "include_vector=false must omit the stored vector, got {:?}",
            hit.vector
        );
        assert!(hit.text.is_some(), "text survives the projection");
        assert!(
            hit.ingested_at_micros.is_some_and(|t| t > 0),
            "timestamp survives the projection"
        );
    }
    assert_eq!(
        light.result.results[0].id, full.result.results[0].id,
        "the search itself is unchanged — same ranking either way"
    );

    // ---- 3. both shapes hit independently on repeat ----
    let light_repeat = service
        .query_with_cache_source(&ns, &light_req)
        .await
        .expect("light repeat");
    assert_eq!(light_repeat.cache_source, QueryCacheSource::ExactCache);
    let full_again = service
        .query_with_cache_source(&ns, &full_req)
        .await
        .expect("full again");
    assert_eq!(
        full_again.cache_source,
        QueryCacheSource::ExactCache,
        "the light entry must not have evicted or replaced the full one"
    );
}

#[tokio::test]
#[ignore]
async fn semantic_sidecar_does_not_cross_payload_shapes() {
    let (service, _manager, _cache, ns) = build_service().await;

    // Seed the sidecar with a full-payload entry.
    let seed = service
        .query_with_cache_source(&ns, &semantic_request(unit_vector(0), true))
        .await
        .expect("seed query");
    assert_eq!(seed.cache_source, QueryCacheSource::Backend);

    // Control: a near-duplicate asking for the same payload shape is
    // served from the sidecar.
    let same_shape = service
        .query_with_cache_source(&ns, &semantic_request(near_unit_vector(0, 0.05), true))
        .await
        .expect("same-shape near-duplicate");
    assert_eq!(
        same_shape.cache_source,
        QueryCacheSource::SemanticCache,
        "near-duplicate with a matching payload shape should reuse the \
         sidecar entry"
    );

    // A near-duplicate asking for the vector-light shape must not be
    // handed the full-payload bytes.
    let other_shape = service
        .query_with_cache_source(&ns, &semantic_request(near_unit_vector(0, 0.07), false))
        .await
        .expect("other-shape near-duplicate");
    assert_eq!(
        other_shape.cache_source,
        QueryCacheSource::Backend,
        "the sidecar must skip entries whose include_vector differs \
         from the incoming request"
    );
    assert!(
        other_shape
            .result
            .results
            .iter()
            .all(|h| h.vector.is_none()),
        "the vector-light request must come back vector-light"
    );
}

/// The result-cache payload layout before this release: `vector` was
/// a required `Vec<f32>` and there was no `ingested_at_micros`. The
/// NVMe tier persists across restarts and the cache generation is the
/// Lance table version, so entries written by an older build can be
/// recovered and hit at the same key after an upgrade.
#[derive(Serialize)]
struct LegacyQueryResult {
    id: u64,
    score: f32,
    vector: Vec<f32>,
    text: Option<String>,
}

#[derive(Serialize)]
struct LegacyQueryResultSet {
    query_id: String,
    results: Vec<LegacyQueryResult>,
}

#[tokio::test]
#[ignore]
async fn undecodable_cached_payload_is_a_miss_not_an_error() {
    let (service, manager, cache, ns) = build_service().await;

    let req = request(unit_vector(0), true);

    // Populate the entry normally first, proving the seed below
    // lands on a key the read path genuinely consults.
    let first = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("populate");
    assert_eq!(first.cache_source, QueryCacheSource::Backend);
    let repeat = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("repeat");
    assert_eq!(repeat.cache_source, QueryCacheSource::ExactCache);

    // Overwrite the entry with bytes in the pre-upgrade layout —
    // the same state an NVMe-recovered entry from an older build
    // would be in.
    let legacy = LegacyQueryResultSet {
        query_id: String::new(),
        results: vec![LegacyQueryResult {
            id: 1,
            score: 0.0,
            vector: unit_vector(0),
            text: Some("row 1".into()),
        }],
    };
    let legacy_bytes =
        bincode::serde::encode_to_vec(&legacy, bincode::config::standard()).expect("encode legacy");
    let generation = manager.generation(&ns).await.expect("generation");
    let hash = hash_query_for_cache(&req).expect("hash");
    cache.populate_with_generation(&ns, generation, hash, legacy_bytes);

    // The unreadable entry must degrade to a miss: the query runs
    // against the backend and succeeds.
    let healed = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("decode failure must not surface as an error");
    assert_eq!(
        healed.cache_source,
        QueryCacheSource::Backend,
        "an undecodable cached payload is a miss, not a hit or a 500"
    );
    assert_eq!(healed.result.results.len(), 3);

    // ... and the fall-through repopulated the entry in the current
    // format, so the next call hits again.
    let after = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("self-heal repeat");
    assert_eq!(after.cache_source, QueryCacheSource::ExactCache);
}
