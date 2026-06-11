//! Opt-in semantic sidecar for the exact result cache.
//!
//! Stores recent (query vector, k, nprobes, include_vector, result
//! bytes) tuples per namespace generation. When the exact cache
//! misses and the caller has opted in, the read path scans this
//! sidecar for a cached query whose vector is within the caller's
//! cosine threshold and whose surrounding shape (`k`, `nprobes`,
//! `include_vector`) matches the new request.
//!
//! The sidecar is deliberately small, in-memory only, and bounded:
//! it is an optimisation layer, not the main corpus index. A linear
//! scan over a few hundred candidates per namespace is acceptable
//! at this scale; an in-memory ANN structure is future work and
//! would only earn its keep once per-namespace candidate counts
//! grow well past the v1 cap.
//!
//! ## Invalidation
//!
//! Each namespace's entry list is stamped with the generation at
//! insert time. The generation is the Lance table version, which the
//! read path writes into the shared [`GenerationCounter`] (via
//! `NamespaceCache::set_generation`) before every lookup. Two routes
//! drop stale entries:
//!
//! 1. **Eager**: [`NamespaceService`](crate::NamespaceService) calls
//!    [`SemanticCache::invalidate`] on every write/delete/compact to
//!    free the memory immediately.
//! 2. **Lazy / defence-in-depth**: every lookup and insert compares
//!    the current generation against the list's stamp and drops a
//!    stale list before proceeding.
//!
//! Because the generation is the table version, **any** commit moves
//! it — including index builds. So unlike the earlier design, an index
//! build now drops the semantic entries on the next lookup, matching
//! the exact cache: the rows are unchanged, but post-index queries
//! re-run against the new index rather than replaying a cached top-k.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use dashmap::DashMap;

use crate::cache::invalidation::GenerationCounter;
use crate::metrics::CoreMetrics;
use crate::NamespaceId;

/// Maximum number of cached query vectors retained per namespace.
///
/// Bounds the per-namespace memory footprint and the worst-case
/// linear-scan cost per lookup. 1024 matches the value documented
/// in the v1 plan: each entry holds a query vector (`dim * 4`
/// bytes), a small pre-computed norm, the `k`/`nprobes` ints, and
/// the serialised result-cache bytes (which are the same payload
/// the exact cache already stores). Tuneable knob if real workloads
/// reveal a different sweet spot.
pub const SEMANTIC_CACHE_MAX_PER_NAMESPACE: usize = 1024;

/// One cached query and its serialised result-cache bytes.
#[derive(Debug, Clone)]
struct SemanticEntry {
    query_vector: Vec<f32>,
    /// L2 norm of `query_vector`, computed once at insert so
    /// cosine lookups don't pay it per candidate.
    norm: f32,
    k: usize,
    /// Resolved (post-default) `nprobes` — the cached result was
    /// produced with this value, so a lookup must match it before
    /// the cached bytes can be reused.
    nprobes: usize,
    /// Whether the cached bytes carry stored vectors. A vector-light
    /// payload must never answer a full-payload request (or vice
    /// versa), so this gates matches the same way `k`/`nprobes` do.
    include_vector: bool,
    /// Same byte payload the exact cache would have stored for this
    /// query. Decoding is the caller's responsibility.
    result_bytes: Vec<u8>,
}

/// Per-namespace state: the generation the entries are stamped
/// with, and a bounded FIFO of cached queries. Wrapped in a single
/// `Mutex` per namespace so concurrent reads against different
/// namespaces don't contend.
#[derive(Debug, Default)]
struct NamespaceState {
    generation: u64,
    entries: VecDeque<SemanticEntry>,
}

/// Outcome of a semantic-cache lookup. The metric counter is
/// recorded at the [`NamespaceService`](crate::NamespaceService)
/// callsite, not inside the sidecar, so the call shape mirrors
/// `NamespaceCache::try_get` (return value first, telemetry
/// outside).
#[derive(Debug)]
pub enum SemanticLookup {
    /// A cached query cleared the cosine threshold; reuse its bytes.
    /// The included similarity is the best score for diagnostics.
    Hit {
        /// Serialised result-cache bytes to decode and return.
        bytes: Vec<u8>,
        /// Cosine similarity of the matched entry against the
        /// incoming query — for diagnostics / explain output.
        similarity: f32,
    },
    /// Eligible lookup, no entry cleared the threshold.
    Miss,
    /// No cached entries at the current generation. Distinct from
    /// `Miss` so the rejections-counter `reason="empty_index"` label
    /// can stay accurate.
    EmptyIndex,
}

