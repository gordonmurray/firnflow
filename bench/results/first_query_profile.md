# First-query latency profile

- **Date**: 2026-06-17
- **start_utc**: 2026-06-17T13:58:05Z
- **stop_utc**: 2026-06-17T14:04:45Z
- **Backend**: S3 (eu-west-1)
- **Storage prefix**: `first-query-profile-1m/` under the configured bucket, a stable S3 access-log filter
- **Namespace**: `first-query-profile-1m`
- **Config**: rows=1000000, dim=1536, k=10, nprobes=20, reps=20
- **Foyer cache**: RAM=16 MB, NVMe=256 MB (fresh tempdir per service build)
- **Harness**: `./scripts/cargo run --release -p firnflow-bench --bin first_query_profile`

## Seed

- Seeded this run: upsert 101.40 s, index 194.88 s, compact 54.67 s

## Per-case latency

| case | reps | p50 | p95 | p99 | min | max | s3_requests delta |
| ---- | ---:| ---:| ---:| ---:| ---:| ---:| ---: |
| cold-process | 20 | 719.47 ms | 1.06 s | 1.06 s | 639.59 ms | 1.06 s | 0 |
| warm-identical | 20 | 38.20 ms | 64.60 ms | 64.60 ms | 35.05 ms | 64.60 ms | 0 |
| warm-novel | 20 | 214.31 ms | 264.06 ms | 264.06 ms | 151.19 ms | 264.06 ms | 0 |
| dropped-handle | 20 | 690.44 ms | 752.74 ms | 752.74 ms | 644.01 ms | 752.74 ms | 0 |
| fresh-process | 20 | 664.53 ms | 779.43 ms | 779.43 ms | 589.05 ms | 779.43 ms | 0 |

## Case definitions

- **cold-process**: fresh manager + cache + service per rep, one query each.
- **warm-identical**: single warm service, repeated identical query (bypasses foyer).
- **warm-novel**: single warm service, novel query per rep.
- **dropped-handle**: evict pooled (Connection, Table) between reps; process stays warm.
- **fresh-process**: fresh manager + cache + service per rep, run AFTER all other cases so AWS SDK / process state is now warm (in-process variant only).

## Raw samples

- `cold-process` (20 reps): 1.06 s, 733.93 ms, 719.47 ms, 759.79 ms, 694.92 ms, 785.90 ms, 742.63 ms, 723.14 ms, 717.60 ms, 734.66 ms, 661.99 ms, 685.27 ms, 682.54 ms, 703.56 ms, 761.20 ms, 706.20 ms, 711.89 ms, 737.40 ms, 690.82 ms, 639.59 ms
- `warm-identical` (20 reps): 38.20 ms, 35.79 ms, 37.66 ms, 49.82 ms, 38.50 ms, 48.58 ms, 35.23 ms, 36.90 ms, 36.79 ms, 63.57 ms, 46.19 ms, 61.84 ms, 64.60 ms, 36.57 ms, 49.97 ms, 37.83 ms, 40.50 ms, 37.89 ms, 35.05 ms, 36.37 ms
- `warm-novel` (20 reps): 216.51 ms, 186.66 ms, 206.13 ms, 201.98 ms, 215.90 ms, 264.06 ms, 172.58 ms, 199.47 ms, 231.88 ms, 243.88 ms, 205.07 ms, 243.49 ms, 214.31 ms, 199.60 ms, 249.46 ms, 251.84 ms, 237.83 ms, 151.25 ms, 199.08 ms, 151.19 ms
- `dropped-handle` (20 reps): 702.57 ms, 685.76 ms, 663.93 ms, 652.97 ms, 690.44 ms, 752.74 ms, 672.03 ms, 734.38 ms, 644.01 ms, 748.08 ms, 726.97 ms, 674.05 ms, 695.61 ms, 678.96 ms, 697.52 ms, 675.53 ms, 702.70 ms, 662.67 ms, 705.99 ms, 653.18 ms
- `fresh-process` (20 reps): 648.64 ms, 647.14 ms, 665.82 ms, 664.53 ms, 639.98 ms, 648.64 ms, 645.57 ms, 671.97 ms, 709.54 ms, 610.22 ms, 769.98 ms, 681.29 ms, 741.21 ms, 669.13 ms, 663.86 ms, 779.43 ms, 685.25 ms, 617.80 ms, 589.05 ms, 615.38 ms

## Caveats

- **`cold-process` is a lower bound on a true fresh-process number.** Every repetition runs inside one binary invocation, so the AWS SDK HTTP client pool, TLS sessions, and the Tokio runtime persist across repetitions even though the in-process `NamespaceManager` + cache + service objects are rebuilt each rep. A true fresh-process measurement needs an outer driver that spawns a fresh binary per repetition. The `fresh-process` case runs after every other case so the difference vs `cold-process` is a coarse signal for SDK / connection warmup.
- **`s3_requests delta` only counts firnflow's service-boundary calls**, not raw `object_store` GETs / range GETs. For real S3 access-log attribution use the namespace prefix above as the path filter (primary) or the start/stop UTC window as a backstop.
- **`warm-identical` and `warm-novel` bypass the foyer result cache** by calling `NamespaceManager::query` directly. This isolates the LanceDB / index / handle-pool warm-state cost. A foyer hit would otherwise dominate the number and tell us nothing about the underlying object-store path.
