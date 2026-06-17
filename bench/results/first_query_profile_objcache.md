# First-query latency profile

- **Date**: 2026-06-17
- **start_utc**: 2026-06-17T14:18:26Z
- **stop_utc**: 2026-06-17T14:18:44Z
- **Backend**: S3 (eu-west-1)
- **Storage prefix**: `first-query-profile-1m/` under the configured bucket, a stable S3 access-log filter
- **Namespace**: `first-query-profile-1m`
- **Config**: rows=1000000, dim=1536, k=10, nprobes=20, reps=20
- **Foyer cache**: RAM=16 MB, NVMe=256 MB (fresh tempdir per service build)
- **Object cache**: ENABLED, dir `/tmp/firn-obj-cache`, budget 10 GiB (persists across reps)
- **Harness**: `./scripts/cargo run --release -p firnflow-bench --bin first_query_profile`

## Seed

- Reused existing namespace (no seed step). Probe query: 932.51 ms

## Per-case latency

| case | reps | p50 | p95 | p99 | min | max | s3_requests delta |
| ---- | ---:| ---:| ---:| ---:| ---:| ---:| ---: |
| cold-process | 20 | 382.67 ms | 509.73 ms | 509.73 ms | 290.12 ms | 509.73 ms | 0 |
| warm-identical | 20 | 5.37 ms | 5.92 ms | 5.92 ms | 5.06 ms | 5.92 ms | 0 |
| warm-novel | 20 | 7.18 ms | 193.86 ms | 193.86 ms | 6.48 ms | 193.86 ms | 0 |
| dropped-handle | 20 | 278.59 ms | 335.89 ms | 335.89 ms | 210.01 ms | 335.89 ms | 0 |
| fresh-process | 20 | 143.54 ms | 155.16 ms | 155.16 ms | 132.25 ms | 155.16 ms | 0 |

## Case definitions

- **cold-process**: fresh manager + cache + service per rep, one query each.
- **warm-identical**: single warm service, repeated identical query (bypasses foyer).
- **warm-novel**: single warm service, novel query per rep.
- **dropped-handle**: evict pooled (Connection, Table) between reps; process stays warm.
- **fresh-process**: fresh manager + cache + service per rep, run AFTER all other cases so AWS SDK / process state is now warm (in-process variant only).

## Raw samples

- `cold-process` (20 reps): 290.12 ms, 419.09 ms, 443.92 ms, 509.73 ms, 466.83 ms, 331.31 ms, 392.32 ms, 353.76 ms, 394.66 ms, 357.05 ms, 427.22 ms, 397.03 ms, 344.06 ms, 364.07 ms, 382.67 ms, 378.57 ms, 431.16 ms, 360.17 ms, 349.67 ms, 357.34 ms
- `warm-identical` (20 reps): 5.10 ms, 5.14 ms, 5.16 ms, 5.47 ms, 5.37 ms, 5.40 ms, 5.31 ms, 5.34 ms, 5.06 ms, 5.49 ms, 5.15 ms, 5.61 ms, 5.14 ms, 5.51 ms, 5.42 ms, 5.43 ms, 5.39 ms, 5.11 ms, 5.92 ms, 5.23 ms
- `warm-novel` (20 reps): 6.65 ms, 6.68 ms, 7.41 ms, 7.54 ms, 6.71 ms, 6.48 ms, 8.84 ms, 6.99 ms, 7.29 ms, 7.06 ms, 7.18 ms, 7.45 ms, 7.15 ms, 7.17 ms, 7.11 ms, 7.64 ms, 7.57 ms, 7.45 ms, 7.15 ms, 193.86 ms
- `dropped-handle` (20 reps): 272.10 ms, 309.63 ms, 300.77 ms, 226.70 ms, 282.81 ms, 335.89 ms, 309.32 ms, 297.35 ms, 259.85 ms, 318.28 ms, 307.59 ms, 255.24 ms, 210.01 ms, 268.15 ms, 220.03 ms, 241.83 ms, 249.27 ms, 305.90 ms, 261.22 ms, 278.59 ms
- `fresh-process` (20 reps): 143.54 ms, 137.89 ms, 145.98 ms, 142.95 ms, 145.98 ms, 153.87 ms, 155.16 ms, 139.99 ms, 142.47 ms, 135.17 ms, 145.42 ms, 143.41 ms, 140.98 ms, 144.56 ms, 141.80 ms, 145.14 ms, 132.25 ms, 144.52 ms, 135.65 ms, 151.21 ms

## Object cache counters

Cumulative over the whole run (seed + all cases), shared across every service build:

- hits: 7823
- misses: 1853
- inner_gets (reads that fell through to object storage): 2025
- s3_bytes (bytes fetched from object storage): 91675965
- evictions: 0

## Caveats

- **`cold-process` is a lower bound on a true fresh-process number.** Every repetition runs inside one binary invocation, so the AWS SDK HTTP client pool, TLS sessions, and the Tokio runtime persist across repetitions even though the in-process `NamespaceManager` + cache + service objects are rebuilt each rep. A true fresh-process measurement needs an outer driver that spawns a fresh binary per repetition. The `fresh-process` case runs after every other case so the difference vs `cold-process` is a coarse signal for SDK / connection warmup.
- **`s3_requests delta` only counts firnflow's service-boundary calls**, not raw `object_store` GETs / range GETs. For real S3 access-log attribution use the namespace prefix above as the path filter (primary) or the start/stop UTC window as a backstop.
- **`warm-identical` and `warm-novel` bypass the foyer result cache** by calling `NamespaceManager::query` directly. This isolates the LanceDB / index / handle-pool warm-state cost. A foyer hit would otherwise dominate the number and tell us nothing about the underlying object-store path.
