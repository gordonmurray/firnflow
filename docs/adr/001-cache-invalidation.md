# ADR-001: Cache invalidation via per-namespace generation counter

- Status: **accepted** (initial — revisit after the orphan-accumulation stress test)
- Date: 2026-04-11
- Spike branch: `spike/cache-invalidation`

## Context

firnflow caches complete serialised query result sets in a foyer
`HybridCache` (RAM + NVMe) to keep warm-query latency well below the S3
round-trip that a cold query requires. The cache is shared across all
namespaces. On any write to a namespace, every cached result for that
namespace is potentially stale and must be invalidated before the next
read.

foyer's eviction is per cache entry. It has no native prefix-scan or
tag-based invalidation, so the naive "walk every entry that matches
`(ns, *)`" approach is not available, and reaching into foyer's
internals to expose one would couple us to implementation details that
churn between releases.

Design goals:

1. **Correctness.** A read that races with a write must never return a
   result that was populated from the pre-write state.
2. **O(1) write cost.** Invalidation must not scale with the number of
   cached queries for the namespace — that number can grow without
   bound under query diversity.
3. **Bounded auxiliary memory.** The invalidation metadata must not
   grow per cache entry; only per namespace.

## Decision

Embed a per-namespace generation counter in the cache key.

```rust
struct CacheKey {
    namespace: NamespaceId,
    generation: u64,
    query: QueryHash,
}
```

The generation counter lives in a
`DashMap<NamespaceId, AtomicU64>`. On reads, the cache layer captures
the current generation for the namespace and uses it as part of the
lookup key. On any write, the generation is atomically bumped. Every
previously populated entry for that namespace carries the old
generation and is therefore unreachable by key — foyer's normal LFU/LRU
policies reclaim the underlying bytes over time.

A capture-once discipline in the cache-aside path prevents a subtle
race: the generation is snapshotted at the *start* of
`get_or_populate` and used for both the lookup and the subsequent
insert. If a writer bumps the generation mid-populate, the insert
lands at the pre-write generation and is immediately unreachable —
wasted work, never served as a stale hit.

## Alternatives considered

### A. Explicit per-entry index (`NamespaceCacheIndex`)

Maintain `DashMap<NamespaceId, HashSet<CacheKey>>` alongside the
cache. On write, walk the set and call `cache.remove()` per entry.

- Pro: stale entries are reclaimed immediately; no orphans in the NVMe
  tier.
- Con: auxiliary memory grows with the number of unique queries per
  namespace — one HashSet entry per cached query, potentially
  unbounded under query diversity.
- Con: write cost is O(n) in cached entries for the namespace.
- Con: introduces a race window between cache population and index
  registration; closing it requires a lock around the populate path.

### B. Reach into foyer internals

Fork or patch foyer to expose prefix/tag invalidation.

- Con: couples us tightly to foyer's internals at a point where the
  project is evolving fast.
- Con: every foyer bump becomes a re-port.

### C. Per-namespace foyer instance

One `HybridCache` per namespace. Invalidation is
`cache.close(); cache = new`.

- Con: defeats global RAM/NVMe budgeting — each namespace gets its
  own tier, and idle namespaces hold memory.
- Con: amortised construction cost on every invalidation.
- Con: contradicts the "single cache instance" architecture
  constraint in `CLAUDE.md`.

## Consequences

- **Positive.** Write cost is O(1) regardless of cached query count.
- **Positive.** Auxiliary state is one `(NamespaceId, u64)` per
  *namespace*, not per cached entry.
- **Positive.** No lock around the populate path — the capture-once
  discipline handles the race.
- **Negative.** Stale-generation entries accumulate in the NVMe tier
  until LFU/LRU reclaims them. On a write-heavy namespace with diverse
  queries this could inflate the NVMe footprint transiently. The
  severity is unknown until measured (see Definition of done, item 3).

## Definition of done

1. Correctness integration test
   (`crates/firnflow-core/tests/cache_invalidation.rs`,
   `invalidation_returns_fresh_results_after_write`) passes. **Done
   in this PR.**
2. Cross-namespace isolation test
   (`invalidation_is_per_namespace`) passes: invalidating namespace A
   must not affect namespace B's cached entries. **Done in this PR.**
3. Orphan-accumulation stress test: drive a write-heavy workload
   against one namespace, measure NVMe footprint growth over time,
   and confirm LFU/LRU reclaims unreachable entries inside a bounded
   envelope. **TODO** — requires workload design; deferred to a
   follow-up PR under the same spike branch.
4. If orphan accumulation exceeds the envelope, re-evaluate
   alternative A and supersede this ADR. **Conditional on (3).**
