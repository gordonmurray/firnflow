//! Namespace service — combines the Lance backend, the foyer hybrid
//! cache, the opt-in semantic sidecar, and the bincode result
//! payload format into a single cache-aside read path with
//! invalidate-on-write.
//!
//! * [`NamespaceManager`] — the Lance backend
//! * [`NamespaceCache`] — foyer hybrid cache + generation counter
//! * [`SemanticCache`] — opt-in near-duplicate result reuse
//! * bincode-2 serde path — the cached result payload format
//!
//! The axum handlers own an `Arc<NamespaceService>` and call
//! straight into `upsert` / `query`.
//!
//! Every call records query/write duration histograms and an
//! `s3_requests_total` counter. The cache hit/miss counters live on
//! the cache itself — see [`NamespaceCache::try_get`].

use std::sync::Arc;
use std::time::Instant;

use bincode::config;
use serde::Serialize;

use crate::cache::{NamespaceCache, QueryHash, SemanticCache, SemanticLookup};
use crate::manager::{CompactResult, NamespaceManager, UpsertRow};
use crate::metrics::CoreMetrics;
use crate::query::{
    effective_semantic_threshold, validate_semantic_cache_request, QueryRequest, DEFAULT_NPROBES,
};
use crate::{FirnflowError, NamespaceId, QueryResultSet};

/// Where a query result came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryCacheSource {
    /// The query ran against the underlying Lance backend.
    Backend,
    /// The exact result cache served the query.
    ExactCache,
    /// The semantic cache sidecar reused a nearby result set.
    SemanticCache,
}

impl QueryCacheSource {
    /// Stable header value used by the API's debug cache-source signal.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backend => "backend",
            Self::ExactCache => "exact_cache",
            Self::SemanticCache => "semantic_cache",
        }
    }
}

/// Query results plus the cache/backend source used to produce them.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryOutcome {
    /// Ranked search results.
    pub result: QueryResultSet,
    /// Cache/backend source for observability and benchmarks.
    pub cache_source: QueryCacheSource,
}

/// Service facade over [`NamespaceManager`] + [`NamespaceCache`] +
/// [`SemanticCache`].
pub struct NamespaceService {
    manager: Arc<NamespaceManager>,
    cache: Arc<NamespaceCache>,
    semantic: Arc<SemanticCache>,
    metrics: Arc<CoreMetrics>,
}

impl NamespaceService {
    /// Construct a new service wrapping a manager, a cache, and a
    /// metrics handle that will be shared across every handler in
    /// the API. `cache` must already have been constructed with the
    /// same `metrics` so hit/miss counts land on the same registry.
    /// The opt-in semantic sidecar is built internally and bound to
    /// the cache's generation counter so the exact and semantic
    /// invalidation paths stay aligned.
    pub fn new(
        manager: Arc<NamespaceManager>,
        cache: Arc<NamespaceCache>,
        metrics: Arc<CoreMetrics>,
    ) -> Self {
        let semantic = Arc::new(SemanticCache::new(
            cache.generation_counter(),
            Arc::clone(&metrics),
        ));
        Self {
            manager,
            cache,
            semantic,
            metrics,
        }
    }

    /// Test-only constructor that lets the caller inject a sidecar
    /// with a smaller per-namespace capacity. Production callsites
    /// should keep using [`Self::new`] — the default cap is the v1
    /// production value.
    #[doc(hidden)]
    pub fn with_semantic_cache(
        manager: Arc<NamespaceManager>,
        cache: Arc<NamespaceCache>,
        semantic: Arc<SemanticCache>,
        metrics: Arc<CoreMetrics>,
    ) -> Self {
        Self {
            manager,
            cache,
            semantic,
            metrics,
        }
    }

    /// Borrow the semantic sidecar — for tests that want to peek at
    /// per-namespace entry counts without going through `/metrics`.
    #[doc(hidden)]
    pub fn semantic_cache(&self) -> &Arc<SemanticCache> {
        &self.semantic
    }

