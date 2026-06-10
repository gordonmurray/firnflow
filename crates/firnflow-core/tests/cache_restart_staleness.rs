//! Restart-staleness repro for the per-namespace generation counter.
//!
//! The cache stays correct under writes by bumping an in-process
//! generation counter (see the cache-invalidation decision record) so
//! previously cached entries become unreachable by key. That counter
//! lives only in memory (a `DashMap`) and resets to 0 when the process
//! restarts. The foyer NVMe tier, however, persists across restarts and
//! recovers its entries on reopen — foyer's disk engine defaults to
//! `RecoverMode::Quiet`, which restores every readable entry.
//!
//! So after a restart two things are true at once: the generation
//! sequence replays from 0, and the recovered NVMe entries still carry
//! their pre-restart `(namespace, generation, query)` keys. A query whose
//! replayed generation lands on a recovered key is served the pre-restart
//! bytes — a stale read across the write that bumped the generation,
//! which violates the invalidation invariant.
//!
//! This test simulates a restart by closing one `NamespaceCache` and
//! building a second over the same NVMe directory. It asserts the
//! correct behaviour: the post-restart lookup must miss. While the bug is
//! live that assertion fails, so the test carries `#[ignore]`. Removing
//! the ignore is the acceptance test for the fix.

use firnflow_core::cache::{NamespaceCache, QueryHash};
use firnflow_core::metrics::test_metrics;
use firnflow_core::{FirnflowError, NamespaceId};

const MEM_BYTES: usize = 16 * 1024 * 1024;
const DISK_BYTES: usize = 64 * 1024 * 1024;

#[tokio::test]
#[ignore = "demonstrates the live stale-read-after-restart bug; un-ignore once the generation survives restart"]
async fn restart_does_not_serve_pre_restart_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let nvme = tmp.path();

    let ns = NamespaceId::new("acme").unwrap();
    let query = QueryHash::of(b"select top 10 where x > 5");

    // --- First process lifetime ---
    let cache = NamespaceCache::new(MEM_BYTES, nvme, DISK_BYTES, test_metrics())
        .await
        .expect("build cache");

    // A read at generation 0 caches the pre-write result.
    let r1 = cache
        .get_or_populate(&ns, query, || async {
            Ok::<_, FirnflowError>(b"v1".to_vec())
        })
        .await
        .unwrap();
    assert_eq!(r1, b"v1");

    // A write lands and invalidates the namespace (generation -> 1). The
    // authoritative data is now "v2"; the gen-0 "v1" entry is stale.
    cache.invalidate(&ns);

    // In-process the stale gen-0 entry is already unreachable: the lookup
    // keys at generation 1 and misses.
    let (hit, gen) = cache.try_get(&ns, query).await;
    assert_eq!(gen, 1, "the write should have advanced the generation to 1");
    assert!(hit.is_none(), "post-write lookup must miss in-process");

    // Flush the NVMe writer and shut down, as a clean restart would.
    cache.close().await.expect("close cache");
    drop(cache);

    // --- Second process lifetime: same NVMe directory, fresh counter ---
    let restarted = NamespaceCache::new(MEM_BYTES, nvme, DISK_BYTES, test_metrics())
        .await
        .expect("rebuild cache over the same NVMe dir");

    // The generation counter reset to 0, so a repeat of the same query
    // keys at generation 0 — exactly the key the recovered "v1" entry
    // still carries on the NVMe tier.
    let (hit, gen) = restarted.try_get(&ns, query).await;
    assert_eq!(
        gen, 0,
        "a fresh process starts every namespace at generation 0"
    );
    assert!(
        hit.is_none(),
        "after a restart the cache must not serve pre-restart entries; got a recovered hit, \
         which is a stale read across the write that bumped the generation"
    );
}
