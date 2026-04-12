//! Namespace service — the layer where the three spike results
//! finally meet.
//!
//! Combines:
//!
//! * [`NamespaceManager`] — the Lance backend (spike-2)
//! * [`NamespaceCache`] — foyer hybrid cache + generation counter
//!   (spike-1)
//! * bincode-2 serde path — the cached result payload format
//!   (spike-3, ADR-002)
//!
//! into a single cache-aside read path with invalidate-on-write.
//! The axum handlers in slice 1c own an `Arc<NamespaceService>`
//! and call straight into `upsert` / `query`.
//!
//! Instrumentation (slice 2a): every call records query/write
//! duration histograms and an `s3_requests_total` counter. The
//! cache hit/miss counters live on the cache itself — see
//! [`NamespaceCache::get_or_populate`].

use std::sync::Arc;
use std::time::Instant;

use bincode::config;

use crate::cache::{NamespaceCache, QueryHash};
use crate::manager::NamespaceManager;
use crate::metrics::CoreMetrics;
use crate::query::QueryRequest;
use crate::{FirnflowError, NamespaceId, QueryResultSet};

/// Service facade over [`NamespaceManager`] + [`NamespaceCache`].
pub struct NamespaceService {
    manager: Arc<NamespaceManager>,
    cache: Arc<NamespaceCache>,
    metrics: Arc<CoreMetrics>,
}

impl NamespaceService {
    /// Construct a new service wrapping a manager, a cache, and a
    /// metrics handle that will be shared across every handler in
    /// the API. `cache` must already have been constructed with the
    /// same `metrics` so hit/miss counts land on the same registry.
    pub fn new(
        manager: Arc<NamespaceManager>,
        cache: Arc<NamespaceCache>,
        metrics: Arc<CoreMetrics>,
    ) -> Self {
        Self {
            manager,
            cache,
            metrics,
        }
    }

    /// Write path: append rows via the manager, then invalidate
    /// every cached query result for this namespace. Invalidation
    /// happens *after* the write succeeds so that a failed append
    /// leaves the cache in a self-consistent state (worst case the
    /// cache keeps returning pre-failure results until the next
    /// successful write).
    ///
    /// Records `s3_requests_total{operation="upsert"}` eagerly (one
    /// per call) and `write_duration_seconds` on return.
    pub async fn upsert(
        &self,
        ns: &NamespaceId,
        rows: Vec<(u64, Vec<f32>)>,
    ) -> Result<(), FirnflowError> {
        let start = Instant::now();
        self.metrics.record_s3_request(ns, "upsert");
        self.manager.upsert(ns, rows).await?;
        self.cache.invalidate(ns);
        self.metrics.record_write(ns, start.elapsed().as_secs_f64());
        Ok(())
    }

    /// Delete every object under the namespace prefix and invalidate
    /// its cache entries.
    ///
    /// Same after-success ordering as [`upsert`](Self::upsert): the
    /// manager-side S3 cleanup runs first, and only if it succeeds
    /// do we bump the generation counter. A failed delete leaves
    /// the cache serving the pre-delete entries, which is
    /// self-consistent with the data still sitting in S3.
    ///
    /// Returns the number of S3 objects the manager removed.
    pub async fn delete(&self, ns: &NamespaceId) -> Result<usize, FirnflowError> {
        let start = Instant::now();
        self.metrics.record_s3_request(ns, "delete");
        let count = self.manager.delete(ns).await?;
        self.cache.invalidate(ns);
        self.metrics.record_write(ns, start.elapsed().as_secs_f64());
        Ok(count)
    }

    /// Read path: check the cache, fall through to the manager on a
    /// miss, and populate the cache with the serialised result on
    /// the way back.
    ///
    /// The cache key is a deterministic hash of the bincode-encoded
    /// request, so two equivalent `QueryRequest` values hit the
    /// same entry. The capture-once generation discipline lives
    /// inside [`NamespaceCache::get_or_populate`]; we do not need
    /// to reimplement it here.
    ///
    /// Records `query_duration_seconds{query_type="vector"}` around
    /// the whole cache-aside path and
    /// `s3_requests_total{operation="query"}` inside the populate
    /// closure, so cache hits do not count towards the S3 metric —
    /// that gap is the signal the metric exists for.
    pub async fn query(
        &self,
        ns: &NamespaceId,
        req: &QueryRequest,
    ) -> Result<QueryResultSet, FirnflowError> {
        let start = Instant::now();
        let request_bytes = bincode::serde::encode_to_vec(req, config::standard())
            .map_err(|e| FirnflowError::Backend(format!("encode query: {e}")))?;
        let query_hash = QueryHash::of(&request_bytes);

        let manager = Arc::clone(&self.manager);
        let metrics_for_populate = Arc::clone(&self.metrics);
        let ns_owned = ns.clone();
        let req_owned = req.clone();

        let payload = self
            .cache
            .get_or_populate(ns, query_hash, move || async move {
                metrics_for_populate.record_s3_request(&ns_owned, "query");
                let result = manager
                    .query(&ns_owned, req_owned.vector, req_owned.k)
                    .await?;
                bincode::serde::encode_to_vec(&result, config::standard())
                    .map_err(|e| FirnflowError::Backend(format!("encode result: {e}")))
            })
            .await?;

        let (decoded, _): (QueryResultSet, usize) =
            bincode::serde::decode_from_slice(&payload, config::standard())
                .map_err(|e| FirnflowError::Backend(format!("decode result: {e}")))?;

        self.metrics
            .record_query(ns, "vector", start.elapsed().as_secs_f64());
        Ok(decoded)
    }
}
