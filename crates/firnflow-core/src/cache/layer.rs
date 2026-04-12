//! The combined cache layer: foyer `HybridCache` + generation counter.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use foyer::{BlockEngineConfig, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder};

use crate::cache::invalidation::GenerationCounter;
use crate::cache::key::{CacheKey, QueryHash};
use crate::metrics::CoreMetrics;
use crate::{FirnflowError, NamespaceId};

/// Namespace-aware cache layer.
///
/// Wraps a single foyer `HybridCache<CacheKey, Vec<u8>>` shared across
/// namespaces and a [`GenerationCounter`] that drives invalidation.
/// Records cache hits and misses directly against the shared
/// [`CoreMetrics`] it was constructed with.
pub struct NamespaceCache {
    cache: HybridCache<CacheKey, Vec<u8>>,
    generations: Arc<GenerationCounter>,
    metrics: Arc<CoreMetrics>,
}

impl NamespaceCache {
    /// Build a new cache backed by a foyer HybridCache.
    ///
    /// * `memory_bytes` – RAM-tier capacity, in bytes.
    /// * `nvme_path`    – directory that will host the NVMe-tier block file.
    /// * `nvme_bytes`   – NVMe-tier capacity, in bytes.
    /// * `metrics`      – shared `CoreMetrics` the cache increments
    ///   hit/miss counters against.
    pub async fn new(
        memory_bytes: usize,
        nvme_path: &Path,
        nvme_bytes: usize,
        metrics: Arc<CoreMetrics>,
    ) -> Result<Self, FirnflowError> {
        let device = FsDeviceBuilder::new(nvme_path)
            .with_capacity(nvme_bytes)
            .build()
            .map_err(|e| FirnflowError::Cache(format!("device build: {e}")))?;

        let cache = HybridCacheBuilder::new()
            .memory(memory_bytes)
            .storage()
            .with_engine_config(BlockEngineConfig::new(device))
            .build()
            .await
            .map_err(|e| FirnflowError::Cache(format!("hybrid build: {e}")))?;

        Ok(Self {
            cache,
            generations: Arc::new(GenerationCounter::new()),
            metrics,
        })
    }

    /// Look up a cached result or populate it via the supplied future.
    ///
    /// The namespace generation is captured once at call entry and used
    /// for both the lookup and the insert. If a concurrent writer bumps
    /// the generation between capture and insert, the resulting entry
    /// is labelled with the pre-write generation and becomes unreachable
    /// — wasted work, but never served as a stale hit.
    ///
    /// Records `cache_hits_total` on a hit or `cache_misses_total` on a
    /// miss. The miss is recorded *before* the populate closure runs, so
    /// a closure that errors is still counted as an attempted miss.
    pub async fn get_or_populate<F, Fut>(
        &self,
        ns: &NamespaceId,
        query: QueryHash,
        populate: F,
    ) -> Result<Vec<u8>, FirnflowError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<u8>, FirnflowError>>,
    {
        let generation = self.generations.current(ns);
        let key = CacheKey {
            namespace: ns.clone(),
            generation,
            query,
        };

        if let Ok(Some(entry)) = self.cache.get(&key).await {
            self.metrics.record_cache_hit(ns);
            return Ok(entry.value().clone());
        }

        self.metrics.record_cache_miss(ns);
        let value = populate().await?;
        self.cache.insert(key, value.clone());
        Ok(value)
    }

    /// Invalidate every cache entry for a namespace.
    ///
    /// O(1): increments the namespace generation counter. Previously
    /// cached entries remain in foyer's underlying store until LFU/LRU
    /// reclaims them, but are no longer reachable by key.
    pub fn invalidate(&self, ns: &NamespaceId) -> u64 {
        self.generations.bump(ns)
    }

    /// The current generation counter for a namespace.
    pub fn generation(&self, ns: &NamespaceId) -> u64 {
        self.generations.current(ns)
    }
}