/// In-process semantic sidecar shared across namespaces.
///
/// Cloneable via `Arc<SemanticCache>` (the inner map and per-NS
/// state lock are already `Send + Sync`).
pub struct SemanticCache {
    inner: DashMap<NamespaceId, Mutex<NamespaceState>>,
    max_per_namespace: usize,
    generations: Arc<GenerationCounter>,
    metrics: Arc<CoreMetrics>,
}

impl SemanticCache {
    /// Construct a sidecar that consults `generations` for
    /// invalidation and reports counters against `metrics`. Use
    /// [`Self::with_capacity`] to override the per-namespace cap.
    pub fn new(generations: Arc<GenerationCounter>, metrics: Arc<CoreMetrics>) -> Self {
        Self::with_capacity(generations, metrics, SEMANTIC_CACHE_MAX_PER_NAMESPACE)
    }

    /// Same as [`Self::new`] but with a custom per-namespace cap —
    /// the tests use a small cap to exercise eviction without
    /// pushing through 1024 fake entries.
    pub fn with_capacity(
        generations: Arc<GenerationCounter>,
        metrics: Arc<CoreMetrics>,
        max_per_namespace: usize,
    ) -> Self {
        Self {
            inner: DashMap::new(),
            max_per_namespace: max_per_namespace.max(1),
            generations,
            metrics,
        }
    }

    /// Drop every cached query for `ns`. Called from
    /// [`NamespaceService`](crate::NamespaceService) alongside the
    /// exact-cache invalidation on writes / deletes / compactions.
    /// O(1) ignoring the per-namespace lock acquisition.
    pub fn invalidate(&self, ns: &NamespaceId) {
        if let Some(state) = self.inner.get(ns) {
            let mut state = state.lock().expect("semantic cache mutex poisoned");
            state.entries.clear();
            // The current generation is already bumped by the
            // exact-cache layer; sync the field so subsequent
            // inserts don't get stamped with a stale value.
            state.generation = self.generations.current(ns);
        }
    }

    /// Look up the best matching cached query for `ns`.
    ///
    /// Eligibility (single-vector, no FTS/hybrid/multivector) must
    /// already have been checked by the caller via
    /// [`validate_semantic_cache_request`](crate::validate_semantic_cache_request).
    ///
    /// Pure read: never inserts. The caller pairs this with
    /// [`Self::insert`] after running the backend query on miss.
    pub fn lookup(
        &self,
        ns: &NamespaceId,
        query_vector: &[f32],
        k: usize,
        nprobes: usize,
        include_vector: bool,
        min_similarity: f32,
    ) -> SemanticLookup {
        let current_gen = self.generations.current(ns);

        let Some(state_ref) = self.inner.get(ns) else {
            return SemanticLookup::EmptyIndex;
        };
        let mut state = state_ref.lock().expect("semantic cache mutex poisoned");

        // Lazy invalidation if a writer bumped the generation
        // without going through `invalidate` (e.g. someone clears
        // entries via a future code path). Cheap; the eager path
        // already keeps these in sync.
        if state.generation != current_gen {
            state.entries.clear();
            state.generation = current_gen;
            return SemanticLookup::EmptyIndex;
        }
        if state.entries.is_empty() {
            return SemanticLookup::EmptyIndex;
        }

        let q_norm = l2_norm(query_vector);
        if q_norm == 0.0 || !q_norm.is_finite() {
            // A zero-vector query has undefined cosine similarity;
            // treat as a miss rather than reusing arbitrary entries.
            return SemanticLookup::Miss;
        }

        let mut best: Option<(f32, &SemanticEntry)> = None;
        for entry in state.entries.iter() {
            // Shape gates first — short-circuits avoid the dot
            // product when k/nprobes/include_vector/dim don't match.
            if entry.k != k || entry.nprobes != nprobes || entry.include_vector != include_vector {
                continue;
            }
            if entry.query_vector.len() != query_vector.len() {
                continue;
            }
            if entry.norm == 0.0 {
                continue;
            }
            let dot = dot_product(&entry.query_vector, query_vector);
            let sim = dot / (entry.norm * q_norm);
            if !sim.is_finite() {
                continue;
            }
            if sim >= min_similarity {
                match best {
                    None => best = Some((sim, entry)),
                    Some((cur, _)) if sim > cur => best = Some((sim, entry)),
                    _ => {}
                }
            }
        }

        match best {
            Some((similarity, entry)) => {
                let _ = &self.metrics; // metrics recording done at the service callsite
                SemanticLookup::Hit {
                    bytes: entry.result_bytes.clone(),
                    similarity,
                }
            }
            None => SemanticLookup::Miss,
        }
    }

