//! Spike-1 correctness test.
//!
//! Exercises the generation-counter invalidation strategy end to end:
//!
//! 1. Populate the cache with a query result.
//! 2. Change the simulated "world" *without* invalidating — the cache
//!    should still return the stale value. This confirms we're
//!    actually testing the invalidation path, not a trivial always-miss.
//! 3. Invalidate the namespace.
//! 4. Re-query — the cache should miss and return the fresh value.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use firnflow_core::cache::{NamespaceCache, QueryHash};
use firnflow_core::metrics::test_metrics;
use firnflow_core::{FirnflowError, NamespaceId};

#[tokio::test]
async fn invalidation_returns_fresh_results_after_write() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = NamespaceCache::new(
        16 * 1024 * 1024,
        tmp.path(),
        64 * 1024 * 1024,
        test_metrics(),
    )
    .await
    .expect("build cache");

    let ns = NamespaceId::new("acme").unwrap();
    let query = QueryHash::of(b"select top 10 where x > 5");
    let world = Arc::new(Mutex::new(b"v1".to_vec()));

    // Miss → populate with v1.
    let world1 = Arc::clone(&world);
    let r1 = cache
        .get_or_populate(&ns, query, move || async move {
            Ok::<_, FirnflowError>(world1.lock().unwrap().clone())
        })
        .await
        .unwrap();
    assert_eq!(r1, b"v1");

    // Simulated write: world changes but no invalidation yet.
    *world.lock().unwrap() = b"v2".to_vec();

    // Hit → still the cached v1. This is the critical assertion: if
    // this returns v2, the cache is a no-op and we're not exercising
    // the invalidation path at all.
    let world2 = Arc::clone(&world);
    let r2 = cache
        .get_or_populate(&ns, query, move || async move {
            Ok::<_, FirnflowError>(world2.lock().unwrap().clone())
        })
        .await
        .unwrap();
    assert_eq!(
        r2, b"v1",
        "without invalidation the cache must return stale v1"
    );

    // Invalidate.
    cache.invalidate(&ns);

    // Miss → repopulate with v2.
    let world3 = Arc::clone(&world);
    let r3 = cache
        .get_or_populate(&ns, query, move || async move {
            Ok::<_, FirnflowError>(world3.lock().unwrap().clone())
        })
        .await
        .unwrap();
    assert_eq!(
        r3, b"v2",
        "after invalidation the cache must repopulate from the fresh world"
    );
}

#[tokio::test]
async fn invalidation_is_per_namespace() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = NamespaceCache::new(
        16 * 1024 * 1024,
        tmp.path(),
        64 * 1024 * 1024,
        test_metrics(),
    )
    .await
    .expect("build cache");

    let alpha = NamespaceId::new("alpha").unwrap();
    let beta = NamespaceId::new("beta").unwrap();
    let query = QueryHash::of(b"shared-query");
    let calls = Arc::new(AtomicU64::new(0));

    // Populate both namespaces. Each first touch must miss.
    for ns in [&alpha, &beta] {
        let calls = Arc::clone(&calls);
        cache
            .get_or_populate(ns, query, move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, FirnflowError>(b"payload".to_vec())
            })
            .await
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // Invalidate only alpha.
    cache.invalidate(&alpha);

    // alpha must now miss — its generation has been bumped.
    let calls_a = Arc::clone(&calls);
    cache
        .get_or_populate(&alpha, query, move || async move {
            calls_a.fetch_add(1, Ordering::SeqCst);
            Ok::<_, FirnflowError>(b"payload".to_vec())
        })
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "alpha should miss after its own invalidation"
    );

    // beta must still hit — invalidating alpha cannot touch beta.
    let calls_b = Arc::clone(&calls);
    cache
        .get_or_populate(&beta, query, move || async move {
            calls_b.fetch_add(1, Ordering::SeqCst);
            Ok::<_, FirnflowError>(b"payload".to_vec())
        })
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "beta must still hit — cross-namespace isolation must hold"
    );
}
