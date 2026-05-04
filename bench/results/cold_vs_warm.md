# Cold vs warm query latency — initial baseline

- **Date**: 2026-04-11
- **Harness**: `./scripts/cargo run --release -p firnflow-bench`
- **Backend**: MinIO (see `docs/provider-support.md` for the pinned digest)
- **Config**: dim=32, rows=100, queries=50
- **Upsert phase**: completed in 33 ms

## Cold vs warm latency

| phase | queries |    p50     |    p95     |    p99     |    max     |
| ----- | -------:| ----------:| ----------:| ----------:| ----------:|
| cold  |      50 |  14580.92 µs |  21233.50 µs |  26047.17 µs |  26047.17 µs |
| warm  |      50 |      2.60 µs |      3.75 µs |     11.57 µs |     11.57 µs |

**Speedup** (cold / warm): p50 = **5616.7×**, p99 = **2250.9×**

## Cache + S3 request asymmetry

| metric                                             | value |
| -------------------------------------------------- | ----: |
| `firnflow_cache_misses_total`                    | 50 |
| `firnflow_cache_hits_total`                      | 50 |
| `firnflow_s3_requests_total{op=upsert}`        | 1 |
| `firnflow_s3_requests_total{op=query}`         | 50 |

The load-bearing observation: **50 query-kind S3 requests for 50 cold queries, then **0 additional** for 50 warm queries**. Every warm query was served from foyer without touching the backend, which is the whole reason the cache exists.

## Notes

- `s3_requests_total` counts firnflow-initiated S3-bound *operations* at the service boundary, not raw HTTP requests to S3 — see the help text on the metric for the approximation caveat.
- Each run starts cold: the bench uses a fresh `tempfile::tempdir` for the foyer NVMe tier and a fresh namespace timestamp so nothing is reused between runs.
- The vector dimension here (32) is deliberately smaller than the 1536 used in the serialisation benchmark so the run completes in seconds against MinIO over the `--network host` loopback. Bump it for production-representative numbers.
