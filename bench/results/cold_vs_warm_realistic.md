# Cold vs warm query latency — realistic parameters (slice 6d)

- **Date**: 2026-04-12
- **Harness**: `./scripts/cargo run --release -p firnflow-bench`
- **Backend**: MinIO (see `docs/provider-support.md` for the pinned digest)
- **Config**: dim=1536, rows=100000, queries=50, nprobes=20
- **Storage**: ~586 MB raw vector data
- **Cache**: RAM=16MB, NVMe=1024MB
- **Upsert**: linear=2.6s, indexed=2.8s
- **Index build**: IVF_PQ (partitions=316, sub_vectors=96) in **147.2s**

## Four-phase latency comparison

| phase       | path |    p50     |    p95     |    p99     |    max     |
| ----------- | ---- | ----------:| ----------:| ----------:| ----------:|
| linear scan | cold |     418.20 ms |     459.08 ms |     533.86 ms |     533.86 ms |
| linear scan | warm |      61.97 us |     101.55 us |     130.45 us |     130.45 us |
| IVF_PQ      | cold |      71.35 ms |      97.05 ms |     105.22 ms |     105.22 ms |
| IVF_PQ      | warm |      55.99 us |      96.34 us |     106.89 us |     106.89 us |

## Speedup ratios

| comparison | p50 | p99 |
| ---------- | ---:| ---:|
| linear cold → warm | 6749x | 4092x |
| indexed cold → warm | 1274x | 984x |
| linear cold → indexed cold | 5.9x | 5.1x |

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
