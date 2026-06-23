# BEIR multivector benchmark — raw results

This directory holds the raw per-run JSON behind every table in
[`../beir_multivector_objcache.md`](../beir_multivector_objcache.md), so the
report's numbers can be audited and reproduced rather than taken on trust. Each
file is the direct output of the benchmark harness for one run; the report
tables are assembled from these.

All runs are against real AWS S3 in `eu-west-1`, model `lightonai/LateOn`
(128-dim per-token vectors via PyLate), IVF_PQ unless noted, `k=100`. Firn
version is called out per group below.

## JSON shape

Quality / latency runs (`*_scores.json`):

```
dataset, namespace, k, nprobes, num_queries,
total_search_time_s, qps, latency_p50_ms, latency_p95_ms, latency_p99_ms,
scores: { map, ndcg@10, ndcg@100, recall@10, recall@100 }
```

Cache A/B and large-QPS runs additionally carry the cell label, the query split,
the concurrency, and a `/metrics` snapshot taken before and after the measured
pass:

```
cell, split, concurrency, wall_s, qps, latency_p50/p95/p99_ms,
counters_before / counters_after / counters_delta: {
  firnflow_object_cache_{hits,misses,inner_gets,s3_bytes,evictions}_total,
  firnflow_cache_hits_total,        # the result-cache guardrail: 0 in every measured cell
  firnflow_cache_misses_total,
  firnflow_s3_requests_total
}
```

Note the fiqa A/B cells pre-date the low-level
`firnflow_object_store_requests_total` / `firnflow_object_store_get_bytes_total`
counters, so the cache-*off* cells there have no backend byte figure of their own
— that gap is exactly what those counters were added to close. The NQ cache A/B
(`nq_cache_ab/`) was run with the instrumented build, so its cells carry the
always-on backend counters and have a true cache-*off* byte figure.

## Map: report table -> files

### §1 Retrieval quality (eight datasets) -> `quality_0.9.2/`

The published quality table. Every dataset scored on Firn `0.9.2`. The five
smaller sets and the two largest were loaded via `/import`; `fiqa` here is the
`/upsert` re-load on `0.9.2` (the single-fragment `/import` namespace scored the
same within rebuild jitter, 0.411, so the table value is representative).

| Dataset | file | ndcg@10 |
|---|---|---:|
| scifact | `quality_0.9.2/scifact_scores.json` | 0.7533 |
| nfcorpus | `quality_0.9.2/nfcorpus_scores.json` | 0.3763 |
| arguana | `quality_0.9.2/arguana_scores.json` | 0.5404 |
| scidocs | `quality_0.9.2/scidocs_scores.json` | 0.2100 |
| fiqa | `quality_0.9.2/fiqa_scores.json` | 0.4124 |
| trec-covid | `quality_0.9.2/trec-covid_scores.json` | 0.5794 |
| webis-touche2020 | `quality_0.9.2/webis-touche2020_scores.json` | 0.2682 |
| quora | `quality_0.9.2/quora_scores.json` | 0.8309 |

### Earlier 0.9.0-era runs (the index-recall caveat) -> `quality_0.9.0_superseded/`

The first sweep, before the table was standardised on `0.9.2`. Kept only because
the report's caveat compares two datasets across the reload (fiqa
0.4563 -> 0.4124, trec-covid 0.8367 -> 0.5794). **Not** part of the published
table — superseded by `quality_0.9.2/`. A like-for-like quality number must come
from one Firn version; do not mix these with the `0.9.2` set.

### §2 Object cache, the cache on/off A/B (fiqa) -> `cache_ab_fiqa/`

One single-fragment fiqa namespace, `0.9.2`, `nprobes=20`, `k=100`,
concurrency 32. The 648-query set is split into two disjoint halves: split A
warms, split B (324 queries) is measured. `firnflow_cache_hits_total` is 0 in
every measured cell (the `counters_delta` field) — proof the result cache never
served a measured query.

| Report cell | measured file | split-A warming file |
|---|---|---|
| cold-off | `cache_ab_fiqa/cold-off.json` | — |
| cold-on | `cache_ab_fiqa/cold-on.json` | — |
| warm-off | `cache_ab_fiqa/warm-off.json` | `warm-off-populate.json` |
| warm-on | `cache_ab_fiqa/warm-on.json` | `warm-on-populate.json` |
| warm-process | `cache_ab_fiqa/warm-process.json` | `warm-process-populate.json` |

