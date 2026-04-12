# Cold vs warm query latency — realistic parameters (slice 6d)

- **Date**: 2026-04-12
- **Harness**: `./scripts/cargo run --release -p firnflow-bench`
- **Backend**: MinIO (see `docs/provider-support.md` for the pinned digest)
- **Config**: dim=1536, rows=100000, queries=50, nprobes=20
- **Storage**: ~586 MB raw vector data
- **Cache**: RAM=16MB, NVMe=1024MB
- **Upsert**: linear=55.7s, indexed=59.2s
- **Index build**: IVF_PQ (partitions=316, sub_vectors=96) in **204.2s**

## Four-phase latency comparison

| phase       | path |    p50     |    p95     |    p99     |    max     |
| ----------- | ---- | ----------:| ----------:| ----------:| ----------:|
| linear scan | cold |      25.14 s  |      29.50 s  |      30.77 s  |      30.77 s  |
| linear scan | warm |      66.23 us |     141.29 us |     165.41 us |     165.41 us |
| IVF_PQ      | cold |     979.12 ms |       1.28 s  |       3.27 s  |       3.27 s  |
| IVF_PQ      | warm |      72.46 us |     160.72 us |     295.72 us |     295.72 us |

## Speedup ratios

| comparison | p50 | p99 |
| ---------- | ---:| ---:|
| linear cold → warm | 379517x | 186022x |
| indexed cold → warm | 13512x | 11066x |
| linear cold → indexed cold | 25.7x | 9.4x |

## Cache + S3 request asymmetry

| namespace | cache misses | cache hits | observation |
| --------- | -----------: | ---------: | ----------- |
| linear    | 50 | 50 | 50 cold queries → 50 S3 trips; 50 warm → 0 |
| indexed   | 50 | 50 | same pattern, but cold queries are dramatically faster |

## The thesis

1. **The cache without the index is a liability.** It hides the underlying linear-scan cost, which surfaces on every cache miss.
2. **The index without the cache leaves money on the table.** Repeat queries still pay the (now-fast) S3 round-trip.
3. **Together, the ANN index and the tiered cache make each other more valuable.** Removing either is strictly worse.

## Notes

- Each run starts cold: fresh `tempfile::tempdir` for the foyer NVMe tier, fresh namespace timestamps.
- `s3_requests_total` counts firnflow-initiated operations at the service boundary, not raw HTTP requests to S3.
- Index build time is the "Index Tax" — paid once, amortised across all subsequent queries.
