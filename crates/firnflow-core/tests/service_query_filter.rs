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
        exact: false,
        refine_factor: None,
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

/// A filtered query whose predicate is the caller's fault must map to
/// `InvalidRequest` (400), not `Backend` (500). Covers a spread of predicate
/// failure shapes: a SQL syntax error, an unknown column, a type mismatch, an
/// unknown function, and an unsupported operator. The last two are the cases a
/// narrower message-matching classifier mislabeled as 500, so they pin the
/// broad classification. Genuine backend failures on a filtered query still map
/// to `Backend`; that path is not reachable from local storage, so it is
/// covered by the classifier's default arm rather than a test here.
#[tokio::test]
async fn filtered_predicate_errors_map_to_invalid_request() {
    let (service, ns, _dir, _cache_dir) = local_service().await;
    for bad in [
        "id =",               // SQL parse error
        "nope > 1",           // unknown column
        "text > 1",           // type mismatch
        "no_such_fn(id) = 1", // unknown function
        "id @> 1",            // unsupported operator
    ] {
        let req = request(Some(bad));
        let err = service
            .query_with_cache_source(&ns, &req)
            .await
            .expect_err("malformed predicate should error");
        match err {
            FirnflowError::InvalidRequest(msg) => {
                assert!(msg.contains("filter"), "predicate {bad:?}: {msg}")
            }
            other => panic!("predicate {bad:?}: expected InvalidRequest, got {other:?}"),
        }
    }
}

/// A predicate that parses but reaches an unimplemented path in Lance's SQL
/// planner (national or bit string literals) panics inside `execute()`. The
/// filtered path must catch that and report a 400, not unwind the request.
#[tokio::test]
async fn filtered_unsupported_syntax_maps_to_invalid_request() {
    let (service, ns, _dir, _cache_dir) = local_service().await;
    for bad in ["text = N'x'", "text = B'1'"] {
        let req = request(Some(bad));
        let err = service
            .query_with_cache_source(&ns, &req)
            .await
            .expect_err("unsupported predicate syntax should error, not panic");
        match err {
            FirnflowError::InvalidRequest(msg) => {
                assert!(msg.contains("filter"), "predicate {bad:?}: {msg}")
            }
            other => panic!("predicate {bad:?}: expected InvalidRequest, got {other:?}"),
        }
    }
}

