# First-query latency profile

- **Date**: 2026-06-17
- **start_utc**: 2026-06-17T13:53:17Z
- **stop_utc**: 2026-06-17T13:56:59Z
- **Backend**: S3 (eu-west-1)
- **Storage prefix**: `first-query-profile-smoke/` under the configured bucket, a stable S3 access-log filter
- **Namespace**: `first-query-profile-smoke`
- **Config**: rows=250000, dim=1536, k=10, nprobes=20, reps=10
- **Foyer cache**: RAM=16 MB, NVMe=256 MB (fresh tempdir per service build)
- **Harness**: `./scripts/cargo run --release -p firnflow-bench --bin first_query_profile`

## Seed

- Seeded this run: upsert 23.76 s, index 157.84 s, compact 17.32 s

## Per-case latency

| case | reps | p50 | p95 | p99 | min | max | s3_requests delta |
| ---- | ---:| ---:| ---:| ---:| ---:| ---:| ---: |
| cold-process | 10 | 667.14 ms | 700.06 ms | 700.06 ms | 583.32 ms | 700.06 ms | 0 |
| warm-identical | 10 | 36.45 ms | 85.51 ms | 85.51 ms | 32.63 ms | 85.51 ms | 0 |
| warm-novel | 10 | 191.06 ms | 221.59 ms | 221.59 ms | 158.94 ms | 221.59 ms | 0 |
| dropped-handle | 10 | 636.85 ms | 773.85 ms | 773.85 ms | 569.35 ms | 773.85 ms | 0 |
| fresh-process | 10 | 596.49 ms | 644.05 ms | 644.05 ms | 553.67 ms | 644.05 ms | 0 |

## Case definitions

- **cold-process**: fresh manager + cache + service per rep, one query each.
- **warm-identical**: single warm service, repeated identical query (bypasses foyer).
- **warm-novel**: single warm service, novel query per rep.
- **dropped-handle**: evict pooled (Connection, Table) between reps; process stays warm.
- **fresh-process**: fresh manager + cache + service per rep, run AFTER all other cases so AWS SDK / process state is now warm (in-process variant only).

## Raw samples

- `cold-process` (10 reps): 700.06 ms, 634.24 ms, 692.64 ms, 697.25 ms, 671.02 ms, 583.32 ms, 644.67 ms, 629.04 ms, 667.14 ms, 603.09 ms
- `warm-identical` (10 reps): 51.40 ms, 34.85 ms, 32.63 ms, 85.51 ms, 34.22 ms, 36.45 ms, 34.41 ms, 37.57 ms, 38.76 ms, 33.81 ms
- `warm-novel` (10 reps): 192.69 ms, 221.59 ms, 159.46 ms, 162.60 ms, 158.94 ms, 191.06 ms, 204.36 ms, 168.87 ms, 189.68 ms, 195.98 ms
- `dropped-handle` (10 reps): 636.85 ms, 642.77 ms, 723.30 ms, 773.85 ms, 597.76 ms, 625.43 ms, 579.49 ms, 569.35 ms, 591.91 ms, 670.82 ms
- `fresh-process` (10 reps): 560.59 ms, 593.71 ms, 588.28 ms, 555.81 ms, 603.96 ms, 621.04 ms, 596.49 ms, 644.05 ms, 553.67 ms, 597.06 ms

## Caveats

- **`cold-process` is a lower bound on a true fresh-process number.** Every repetition runs inside one binary invocation, so the AWS SDK HTTP client pool, TLS sessions, and the Tokio runtime persist across repetitions even though the in-process `NamespaceManager` + cache + service objects are rebuilt each rep. A true fresh-process measurement needs an outer driver that spawns a fresh binary per repetition. The `fresh-process` case runs after every other case so the difference vs `cold-process` is a coarse signal for SDK / connection warmup.
- **`s3_requests delta` only counts firnflow's service-boundary calls**, not raw `object_store` GETs / range GETs. For real S3 access-log attribution use the namespace prefix above as the path filter (primary) or the start/stop UTC window as a backstop.
- **`warm-identical` and `warm-novel` bypass the foyer result cache** by calling `NamespaceManager::query` directly. This isolates the LanceDB / index / handle-pool warm-state cost. A foyer hit would otherwise dominate the number and tell us nothing about the underlying object-store path.