    /// Write path: append rows via the manager. The append advances
    /// the Lance table version, which is the cache generation, so the
    /// next read derives a new generation and every result cached
    /// against the pre-write version becomes unreachable — no explicit
    /// cache bump is needed. Because the version only moves on a
    /// successful commit, a failed append leaves the cache
    /// self-consistent (it keeps serving the pre-failure results). The
    /// semantic sidecar is cleared eagerly to free memory rather than
    /// wait for its lazy generation-mismatch drop on the next lookup.
    ///
    /// Records `s3_requests_total{operation="upsert"}` eagerly (one
    /// per call) and `write_duration_seconds` on return.
    pub async fn upsert(
        &self,
        ns: &NamespaceId,
        rows: Vec<UpsertRow>,
    ) -> Result<(), FirnflowError> {
        let start = Instant::now();
        self.metrics.record_s3_request(ns, "upsert");
        self.manager.upsert(ns, rows).await?;
        self.semantic.invalidate(ns);
        self.metrics.record_write(ns, start.elapsed().as_secs_f64());
        Ok(())
    }

    /// Delete every object under the namespace prefix.
    ///
    /// Removing the Lance table drops the namespace back to "no table"
    /// (generation 0 on the next read), so results cached against the
    /// deleted table become unreachable. Recreating the namespace under
    /// the same name is safe even though its Lance version restarts at
    /// 1: the cache generation folds in the manifest commit timestamp
    /// (see [`NamespaceManager::generation`](crate::NamespaceManager::generation)),
    /// so the recreated incarnation keys differently from the deleted
    /// one and cannot re-serve its cached bytes. The semantic sidecar is
    /// cleared eagerly. A failed delete leaves the cache serving the
    /// pre-delete entries, self-consistent with the data still sitting
    /// in object storage.
    ///
    /// Returns the number of objects the manager removed.
    pub async fn delete(&self, ns: &NamespaceId) -> Result<usize, FirnflowError> {
        let start = Instant::now();
        self.metrics.record_s3_request(ns, "delete");
        let count = self.manager.delete(ns).await?;
        self.semantic.invalidate(ns);
        self.metrics.record_write(ns, start.elapsed().as_secs_f64());
        Ok(count)
    }

    /// Read path: check the exact cache, optionally consult the
    /// semantic sidecar, and fall through to the manager on a miss,
    /// populating both layers with the serialised result on the way
    /// back.
    ///
    /// The exact-cache key is a deterministic hash of the bincode
    /// encoding of the request's *cacheable* fields — the
    /// `semantic_cache` control field is deliberately excluded so
    /// flipping the opt-in does not split otherwise-identical
    /// entries. The capture-once generation discipline lives inside
    /// [`NamespaceCache::try_get`] / [`NamespaceCache::populate_with_generation`];
    /// we don't reimplement it here.
    ///
    /// Records `query_duration_seconds{query_type="…"}` around the
    /// whole path and `s3_requests_total{operation="query"}` only
    /// when the backend actually runs (exact and semantic hits stay
    /// off the S3 counter, which is the entire point of the metric).
    pub async fn query(
        &self,
        ns: &NamespaceId,
        req: &QueryRequest,
    ) -> Result<QueryResultSet, FirnflowError> {
        Ok(self.query_with_cache_source(ns, req).await?.result)
    }

