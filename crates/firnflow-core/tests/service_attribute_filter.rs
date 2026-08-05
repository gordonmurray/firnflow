//! Cache behaviour of queries filtered on attribute columns, through
//! `NamespaceService` on local storage.
//!
//! A predicate over an attribute column is stable, so a repeat of the
//! same filtered query is expected to come back from the exact cache
//! rather than the backend. That is not automatic: the read path first
//! plans the predicate against a schema the manager reconstructs, and
//! a predicate it cannot plan is treated as uncacheable. These tests
//! pin the hit, so a change to either the reconstruction or the
//! analysis cannot quietly cost the cache on the queries attribute
//! columns exist to serve.

use std::collections::HashMap;
use std::sync::Arc;

use firnflow_core::cache::NamespaceCache;
use firnflow_core::metrics::test_metrics;
use firnflow_core::{
    AttributeColumn, AttributeType, AttributeValue, NamespaceId, NamespaceManager,
    NamespaceService, QueryCacheSource, QueryRequest, StorageRoot, UpsertRow,
};
use tempfile::TempDir;

const DIM: usize = 4;

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

fn row(id: u64, axis: usize, section: &str) -> UpsertRow {
    UpsertRow {
        id,
        vector: unit_vector(axis),
        vectors: None,
        text: None,
        attributes: [(
            "section".to_string(),
            Some(AttributeValue::String(section.to_string())),
        )]
        .into_iter()
        .collect(),
    }
}

async fn seeded() -> (NamespaceService, NamespaceId, TempDir, TempDir) {
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
    let ns = NamespaceId::new("service-attribute-filter").unwrap();

    service
        .upsert(&ns, vec![(1u64, unit_vector(0)).into()])
        .await
        .expect("create the namespace");
    service
        .declare_attributes(
            &ns,
            &[AttributeColumn::new("section", AttributeType::String)],
        )
        .await
        .expect("declare");
    service
        .upsert(
            &ns,
            vec![
                row(1, 0, "warnings"),
                row(2, 1, "dosage"),
                row(3, 2, "warnings"),
            ],
        )
        .await
        .expect("seed");

    (service, ns, dir, cache_dir)
}

#[tokio::test]
async fn a_repeat_attribute_filtered_query_hits_the_exact_cache() {
    let (service, ns, _dir, _cache_dir) = seeded().await;
    let req = request(Some("section = 'warnings'"));

    let first = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("first");
    assert_eq!(first.cache_source, QueryCacheSource::Backend);
    let mut ids: Vec<u64> = first.result.results.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 3]);

    let second = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("second");
    assert_eq!(
        second.cache_source,
        QueryCacheSource::ExactCache,
        "an attribute predicate is stable, so the repeat must be served from cache"
    );
    assert_eq!(second.result, first.result);
}

/// Two predicates over the same column are different queries. The
/// cache key covers the predicate text, so they must not collide.
#[tokio::test]
async fn different_attribute_predicates_do_not_share_an_entry() {
    let (service, ns, _dir, _cache_dir) = seeded().await;

    let warnings = service
        .query_with_cache_source(&ns, &request(Some("section = 'warnings'")))
        .await
        .expect("warnings");
    let dosage = service
        .query_with_cache_source(&ns, &request(Some("section = 'dosage'")))
        .await
        .expect("dosage");

    assert_eq!(dosage.cache_source, QueryCacheSource::Backend);
    assert_eq!(warnings.result.results.len(), 2);
    assert_eq!(dosage.result.results.len(), 1);
    assert_eq!(dosage.result.results[0].id, 2);
}

/// A write turns the cache over, so a filtered query that ran before
/// the write does not keep serving its old rows.
#[tokio::test]
async fn a_write_strands_a_cached_attribute_filtered_result() {
    let (service, ns, _dir, _cache_dir) = seeded().await;
    let req = request(Some("section = 'warnings'"));

    service.query_with_cache_source(&ns, &req).await.unwrap();
    service
        .upsert(&ns, vec![row(4, 3, "warnings")])
        .await
        .expect("write");

    let after = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("after the write");
    assert_eq!(after.cache_source, QueryCacheSource::Backend);
    assert_eq!(after.result.results.len(), 3);
}

/// A predicate naming a column the namespace has never declared is the
/// caller's mistake and reaches them as a 400, not a 500.
#[tokio::test]
async fn a_predicate_over_an_undeclared_column_is_a_client_error() {
    let (service, ns, _dir, _cache_dir) = seeded().await;
    let err = service
        .query_with_cache_source(&ns, &request(Some("route = 'ORAL'")))
        .await
        .expect_err("route does not exist");
    match err {
        firnflow_core::FirnflowError::InvalidRequest(msg) => {
            assert!(msg.contains("filter"), "{msg}")
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}