    /// Insert a fresh entry for `ns`. Called after a successful
    /// backend query has populated the exact cache. The supplied
    /// `generation` should be the value the exact-cache populate
    /// used; mismatches against the live generation are dropped
    /// silently (same wasted-work fate as the exact cache).
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &self,
        ns: &NamespaceId,
        generation: u64,
        query_vector: Vec<f32>,
        k: usize,
        nprobes: usize,
        include_vector: bool,
        result_bytes: Vec<u8>,
    ) {
        let current_gen = self.generations.current(ns);
        if generation != current_gen {
            // A writer raced this query; the new entry would never
            // be reachable on lookup. Skip the work.
            return;
        }

        let norm = l2_norm(&query_vector);
        if norm == 0.0 || !norm.is_finite() {
            // Zero / non-finite query vectors are unscored at
            // lookup time, so storing them would just waste a slot.
            return;
        }

        let entry = SemanticEntry {
            query_vector,
            norm,
            k,
            nprobes,
            include_vector,
            result_bytes,
        };

        let cap = self.max_per_namespace;
        let state_ref = self.inner.entry(ns.clone()).or_default();
        let mut state = state_ref.lock().expect("semantic cache mutex poisoned");
        if state.generation != current_gen {
            state.entries.clear();
            state.generation = current_gen;
        }
        if state.entries.len() >= cap {
            state.entries.pop_front();
        }
        state.entries.push_back(entry);
    }

    /// Number of cached entries for `ns` at the current generation.
    /// Test-only accessor; production code reads `/metrics` to
    /// reason about behaviour.
    pub fn len(&self, ns: &NamespaceId) -> usize {
        let Some(state) = self.inner.get(ns) else {
            return 0;
        };
        let state = state.lock().expect("semantic cache mutex poisoned");
        if state.generation != self.generations.current(ns) {
            return 0;
        }
        state.entries.len()
    }

    /// Whether the sidecar has any cached entries for `ns`.
    pub fn is_empty(&self, ns: &NamespaceId) -> bool {
        self.len(ns) == 0
    }
}

fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn l2_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::test_metrics;

    fn ns(name: &str) -> NamespaceId {
        NamespaceId::new(name).unwrap()
    }

    fn fixture() -> (SemanticCache, Arc<GenerationCounter>, NamespaceId) {
        let generations = Arc::new(GenerationCounter::new());
        let metrics = test_metrics();
        let cache = SemanticCache::with_capacity(Arc::clone(&generations), metrics, 4);
        let n = ns("nstest");
        // Match the post-first-write state so the first insert lands
        // at a non-zero generation, mirroring how
        // `NamespaceService::upsert` would have arranged things.
        generations.bump(&n);
        (cache, generations, n)
    }

    #[test]
    fn empty_namespace_reports_empty_index() {
        let (cache, _gens, n) = fixture();
        let lookup = cache.lookup(&n, &[1.0, 0.0], 10, 20, true, 0.99);
        assert!(matches!(lookup, SemanticLookup::EmptyIndex));
    }

    #[test]
    fn exact_repeat_hits() {
        let (cache, gens, n) = fixture();
        cache.insert(
            &n,
            gens.current(&n),
            vec![1.0, 0.0],
            10,
            20,
            true,
            b"payload".to_vec(),
        );
        let lookup = cache.lookup(&n, &[1.0, 0.0], 10, 20, true, 0.99);
        match lookup {
            SemanticLookup::Hit { bytes, similarity } => {
                assert_eq!(bytes, b"payload");
                assert!((similarity - 1.0).abs() < 1e-6, "sim={similarity}");
            }
            other => panic!("expected hit, got {other:?}"),
        }
    }

    #[test]
    fn near_duplicate_hits_when_threshold_lenient() {
        let (cache, gens, n) = fixture();
        cache.insert(
            &n,
            gens.current(&n),
            vec![1.0, 0.0, 0.0],
            10,
            20,
            true,
            b"close".to_vec(),
        );
        // 0.999 cosine ~ a tiny rotation
        let probe = vec![0.999_f32.sqrt(), (1.0_f32 - 0.999).sqrt(), 0.0];
        let lookup = cache.lookup(&n, &probe, 10, 20, true, 0.9);
        assert!(matches!(lookup, SemanticLookup::Hit { .. }));
    }

    #[test]
    fn below_threshold_misses() {
        let (cache, gens, n) = fixture();
        cache.insert(
            &n,
            gens.current(&n),
            vec![1.0, 0.0],
            10,
            20,
            true,
            b"x".to_vec(),
        );
        // Orthogonal: cosine == 0
        let lookup = cache.lookup(&n, &[0.0, 1.0], 10, 20, true, 0.5);
        assert!(matches!(lookup, SemanticLookup::Miss));
    }

    #[test]
    fn k_mismatch_skips_entry() {
        let (cache, gens, n) = fixture();
        cache.insert(
            &n,
            gens.current(&n),
            vec![1.0, 0.0],
            10,
            20,
            true,
            b"x".to_vec(),
        );
        let lookup = cache.lookup(&n, &[1.0, 0.0], 50, 20, true, 0.99);
        assert!(matches!(lookup, SemanticLookup::Miss));
    }

    #[test]
    fn include_vector_mismatch_skips_entry() {
        let (cache, gens, n) = fixture();
        cache.insert(
            &n,
            gens.current(&n),
            vec![1.0, 0.0],
            10,
            20,
            true,
            b"x".to_vec(),
        );
        // A full-payload entry must not answer a vector-light request.
        let lookup = cache.lookup(&n, &[1.0, 0.0], 10, 20, false, 0.99);
        assert!(matches!(lookup, SemanticLookup::Miss));
    }

    #[test]
    fn nprobes_mismatch_skips_entry() {
        let (cache, gens, n) = fixture();
        cache.insert(
            &n,
            gens.current(&n),
            vec![1.0, 0.0],
            10,
            20,
            true,
            b"x".to_vec(),
        );
        let lookup = cache.lookup(&n, &[1.0, 0.0], 10, 50, true, 0.99);
        assert!(matches!(lookup, SemanticLookup::Miss));
    }

    #[test]
    fn generation_bump_drops_entries_on_next_lookup() {
        let (cache, gens, n) = fixture();
        cache.insert(
            &n,
            gens.current(&n),
            vec![1.0, 0.0],
            10,
            20,
            true,
            b"x".to_vec(),
        );
        assert_eq!(cache.len(&n), 1);

        // Simulate a write bumping the generation; the eager
        // invalidate path is not invoked here, so we lean on the
        // lazy drop inside `lookup`.
        gens.bump(&n);
        let lookup = cache.lookup(&n, &[1.0, 0.0], 10, 20, true, 0.99);
        assert!(matches!(lookup, SemanticLookup::EmptyIndex));
        assert_eq!(cache.len(&n), 0);
    }

    #[test]
    fn explicit_invalidate_drops_entries() {
        let (cache, gens, n) = fixture();
        cache.insert(
            &n,
            gens.current(&n),
            vec![1.0, 0.0],
            10,
            20,
            true,
            b"x".to_vec(),
        );
        gens.bump(&n);
        cache.invalidate(&n);
        assert_eq!(cache.len(&n), 0);
    }

    #[test]
    fn insert_against_stale_generation_is_dropped() {
        let (cache, gens, n) = fixture();
        let stale_gen = gens.current(&n);
        gens.bump(&n);
        cache.insert(&n, stale_gen, vec![1.0, 0.0], 10, 20, true, b"x".to_vec());
        // Nothing landed: the lazy reset sees the empty list.
        assert_eq!(cache.len(&n), 0);
    }

    #[test]
    fn fifo_evicts_oldest_when_capacity_exceeded() {
        let (cache, gens, n) = fixture();
        // capacity=4 from fixture()
        for i in 0..6_usize {
            let mut v = vec![0.0_f32; 4];
            let idx = i % v.len();
            v[idx] = 1.0 + i as f32 * 0.001;
            cache.insert(&n, gens.current(&n), v, 10, 20, true, vec![i as u8]);
        }
        assert_eq!(cache.len(&n), 4);
    }

    #[test]
    fn zero_vector_lookup_misses_without_panic() {
        let (cache, gens, n) = fixture();
        cache.insert(
            &n,
            gens.current(&n),
            vec![1.0, 0.0],
            10,
            20,
            true,
            b"x".to_vec(),
        );
        let lookup = cache.lookup(&n, &[0.0, 0.0], 10, 20, true, 0.5);
        assert!(matches!(lookup, SemanticLookup::Miss));
    }
}