    /// Same read path as [`Self::query`], but returns the cache/backend
    /// source used for this request. Intended for API debug headers and
    /// benchmark harnesses; normal callers should keep using
    /// [`Self::query`].
    pub async fn query_with_cache_source(
        &self,
        ns: &NamespaceId,
        req: &QueryRequest,
    ) -> Result<QueryOutcome, FirnflowError> {
        let start = Instant::now();
        validate_semantic_cache_request(req)?;

        let query_hash = hash_query_for_cache(req)?;

        // Derive the cache generation from the persistent Lance table
        // version before consulting either layer. The in-process
        // generation counter resets to 0 on restart; the table version
        // does not, so seeding the counter from it makes a recovered
        // NVMe entry reachable only when the namespace has not changed
        // since the entry was stored. Both the exact cache and the
        // semantic sidecar (which shares the counter) key off this
        // value. Cheap on a warm handle — an in-memory manifest read.
        let generation = self.manager.generation(ns).await?;
        self.cache.set_generation(ns, generation);

        // 1. Exact cache — always consulted, opt-in or not.
        let (exact_hit, captured_generation) = self.cache.try_get(ns, query_hash).await;
        if let Some(bytes) = exact_hit {
            let decoded = decode_payload(&bytes)?;
            self.metrics
                .record_query(ns, classify_query_type(req), start.elapsed().as_secs_f64());
            return Ok(QueryOutcome {
                result: decoded,
                cache_source: QueryCacheSource::ExactCache,
            });
        }

        // 2. Semantic sidecar — only when opt-in and eligible.
        let semantic_opt = req.semantic_cache.as_ref().filter(|s| s.enabled);
        let nprobes_resolved = req.nprobes.unwrap_or(DEFAULT_NPROBES);
        let semantic_eligible = semantic_opt.is_some()
            && !req.vector.is_empty()
            && req.vectors.as_ref().is_none_or(|v| v.is_empty())
            && req.text.is_none();

        if semantic_opt.is_some() && !semantic_eligible {
            // This branch is reachable today only if validation
            // missed a corner; track it under a rejection counter
            // so the gap shows up rather than silently degrading
            // to backend traffic.
            self.metrics
                .record_semantic_cache_rejection(ns, "unsupported_query_shape");
        }

        if let Some(sem) = semantic_opt {
            if semantic_eligible {
                let threshold = effective_semantic_threshold(sem);
                match self
                    .semantic
                    .lookup(ns, &req.vector, req.k, nprobes_resolved, threshold)
                {
                    SemanticLookup::Hit { bytes, .. } => {
                        let decoded = decode_payload(&bytes)?;
                        self.metrics.record_semantic_cache_hit(ns);
                        self.metrics.record_query(
                            ns,
                            classify_query_type(req),
                            start.elapsed().as_secs_f64(),
                        );
                        return Ok(QueryOutcome {
                            result: decoded,
                            cache_source: QueryCacheSource::SemanticCache,
                        });
                    }
                    SemanticLookup::Miss => {
                        self.metrics.record_semantic_cache_miss(ns);
                    }
                    SemanticLookup::EmptyIndex => {
                        self.metrics
                            .record_semantic_cache_rejection(ns, "empty_index");
                    }
                }
            }
        }

        // 3. Backend — same metrics shape as before: one s3_request
        //    per cache-miss query, regardless of the semantic layer.
        self.metrics.record_s3_request(ns, "query");
        let result = self
            .manager
            .query(
                ns,
                req.vector.clone(),
                req.vectors.clone(),
                req.k,
                req.nprobes,
                req.text.clone(),
            )
            .await?;
        let bytes = encode_payload(&result)?;
        self.cache
            .populate_with_generation(ns, captured_generation, query_hash, bytes.clone());
        if semantic_eligible {
            self.semantic.insert(
                ns,
                captured_generation,
                req.vector.clone(),
                req.k,
                nprobes_resolved,
                bytes.clone(),
            );
        }

        self.metrics
            .record_query(ns, classify_query_type(req), start.elapsed().as_secs_f64());
        Ok(QueryOutcome {
            result,
            cache_source: QueryCacheSource::Backend,
        })
    }

    /// Build an IVF_PQ index on the namespace's vector column.
    ///
    /// Records `firnflow_index_build_duration_seconds{namespace, kind}`
    /// on completion — the "Index Tax" metric.
    ///
    /// Building an index is a Lance commit, so it advances the table
    /// version and the next read derives a new generation: results
    /// cached before the build become unreachable. This is a behaviour
    /// change from the old generation-counter design, which left the
    /// cache untouched on index builds. It is the safer default —
    /// post-build queries run against the new index instead of
    /// replaying a pre-index cached result — at the cost of dropping
    /// the warm cache after an infrequent, operator-triggered build.
    pub async fn create_index(
        &self,
        ns: &NamespaceId,
        num_partitions: Option<u32>,
        num_sub_vectors: Option<u32>,
        num_bits: Option<u32>,
    ) -> Result<(), FirnflowError> {
        let start = Instant::now();
        self.metrics.record_s3_request(ns, "index");
        self.manager
            .create_index(ns, num_partitions, num_sub_vectors, num_bits)
            .await?;
        self.metrics
            .record_index_build(ns, "ivf_pq", start.elapsed().as_secs_f64());
        Ok(())
    }