Storage-footprint / eviction row (cache budget set to 256 MiB, below the working
set): `cache_ab_fiqa/evict-256MiB.json` (+ `evict-populate.json`). Hit ratio
falls to 29%, S3 bytes climb back toward the cache-off volume, evictions are
non-zero — the savings collapse when the budget cannot hold the working set.

### §2 Large-dataset QPS -> `large_dataset_qps/`

Warm measurement, cache on, result-cache hits at 0, across the larger corpora:
`quora-warm.json` (523k docs, short documents, 7.5 QPS) and `webis-warm.json`
(382k docs, long documents, 0.61 QPS), with their split-A `*-populate.json`
files. fiqa's row is the `warm-on` cell above.

### §2 nprobes sweep (fiqa, full 648-query) -> `nprobes_sweep_fiqa/`

`nprobes_{8,20,50,100}.json`. Quality is identical at every setting
(ndcg@10 0.4110). `nprobes_20.json` is the one run that recorded a sub-second
p50 / ~2x QPS; the report flags it as unreproduced (the warm-process cell and
the other nprobes runs all sit at ~17 s), so it is reported as an anomaly, not a
result.

### §2 Index configuration sweep (fiqa) -> `index_config_sweep_fiqa/`

`sub{32,64}_{4,8}bit.json` = `num_sub_vectors` x `num_bits`. ndcg@10: 32/8 0.4212,
64/8 0.4106, 64/4 0.4129, 32/4 0.3922.

### §2 Object cache at 1M scale (NQ) -> `nq_cache_ab/`

The cache A/B repeated on `beir-nq` (1,000,000 multivector documents, single
fragment, IVF_PQ `num_sub_vectors=64`/`num_bits=8`, `nprobes=20`, `k=100`,
concurrency 32, 500 measured queries per cell). Run on the instrumented build, so
every cell carries the always-on backend counters
(`firnflow_object_store_requests_total` / `_get_bytes_total`) — which is what
gives the cache-*off* cells a real backend byte figure that the fiqa A/B could not
produce.

| Report cell | measured file | split-A warming file |
|---|---|---|
| cold-off | `nq_cache_ab/cold-off.json` | — |
| cold-on | `nq_cache_ab/cold-on.json` | — |
| warm-off | `nq_cache_ab/warm-off.json` | `warm-off-populate.json` |
| warm-on | `nq_cache_ab/warm-on.json` | `warm-on-populate.json` |

Headline: for the same 500 queries, cache-off (`warm-off`) reads **361 GB** from
S3 vs cache-on warm (`warm-on`) **2.79 GB** — a ~130x byte reduction, read
directly off `firnflow_object_store_get_bytes_total` on both arms. Latency is flat
(~46-48 s p50) across all four cells; `firnflow_cache_hits_total` (the
result-cache guardrail) is 0 in every cell. `uncontended-probe.json` is a 10-query
probe on an already-warmed process at concurrency 32 (so no queueing): it records
the ~14.5 s *single-query* latency that the ~46 s saturated cells sit on top of —
supplementary, not a headline cell.

### Caveats — exact vs IVF_PQ across corpus sizes -> `../fiqa_exact_vs_indexed/` + `exact_vs_indexed_variance/`

The exact-vs-indexed measurement behind the index-recall conclusion spans three
corpus sizes. fiqa is committed separately at
[`../fiqa_exact_vs_indexed/`](../fiqa_exact_vs_indexed/): `fiqa_brute.json` (exact
MaxSim, ndcg@10 0.5264) vs `fiqa_indexed.json` (IVF_PQ 64/8, 0.4084), full
648-query set. The two smaller corpora are in `exact_vs_indexed_variance/`, same
`nprobes=20` / `k=100` / `0.9.2` / real S3 setup:

| dataset | docs | exact (file) | indexed (file) | indexed ndcg@10 |
|---|---:|---|---|---:|
| arguana | 8.7k | `arguana_exact.json` 0.5095 | `arguana_indexed_run{1,2,3}.json` | 0.5413 / 0.5381 / 0.5417 |
| scidocs | 25k | `scidocs_exact.json` 0.2180 | `scidocs_indexed_run1.json` | 0.2107 |
| fiqa | 57k | `../fiqa_exact_vs_indexed/fiqa_brute.json` 0.5264 | `../fiqa_exact_vs_indexed/fiqa_indexed.json` | 0.4084 |

arguana carries three index rebuilds to show the run-to-run jitter (~0.004) is far
smaller than the exact-vs-indexed gap. The trend: indexed matches (even slightly
exceeds) exact at 8.7k, is within ~3% at 25k, and falls ~22% at 57k — the loss
scales with corpus/index size, it is not a per-dataset effect.
