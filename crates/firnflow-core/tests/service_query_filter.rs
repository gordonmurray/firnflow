//! Query filter behavior through `NamespaceService` on local storage.
//!
//! Covers exact-cache splitting by filter and semantic-cache rejection
//! for filtered requests without requiring MinIO.

use std::collections::HashMap;
use std::sync::Arc;

use firnflow_core::cache::NamespaceCache;
use firnflow_core::metrics::test_metrics;
use firnflow_core::{
    FirnflowError, NamespaceId, NamespaceManager, NamespaceService, QueryCacheSource, QueryRequest,
    SemanticCacheRequest, StorageRoot, UpsertRow,
};
use tempfile::TempDir;

const DIM: usize = 8;

fn unit_vector(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[axis] = 1.0;
    v
}

fn request(filter: Option<&str>) -> QueryRequest {
    QueryRequest {
        vector: unit_vector(0),
        vectors: None,
        k: 10,
        nprobes: None,
        text: None,
        filter: filter.map(str::to_string),
        include_vector: false,
        semantic_cache: None,
    }
}

async fn local_service() -> (NamespaceService, NamespaceId, TempDir, TempDir) {
    let dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let metrics = test_metrics();
    let manager = Arc::new(NamespaceManager::new(
        StorageRoot::local(dir.path()).unwrap(),
        HashMap::new(),
        Arc::clone(&metrics),
    ));
    let cache = Arc::new(
        NamespaceCache::new(
            16 * 1024 * 1024,
            cache_dir.path(),
            64 * 1024 * 1024,
            Arc::clone(&metrics),
        )
        .await
        .expect("cache"),
    );
    let service = NamespaceService::new(Arc::clone(&manager), cache, metrics);
    let ns = NamespaceId::new("service-query-filter").unwrap();

    let rows: Vec<UpsertRow> = vec![
        (1u64, unit_vector(0)).into(),
        (2u64, unit_vector(1)).into(),
        (3u64, unit_vector(2)).into(),
    ];
    service.upsert(&ns, rows).await.expect("seed upsert");

    (service, ns, dir, cache_dir)
}

#[tokio::test]
async fn filtered_and_unfiltered_queries_cache_independently() {
    let (service, ns, _dir, _cache_dir) = local_service().await;

    let unfiltered = request(None);
    let filtered = request(Some("id > 1"));

    let a = service
        .query_with_cache_source(&ns, &unfiltered)
        .await
        .expect("unfiltered #1");
    assert_eq!(a.cache_source, QueryCacheSource::Backend);
    let mut ids_a: Vec<u64> = a.result.results.iter().map(|r| r.id).collect();
    ids_a.sort_unstable();
    assert_eq!(ids_a, vec![1, 2, 3]);

    let b = service
        .query_with_cache_source(&ns, &filtered)
        .await
        .expect("filtered #1");
    assert_eq!(b.cache_source, QueryCacheSource::Backend);
    let mut ids_b: Vec<u64> = b.result.results.iter().map(|r| r.id).collect();
    ids_b.sort_unstable();
    assert_eq!(ids_b, vec![2, 3]);

    let a2 = service
        .query_with_cache_source(&ns, &unfiltered)
        .await
        .expect("unfiltered #2");
    assert_eq!(a2.cache_source, QueryCacheSource::ExactCache);
    assert_eq!(a2.result, a.result);

    let b2 = service
        .query_with_cache_source(&ns, &filtered)
        .await
        .expect("filtered #2");
    assert_eq!(b2.cache_source, QueryCacheSource::ExactCache);
    assert_eq!(b2.result, b.result);
}

#[tokio::test]
async fn distinct_filters_do_not_collide_in_exact_cache() {
    let (service, ns, _dir, _cache_dir) = local_service().await;

    let lt = request(Some("id < 3"));
    let gt = request(Some("id > 1"));

    let a = service
        .query_with_cache_source(&ns, &lt)
        .await
        .expect("lt filter");
    assert_eq!(a.cache_source, QueryCacheSource::Backend);
    let mut ids_a: Vec<u64> = a.result.results.iter().map(|r| r.id).collect();
    ids_a.sort_unstable();
    assert_eq!(ids_a, vec![1, 2]);

    let b = service
        .query_with_cache_source(&ns, &gt)
        .await
        .expect("gt filter");
    assert_eq!(b.cache_source, QueryCacheSource::Backend);
    let mut ids_b: Vec<u64> = b.result.results.iter().map(|r| r.id).collect();
    ids_b.sort_unstable();
    assert_eq!(ids_b, vec![2, 3]);
}

#[tokio::test]
async fn filtered_semantic_cache_request_is_rejected() {
    let (service, ns, _dir, _cache_dir) = local_service().await;
    let mut req = request(Some("id > 1"));
    req.semantic_cache = Some(SemanticCacheRequest {
        enabled: true,
        min_similarity: None,
    });

    let err = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect_err("filtered semantic-cache query should reject");
    match err {
        FirnflowError::InvalidRequest(msg) => assert!(msg.contains("filter"), "{msg}"),
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}