    /// Build a BM25 full-text search index on the namespace's `text`
    /// column.
    pub async fn create_fts_index(&self, ns: &NamespaceId) -> Result<(), FirnflowError> {
        let start = Instant::now();
        self.metrics.record_s3_request(ns, "fts_index");
        self.manager.create_fts_index(ns).await?;
        self.metrics
            .record_index_build(ns, "fts", start.elapsed().as_secs_f64());
        Ok(())
    }

    /// Build a BTree scalar index on `column` (v1: `_ingested_at`
    /// only). Records `firnflow_index_build_duration_seconds{kind="scalar"}`
    /// on completion.
    ///
    /// Like the other index builders, this is a Lance commit, so it
    /// advances the table version and the next read derives a new
    /// generation — the warm cache is dropped even though the rows
    /// themselves are unchanged. See [`create_index`](Self::create_index).
    pub async fn create_scalar_index(
        &self,
        ns: &NamespaceId,
        column: &str,
    ) -> Result<(), FirnflowError> {
        let start = Instant::now();
        self.metrics.record_s3_request(ns, "scalar_index");
        self.manager.create_scalar_index(ns, column).await?;
        self.metrics
            .record_index_build(ns, "scalar", start.elapsed().as_secs_f64());
        Ok(())
    }

    /// Compact the namespace's data files.
    ///
    /// Compaction is a Lance commit, so it advances the table version
    /// and the next read derives a new generation — results cached
    /// against the pre-compaction version fall out of reach without an
    /// explicit bump. The semantic sidecar is cleared eagerly.
    ///
    /// Records `firnflow_compaction_duration_seconds{namespace}` on
    /// completion.
    pub async fn compact(&self, ns: &NamespaceId) -> Result<CompactResult, FirnflowError> {
        let start = Instant::now();
        self.metrics.record_s3_request(ns, "compact");
        let result = self.manager.compact(ns).await?;
        self.semantic.invalidate(ns);
        self.metrics
            .record_compaction(ns, start.elapsed().as_secs_f64());
        Ok(result)
    }
}

/// Hash the cacheable fields of `req` for the exact-cache key.
///
/// The `semantic_cache` control field is intentionally excluded —
/// toggling opt-in semantic caching must not split otherwise
/// identical cache entries. Bincode-2 over a tuple-view of the
/// underlying fields gives a deterministic, allocation-light
/// encoding suitable for hashing.
fn hash_query_for_cache(req: &QueryRequest) -> Result<QueryHash, FirnflowError> {
    #[derive(Serialize)]
    struct Canonical<'a> {
        vector: &'a Vec<f32>,
        vectors: &'a Option<Vec<Vec<f32>>>,
        k: usize,
        nprobes: Option<usize>,
        text: &'a Option<String>,
    }
    let canonical = Canonical {
        vector: &req.vector,
        vectors: &req.vectors,
        k: req.k,
        nprobes: req.nprobes,
        text: &req.text,
    };
    let bytes = bincode::serde::encode_to_vec(&canonical, config::standard())
        .map_err(|e| FirnflowError::Backend(format!("encode query: {e}")))?;
    Ok(QueryHash::of(&bytes))
}

fn encode_payload(result: &QueryResultSet) -> Result<Vec<u8>, FirnflowError> {
    bincode::serde::encode_to_vec(result, config::standard())
        .map_err(|e| FirnflowError::Backend(format!("encode result: {e}")))
}

fn decode_payload(bytes: &[u8]) -> Result<QueryResultSet, FirnflowError> {
    let (decoded, _): (QueryResultSet, usize) =
        bincode::serde::decode_from_slice(bytes, config::standard())
            .map_err(|e| FirnflowError::Backend(format!("decode result: {e}")))?;
    Ok(decoded)
}

/// Compute the `query_type` label exactly the same way the previous
/// implementation did. Lifted out so exact-cache, semantic-cache,
/// and backend paths can all attribute the same label.
fn classify_query_type(req: &QueryRequest) -> &'static str {
    let has_single = !req.vector.is_empty();
    let has_multi = req.vectors.as_ref().map(|v| !v.is_empty()).unwrap_or(false);
    let has_text = req.text.is_some();
    match (has_single, has_multi, has_text) {
        (true, _, true) | (_, true, true) => "hybrid",
        (true, _, false) => "vector",
        (_, true, false) => "multivector",
        (false, false, true) => "fts",
        (false, false, false) => "vector",
    }
}
