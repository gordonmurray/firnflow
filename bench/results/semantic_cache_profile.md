# Semantic cache profile

- **Start UTC**: 2026-05-31T12:35:36Z
- **Stop UTC**: 2026-05-31T12:35:41Z
- **Harness**: `./scripts/cargo run --release -p firnflow-bench --bin semantic_cache_profile -j 1`
- **Backend**: `s3://firnflow`
- **Namespace**: `bench-semantic-1780230936697832392`
- **Config**: dim=512, rows=10000, reps=200, k=10, nprobes=20, threshold=0.995000
- **Storage**: ~19.5 MB raw vector data
- **Cache**: RAM=16MB, NVMe=256MB
- **Upsert**: 0.3s
- **Index build**: IVF_PQ (partitions=100, sub_vectors=32) in 1.6s

The benchmark uses deterministic normalized synthetic vectors and drives `NamespaceService` in-process. Semantic-hit cases seed the sidecar with a real encoded top-k result, then issue a near-duplicate query whose exact-cache key is different.

## Latency by tier

| case | samples | p50 | p95 | p99 | max | backend queries | semantic hits | semantic misses | exact hits | exact misses |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cold novel | 200 | 6.34 ms | 12.28 ms | 16.90 ms | 18.64 ms | 200 | 0 | 200 | 0 | 200 |
| foyer hit | 200 | 19.03 us | 23.21 us | 60.91 us | 64.68 us | 0 | 0 | 0 | 200 | 0 |
| semantic hit | 200 | 28.39 us | 34.31 us | 46.42 us | 57.79 us | 0 | 200 | 0 | 0 | 200 |
| semantic miss below threshold | 200 | 4.98 ms | 10.34 ms | 13.90 ms | 14.99 ms | 200 | 0 | 200 | 0 | 200 |

## Primary comparison

| comparison | p50 ratio |
| --- | ---: |
| cold novel / semantic hit | 223.1x |
| cold novel / foyer hit | 332.9x |

The semantic-hit probe was generated at cosine 0.997000 against the cached query. Its true backend top-k overlapped the reused cached top-k by 6/10 ids.

The below-threshold miss probe was generated at cosine 0.994000. It should fall through to LanceDB at threshold 0.995000; backend query counts in the latency table confirm that behavior.

## Threshold cliff

| target cosine | observed cosine | outcome | overlap vs true top-k |
| ---: | ---: | --- | ---: |
| 0.999000 | 0.999000669 | semantic-hit | 9/10 |
| 0.997000 | 0.996999562 | semantic-hit | 2/10 |
| 0.995000 | 0.994999766 | semantic-miss | 10/10 |
| 0.990000 | 0.989999831 | semantic-miss | 10/10 |
| 0.970000 | 0.969999731 | semantic-miss | 10/10 |
| 0.950000 | 0.949999809 | semantic-miss | 10/10 |
| 0.900000 | 0.899999619 | semantic-miss | 10/10 |
| 0.850000 | 0.849999785 | semantic-miss | 10/10 |

## Scan cost

| sidecar entries | samples | p50 | p95 | p99 | max | semantic hits |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 200 | 15.96 us | 32.11 us | 48.81 us | 79.38 us | 200 |
| 16 | 200 | 18.51 us | 19.50 us | 25.66 us | 41.53 us | 200 |
| 128 | 200 | 39.26 us | 46.36 us | 49.77 us | 74.21 us | 200 |
| 512 | 200 | 114.11 us | 171.71 us | 358.64 us | 387.23 us | 200 |
| 1024 | 200 | 205.34 us | 227.69 us | 336.96 us | 411.38 us | 200 |

## Notes

- The semantic sidecar is a linear scan over at most 1024 entries per namespace generation.
- `backend queries` is the service-level S3-bound query counter, not raw HTTP request count.
- Semantic hits return the cached neighbour's serialized top-k bytes. The overlap number is the quality check against a fresh backend query for the probe vector.
