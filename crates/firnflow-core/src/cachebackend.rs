//! No-op `lance_core::cache::CacheBackend` for reachability probes.
//!
//! Used to confirm that a custom backend installed through
//! [`lancedb::ConnectBuilder::session(...)`] actually receives
//! calls from Lance's index-cache code path on a real query. The
//! implementation deliberately:
//!
//! - Answers every `get` with `None` (cold miss).
//! - Runs the loader on every `get_or_insert`, returns the
//!   computed entry, and reports `was_cached = false` (so Lance
//!   keeps making forward progress on cold opens).
//! - Discards every `insert` payload.
//! - Reports zero entries / zero bytes at all times.
//!
//! Every method also bumps an atomic counter on the
//! `Arc<CacheBackendCounters>` the backend was constructed with,
//! so callers can snapshot the counts before and after a query
//! to confirm the cache layer is on the hot path.
//!
//! This module is part of the index-cache prototype (issue #51).
//! It is **not** a production cache layer — the foyer-backed
//! implementation that would actually warm Lance's index state
//! is gated on a separate decision later in the prototype plan.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::Future;
use lance_core::cache::{CacheBackend, CacheCodec, CacheEntry, InternalCacheKey};
use lance_core::Result as LanceResult;

/// Atomic counters incremented by every call into a
/// [`NoOpCacheBackend`]. Wrap in an `Arc` and share the same
/// handle between the backend and the observer (typically a
/// probe test).
#[derive(Debug, Default)]
pub struct CacheBackendCounters {
    /// Total `get` calls (lookups). Always reports a miss.
    pub get: AtomicU64,
    /// Total `insert` calls (store). Payloads discarded.
    pub insert: AtomicU64,
    /// Total `get_or_insert` calls. Loader always runs.
    pub get_or_insert: AtomicU64,
    /// Total `invalidate_prefix` calls.
    pub invalidate_prefix: AtomicU64,
    /// Total `clear` calls.
    pub clear: AtomicU64,
}

impl CacheBackendCounters {
    /// Snapshot all counters as plain `u64` values.
    ///
    /// Loads each counter independently with
    /// [`Ordering::Relaxed`]; the snapshot is **not** atomic
    /// across fields and concurrent writes may produce a slightly
    /// inconsistent view. Adequate for the "did any traffic
    /// arrive at all?" question the probe asks.
    pub fn snapshot(&self) -> CacheBackendCountersSnapshot {
        CacheBackendCountersSnapshot {
            get: self.get.load(Ordering::Relaxed),
            insert: self.insert.load(Ordering::Relaxed),
            get_or_insert: self.get_or_insert.load(Ordering::Relaxed),
            invalidate_prefix: self.invalidate_prefix.load(Ordering::Relaxed),
            clear: self.clear.load(Ordering::Relaxed),
        }
    }
}

/// Plain-`u64` snapshot of [`CacheBackendCounters`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheBackendCountersSnapshot {
    /// Total `get` calls observed at snapshot time.
    pub get: u64,
    /// Total `insert` calls observed at snapshot time.
    pub insert: u64,
    /// Total `get_or_insert` calls observed at snapshot time.
    pub get_or_insert: u64,
    /// Total `invalidate_prefix` calls observed at snapshot time.
    pub invalidate_prefix: u64,
    /// Total `clear` calls observed at snapshot time.
    pub clear: u64,
}

/// A `lance_core::cache::CacheBackend` that stores nothing and
/// counts every call.
///
/// See the module-level docs for the contract — the goal is to
/// keep Lance's cache hot path correct (every lookup looks like a
/// cold miss) while making the counter changes the only
/// observable side effect.
#[derive(Debug)]
pub struct NoOpCacheBackend {
    counters: Arc<CacheBackendCounters>,
}

impl NoOpCacheBackend {
    /// Construct a new no-op backend that records every call
    /// into `counters`. Clone the `Arc` and hold the clone
    /// externally to read counts after a query.
    pub fn new(counters: Arc<CacheBackendCounters>) -> Self {
        Self { counters }
    }
}

#[async_trait]
impl CacheBackend for NoOpCacheBackend {
    async fn get(&self, key: &InternalCacheKey, _codec: Option<CacheCodec>) -> Option<CacheEntry> {
        self.counters.get.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            target: "firnflow_core::cachebackend",
            prefix = key.prefix(),
            key = key.key(),
            type_name = key.type_name(),
            "cachebackend get (miss)",
        );
        None
    }

    async fn insert(
        &self,
        key: &InternalCacheKey,
        _entry: CacheEntry,
        size_bytes: usize,
        _codec: Option<CacheCodec>,
    ) {
        self.counters.insert.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            target: "firnflow_core::cachebackend",
            prefix = key.prefix(),
            key = key.key(),
            type_name = key.type_name(),
            size_bytes,
            "cachebackend insert (discarded)",
        );
    }

    async fn get_or_insert<'a>(
        &self,
        key: &InternalCacheKey,
        loader: Pin<Box<dyn Future<Output = LanceResult<(CacheEntry, usize)>> + Send + 'a>>,
        _codec: Option<CacheCodec>,
    ) -> LanceResult<(CacheEntry, bool)> {
        self.counters.get_or_insert.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            target: "firnflow_core::cachebackend",
            prefix = key.prefix(),
            key = key.key(),
            type_name = key.type_name(),
            "cachebackend get_or_insert (loader runs)",
        );
        let (entry, _size) = loader.await?;
        Ok((entry, false))
    }

    async fn invalidate_prefix(&self, prefix: &str) {
        self.counters
            .invalidate_prefix
            .fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            target: "firnflow_core::cachebackend",
            prefix,
            "cachebackend invalidate_prefix",
        );
    }

    async fn clear(&self) {
        self.counters.clear.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            target: "firnflow_core::cachebackend",
            "cachebackend clear",
        );
    }

    async fn num_entries(&self) -> usize {
        0
    }

    async fn size_bytes(&self) -> usize {
        0
    }
}