/// A filtered full-text query on a namespace with no inverted index fails with
/// `InvalidInput`. The filter-error classifier is deliberately broad, so this
/// maps to a 400 rather than a 500. That is an accepted trade (a missing-index
/// 400 reads as "build the index"); the alternative, message-matching to force
/// it to 500, would mislabel genuine bad predicates as backend errors. This
/// test pins the chosen behaviour so a future change to it is a conscious one.
#[tokio::test]
async fn filtered_fts_without_index_maps_to_invalid_request() {
    let (service, ns, _dir, _cache_dir) = local_service().await;
    let req = QueryRequest {
        vector: Vec::new(),
        vectors: None,
        k: 10,
        nprobes: None,
        text: Some("anything".into()),
        filter: Some("id > 1".into()),
        include_vector: false,
        semantic_cache: None,
        exact: false,
        refine_factor: None,
    };
    let err = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect_err("fts query without an index should error");
    assert!(
        matches!(err, FirnflowError::InvalidRequest(_)),
        "missing FTS index on a filtered query maps to InvalidRequest, got {err:?}"
    );
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

/// A predicate whose meaning can change between two identical
/// requests must never be served from the exact cache (#89).
///
/// Both repeats reporting `Backend` is a complete proof of the
/// mechanism in one assertion: had the first response been written
/// to the cache, the second would have come back as `ExactCache`. So
/// this pins the read bypass and the write bypass together.
///
/// `now()` is `Stable` rather than `Volatile` in the query planner —
/// fixed within one query, free to move between them — which is
/// exactly the level a check for "is it volatile" would miss, so it
/// leads the list here. The bare `CURRENT_TIMESTAMP` spelling is
/// included because it is not a function call in the parsed SQL at
/// all; it becomes `now()` only after the planner rewrites it.
#[tokio::test]
async fn volatile_filters_never_serve_from_the_exact_cache() {
    let (service, ns, _dir, _cache_dir) = local_service().await;

    for filter in [
        "_ingested_at < now()",
        "_ingested_at < CURRENT_TIMESTAMP",
        "_ingested_at < current_date",
        "random() < 1.1",
        "id > 0 AND random() < 1.1",
    ] {
        let req = request(Some(filter));

        let first = service
            .query_with_cache_source(&ns, &req)
            .await
            .unwrap_or_else(|e| panic!("{filter:?} first query: {e}"));
        assert_eq!(
            first.cache_source,
            QueryCacheSource::Backend,
            "{filter:?} first query should reach the backend"
        );

        let second = service
            .query_with_cache_source(&ns, &req)
            .await
            .unwrap_or_else(|e| panic!("{filter:?} second query: {e}"));
        assert_eq!(
            second.cache_source,
            QueryCacheSource::Backend,
            "{filter:?} repeat must re-run rather than replay a cached result"
        );
    }
}

/// Bypassing the cache must not change what a volatile filter
/// returns — the rows are still filtered, just never cached. All
/// three seed rows carry an `_ingested_at` in the past, so a `<
/// now()` cutoff selects all of them.
#[tokio::test]
async fn volatile_filtered_queries_still_return_filtered_rows() {
    let (service, ns, _dir, _cache_dir) = local_service().await;

    let all = service
        .query_with_cache_source(&ns, &request(Some("_ingested_at < now()")))
        .await
        .expect("now() filter");
    let mut ids: Vec<u64> = all.result.results.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 3]);

    let narrowed = service
        .query_with_cache_source(&ns, &request(Some("id > 1 AND _ingested_at < now()")))
        .await
        .expect("combined filter");
    let mut ids: Vec<u64> = narrowed.result.results.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![2, 3]);
}

/// The bypass is scoped to the predicates that need it. A stable
/// filter keeps the cached fast path, and a column that merely
/// happens to be named `now` is stable — the trap a text search for
/// `now(` would fall into. Guards against a fix that quietly
/// disables caching for every filtered query.
#[tokio::test]
async fn stable_filters_keep_the_cached_fast_path() {
    let (service, ns, _dir, _cache_dir) = local_service().await;

    let req = request(Some("id > 1"));
    let first = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("stable filter #1");
    assert_eq!(first.cache_source, QueryCacheSource::Backend);

    let second = service
        .query_with_cache_source(&ns, &req)
        .await
        .expect("stable filter #2");
    assert_eq!(second.cache_source, QueryCacheSource::ExactCache);
    assert_eq!(second.result, first.result);
}

/// A filtered query against a namespace that has never been written
/// to must not populate the exact cache.
///
/// The predicate cannot be analysed without a schema, and treating
/// it as cacheable would lose the guarantee to a first-write race:
/// the cacheability check and the cache population that follows are
/// not atomic, so a concurrent first write can land between them and
/// leave the query returning rows a volatile predicate then caches.
/// Nothing is given up by refusing — the result set is empty.
#[tokio::test]
async fn filtered_query_on_unwritten_namespace_does_not_cache() {
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
    let ns = NamespaceId::new("never-written").unwrap();

    let req = request(Some("id > 1"));
    for attempt in 1..=2 {
        let out = service
            .query_with_cache_source(&ns, &req)
            .await
            .unwrap_or_else(|e| panic!("attempt {attempt}: {e}"));
        assert!(out.result.results.is_empty(), "attempt {attempt}");
        assert_eq!(
            out.cache_source,
            QueryCacheSource::Backend,
            "attempt {attempt} must not be served from the cache"
        );
    }
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
