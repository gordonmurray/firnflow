# Serialisation benchmark — spike-3 baseline

- **Date**: 2026-04-11
- **Benchmark**: `crates/firnflow-core/benches/serialisation.rs`
- **Run**: `./scripts/cargo bench -p firnflow-core --bench serialisation`
- **Serialiser**: bincode 2 (serde path) with `config::standard()`
- **Payload**: `QueryResultSet { query_id, Vec<QueryResult { id, score, vector: Vec<f32; 1536] }> }`
- **Iterations**: 200 warmup / 2000 samples per phase
- **Host**: `firnflow-rust-dev:1.94` (rust:1.94-bookworm) under `./scripts/cargo bench` (release profile)

## Raw numbers

```
bincode 2 (serde path) — 1536-dim f32 vectors, 200 warmup / 2000 samples
phase             bytes           p50           p95           p99           max
------------------------------------------------------------------------------
encode/10         61536      12.88 µs      15.24 µs      17.27 µs      25.67 µs
decode/10         61536      16.96 µs      17.17 µs      18.32 µs      21.08 µs
rt/10             61536      29.79 µs      30.12 µs      31.39 µs      39.57 µs

encode/100       615217     135.28 µs     137.74 µs     139.55 µs     148.00 µs
decode/100       615217     143.84 µs     152.90 µs     174.06 µs    1774.33 µs
rt/100           615217     270.51 µs     275.61 µs     317.64 µs     448.13 µs

encode/1000     6153518    1120.95 µs    1160.35 µs    1377.44 µs    2730.35 µs
decode/1000     6153518    1462.33 µs    1629.96 µs    2855.22 µs    4181.56 µs
rt/1000         6153518    2598.19 µs    2765.51 µs    3023.14 µs    7468.15 µs
```

## Decision gate (CLAUDE.md § 3)

> If bincode round-trip exceeds 1 ms at p99 for 100-result sets,
> evaluate rkyv and flatbuffers as alternatives.

**100-result round-trip p99 = 317.64 µs ≈ 0.32 ms.** The gate does
not trigger. See `docs/adr/002-result-serialisation.md` for the
full decision and alternative analysis.

## Observations

- **Encode is consistent, decode is bursty.** At 100 results the
  decode p99 is 174 µs but the max is 1774 µs — a ~10× tail
  presumably driven by heap allocations for the inner `Vec<f32>`
  buffers. The round-trip timing smooths this out because encode
  never hits that stall. Worth watching if we observe spiky tail
  latencies in the real query path; a reusable-buffer decode API
  (bincode's `borrow_decode` or rkyv's zero-copy read) would
  eliminate it.
- **1000-result tier exceeds 1 ms round-trip** (p99 ≈ 3 ms). This is
  above the threshold CLAUDE.md set for the 100-result tier but
  below the threshold for 1000. The spec only gates on 100, so we
  accept bincode, but the 1000-result number is called out in the
  ADR as the trigger for a future re-evaluation.
- **Encode size is ~6144 bytes per result plus framing.** The raw
  vector is 1536 × 4 = 6144 bytes, so bincode's `config::standard()`
  adds roughly 12 bytes of framing per result — negligible
  overhead vs. the vector itself.
