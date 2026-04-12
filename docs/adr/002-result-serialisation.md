# ADR-002: Cached result set serialisation — bincode 2 via serde

- Status: **accepted** (revisit if 1000-result queries become a hot path)
- Date: 2026-04-11
- Spike branch: `spike/serialisation`

## Context

The cache layer stores complete serialised query result sets as
opaque `Vec<u8>` values inside a foyer `HybridCache`. Every cache
hit pays the cost of deserialising one of those blobs back into
something the API handler can return; every cache population pays
the cost of serialising one. For namespaces with large result
pages, serialisation in the API layer can become the dominant cost
long before S3 access does, defeating the point of the cache.

CLAUDE.md § "Known hard problems" 3 sets an explicit gate for the
spike: *"If bincode round-trip exceeds 1 ms at p99 for 100-result
sets, evaluate rkyv and flatbuffers as alternatives."*

## Decision

Use **bincode 2 via the serde path** with `bincode::config::standard()`.
This is the default choice that foyer's `serde` feature activates,
and the measured round-trip latency at 100 results is well below
the 1 ms threshold the spec gates on.

The cached payload type is:

```rust
struct QueryResult  { id: u64, score: f32, vector: Vec<f32> }  // 1536-dim
struct QueryResultSet { query_id: String, results: Vec<QueryResult> }
```

Both derive `serde::{Serialize, Deserialize}`. No framework-specific
annotations (no `rkyv::Archive`, no flatbuffers schema) — the type
stays serde-compatible and any future swap of serialiser is a
crate-level change, not a type-level one.

## Measured latency

Full numbers at `bench/results/serialisation.md`. Headline:

| size (hits) | encoded bytes | encode p99 | decode p99 | round-trip p99 |
| ---: | ---: | ---: | ---: | ---: |
|   10 |      61 536 |  17.27 µs |  18.32 µs |    31.39 µs |
|  100 |     615 217 | 139.55 µs | 174.06 µs |   317.64 µs |
| 1000 |   6 153 518 |  1.38 ms  |   2.86 ms |     3.02 ms |

The 100-result round-trip p99 is **317.64 µs = 0.32 ms**, about
**3× under** the CLAUDE.md threshold. The gate does not trigger.

## Alternatives considered

### A. bincode 2 native (Encode/Decode derive), no serde

bincode 2 ships its own derive macros that skip serde's visitor
machinery entirely.

- **Pro**: typically 1.3–2× faster than the serde path for similar
  payloads; fewer trait indirections.
- **Con**: requires every cached type to carry two derives (serde
  and bincode). The API layer needs serde anyway for request/
  response JSON, so the type ends up with both.
- **Con**: not needed. The serde path already meets the
  latency budget with meaningful headroom.
- **Verdict**: defer. If a future workload pushes the 100-result
  round-trip past ~700 µs p99, switch to native bincode first
  before reaching for rkyv.

### B. rkyv (zero-copy archives)

rkyv deserialises by pointing into the original byte buffer — no
allocation of the inner `Vec<f32>` per-hit — which would eliminate
the decode-side tail (the 100-result decode max of 1774 µs is
almost certainly allocator stalls on those buffers).

- **Pro**: effectively zero-copy reads; decode becomes a bounds
  check rather than a parse.
- **Pro**: removes the decode-side tail described in the bench
  observations.
- **Con**: every cached type must implement `rkyv::Archive` /
  `Serialize` / `Deserialize` — a parallel derive tree to serde.
  That is exactly the type-design lock-in CLAUDE.md flagged ("must
  be decided before the type stabilises").
- **Con**: the archived type is not the same as the live type, so
  every call site that reads from cache handles an `Archived<T>`
  projection rather than a `T`. The API layer ergonomics regress.
- **Verdict**: reject for now. Reconsider if (a) the 100-result
  round-trip p99 drifts past the threshold, or (b) the
  1000-result tier becomes a hot path and 3 ms decode is
  unacceptable.

### C. flatbuffers

Schema-driven, language-agnostic, with lazy field access similar
to rkyv in spirit.

- **Pro**: zero-copy reads.
- **Pro**: the schema is authoritative and cross-language, which
  would matter if we ever ship a non-Rust client against the cache
  format directly (we don't, and there's no plan to).
- **Con**: requires a separate `.fbs` schema file plus a build-time
  generator, and the generated types are not ergonomic in Rust.
- **Con**: buys us nothing that rkyv doesn't buy, and rkyv is
  more idiomatic for pure-Rust code.
- **Verdict**: reject. Only revisit if a multi-language cache
  protocol becomes a real requirement.

### D. Raw byte format (hand-rolled)

Concat the fixed-width fields manually — `u64` for id, `f32` for
score, `f32 * 1536` for the vector. Skip framing, skip headers.

- **Pro**: maximum speed — essentially memcpy.
- **Con**: no schema versioning. A single field addition is a
  silent compatibility break, which trades a latency problem we
  don't have for a correctness problem we'd have to debug under
  load.
- **Verdict**: reject. Premature optimisation; the latency budget
  is already met.

## Consequences

- **Positive.** The cached type derives only serde. No framework
  lock-in; any future swap (native bincode, rkyv) is a
  serialiser-level change, not a type-level one.
- **Positive.** foyer's default `serde` feature works directly
  with `QueryResultSet` — no custom `StorageValue` impl needed.
- **Positive.** The 100-result round-trip has meaningful headroom
  (~3×) under the CLAUDE.md threshold, absorbing growth in payload
  size or decode cost without re-architecting.
- **Negative.** 1000-result queries pay ~3 ms per round-trip. If a
  workload regularly pulls 1000 hits out of the cache, the
  deserialise step will be the dominant latency. Document the
  cliff; do not cache result sets above ~500 hits without
  reconsidering this ADR.
- **Negative.** The decode-side tail (100-result max of 1.77 ms vs
  p99 of 174 µs) is almost certainly allocator-driven. It does not
  breach the budget today, but it is worth monitoring once the API
  layer is wired up and production query mixes are observable.

## Definition of done

1. Benchmark committed to `bench/results/serialisation.md` with
   p50/p95/p99/max for each tier. **Done.**
2. Decision captured in this ADR with the measured numbers and
   explicit rejection rationale for each alternative. **Done.**
3. `QueryResultSet` / `QueryResult` types committed to
   `firnflow-core` under `result.rs`, derived with serde.
   **Done.**

## Revisit triggers

Reopen this ADR if any of the following become true:

- 100-result round-trip p99 drifts past 700 µs on a representative
  workload (i.e. uses half the headroom).
- 1000-result queries become a hot path (>1 query/sec workload-wide)
  and 3 ms round-trip is measured as the dominant latency.
- The API layer starts streaming result pages rather than
  materialising them, at which point the cache payload shape
  should change anyway.
