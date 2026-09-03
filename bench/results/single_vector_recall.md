# Single-vector recall against a full scan

- **Date**: 2026-08-21 (1,000,000 rows), 2026-08-29 to 2026-09-01 (100,000 rows)
- **Harness**: [`bench/recall/`](../recall/README.md)
- **Validated Firn image**: `ghcr.io/gordonmurray/firnflow@sha256:4c2b7a58df687423cc1db9b83a541c4b776fefa63d3a73a05850c34173416e19` (version 0.9.5)
- **Backend**: MinIO on loopback
- **Corpus**: `CohereLabs/wikipedia-2023-11-embed-multilingual-v3` English split, dim=1024, at revision `ade45fb52bd549f5e8c065636fe4160a43c2af36`. The repository was previously named `Cohere/...` and the old name still redirects. Two namespaces: 1,000,000 rows and 100,000 rows. Shard checksums are pinned in `bench/recall/corpus.py` and verified before every import
- **Index**: IVF_PQ with no tuning options passed. The validated 100,000-row index reported L2 distance, 100,000 indexed rows, no unindexed rows and one IVF_PQ index. Source defaults give 12 partitions, 64 sub-vectors and 8-bit product quantization
- **Queries**: held-out vectors from a shard that was never loaded, `k=10`, `include_vector: false`. Indexed runs use 200 queries. The unindexed control uses 20
- **Raw data**: [`single_vector_recall_raw/`](single_vector_recall_raw/)
- **Validation environment**: [`validation_environment_100k.json`](single_vector_recall_raw/validation_environment_100k.json)

## What recall@10 is here

There are two ways to answer "which rows are closest to this vector".

The first compares the query against every row in the table and keeps
the closest ten. That is the correct answer by definition.

The second is the index. It searches only some of the partitions, and it
stores each vector in a compressed form. Both shortcuts can change which
rows come back.

Recall@10 counts how many of the index's ten results also appear in the
exact top ten, divided by ten. A value of 1.0 means the index agreed
with the exact scan. A value of 0.5 means half the rows it returned are
not among the true ten nearest.

This is not the `recall@10` in the BEIR reports in this directory. That
one asks whether a human labelled the returned document relevant. An
index can return a different relevant document, score well on relevance,
and still return the wrong rows.

## How many partitions there are

`nprobes` is the number of index partitions searched per query. The
server default is 20, from `DEFAULT_NPROBES` in
`crates/firnflow-core/src/query.rs`. What that setting can buy depends
on how many partitions the index holds, so that number comes first.

Firn passes no `num_partitions` when it creates an index. LanceDB 0.29
then takes the Lance 6 default, which aims for 8,192 rows in each
IVF_PQ partition and divides the row count by that, with a floor of 1
and a ceiling of 4,096.

**That is 12 partitions over 100,000 rows and 122 over 1,000,000.**

So the default `nprobes` of 20 searches every partition of the
100,000-row index, and 20 of the 122 in the million-row index. A value
above the partition count is capped by it. Asking for 1,000 partitions
of an index that holds 12 searches those 12 and stops.

The default is two steps of library code:

- LanceDB 0.29 falls through to `IvfBuildParams::default()` when
  `num_partitions` is unset:
  [`rust/lancedb/src/table.rs`](https://github.com/lancedb/lancedb/blob/v0.29.0/rust/lancedb/src/table.rs#L1820-L1837)
- Lance 6 gives IVF_PQ a target partition size of 8,192 rows:
  [`rust/lance-index/src/lib.rs`](https://github.com/lance-format/lance/blob/v6.0.0/rust/lance-index/src/lib.rs#L295-L313)
- and turns that into a count with
  `(num_rows / target).clamp(1, 4096)`:
  [`rust/lance-index/src/vector/ivf/builder.rs`](https://github.com/lance-format/lance/blob/v6.0.0/rust/lance-index/src/vector/ivf/builder.rs#L115-L120)

The sweeps below include a 316 setting. It is above the partition count
at both corpus sizes, so it searches the whole index.

## Maintainer-validated 100k results

The final validation started with no vector index. Twenty held-out
queries went through Firn's full-scan path and matched the independently
computed exact top ten for every query:

| path | queries | recall@10 | p50 | p95 | cache hits |
| ---- | ------: | --------: | --: | --: | ---------: |
| unindexed full scan | 20 | 1.0000 | 528.41 ms | 659.89 ms | 0 |

This control checks the held-out queries, normalization, distance
ordering and row-id mapping end to end. Its raw output is in
[`flat_scan_100k_validation.json`](single_vector_recall_raw/flat_scan_100k_validation.json).

The same unchanged namespace was then indexed three times with the
default IVF_PQ configuration. Each build was scored at Firn's default
`nprobes` of 20 over the same 200 queries:

| build | recall@10 | p50 | p95 | cache hits |
| ----: | --------: | --: | --: | ---------: |
| 1 | 0.5880 | 18.77 ms | 22.84 ms | 0 |
| 2 | 0.5925 | 14.25 ms | 16.85 ms | 0 |
| 3 | 0.5935 | 16.04 ms | 20.15 ms | 0 |

Recall spans 0.5880 to 0.5935, a spread of 0.55 percentage points.
The raw build data is in
[`build_repeat_100k_validation.json`](single_vector_recall_raw/build_repeat_100k_validation.json).

The third build was swept across three orders of magnitude:

| nprobes | recall@10 | p50 | p95 | cache hits |
| ------- | --------: | --: | --: | ---------: |
| 1 | 0.4305 | 9.70 ms | 12.64 ms | 0 |
| 2 | 0.4975 | 8.87 ms | 11.22 ms | 0 |
| 5 | 0.5795 | 10.76 ms | 13.80 ms | 0 |
| 10 | 0.5935 | 15.05 ms | 18.36 ms | 0 |
| **20** (default) | **0.5935** | **16.04 ms** | **20.15 ms** | **0** |
| 50 | 0.5935 | 15.60 ms | 18.50 ms | 0 |
| 100 | 0.5935 | 15.63 ms | 18.39 ms | 0 |
| 316 | 0.5935 | 15.96 ms | 20.21 ms | 0 |
| 1000 | 0.5935 | 16.42 ms | 20.13 ms | 0 |

The low settings are the control for `nprobes`: changing the setting
changes both answers and latency. Recall stops moving at 10, just before
the 12-partition index is fully searched. Searching all partitions does
not recover the missing neighbours. The raw sweep is in
[`nprobes_exhaustive_100k_validation.json`](single_vector_recall_raw/nprobes_exhaustive_100k_validation.json).

The first query shows the plateau without averaging. From `nprobes` 5
through 1000, every setting returned the same ten ids and six matched
the exact top ten:

```
nprobes     1  found 5/10  [3783, 3793, 3794, 3780, 3779, ...]
nprobes     2  found 5/10  [3783, 3793, 3794, 3780, 3779, ...]
nprobes     5  found 6/10  [3783, 3793, 3794, 3780, 3779, ...]
nprobes  1000  found 6/10  [3783, 3793, 3794, 3780, 3779, ...]
exact                      [3777, 3793, 3783, 3779, 3786, ...]
```

Full lists are in
[`query0_ids_by_nprobes_100k_validation.json`](single_vector_recall_raw/query0_ids_by_nprobes_100k_validation.json).
Rows 3777, 3867, 25697 and 3807 are among the true ten nearest and are
returned at no setting.

## Historical 1M results

The original million-row run is retained because it shows the same
direction at a larger corpus size:

| nprobes | recall@10 | p50 | p95 |
| ------- | --------: | --: | --: |
| 10 | 0.536 | 4.63 ms | 11.03 ms |
| **20** (default) | **0.559** | **4.72 ms** | **5.82 ms** |
| 50 | 0.5725 | 9.86 ms | 10.89 ms |
| 100 | 0.575 | 18.47 ms | 18.77 ms |

These measurements predate the final output-file guard, environment
record and cache-hit rejection. They are directional evidence only.
Their latency must not be compared with the validated 100,000-row run,
which used different hardware and container networking.

## Why more probing does not help

The 100,000-row index holds 12 partitions. Every setting from 12 upward
searches all of them. Recall and latency stop moving at that boundary,
which is where the index runs out of partitions to search.

## What that rules out, and what it does not

At `nprobes` 20 the 100,000-row index searches all 12 of its partitions.
Every row in the table is a candidate and every row is scored. Nothing
is left out by the choice of partitions, and recall is still well short
of 1.0.

That is not the same as a full scan. A full scan compares the query
against the stored 4,096-byte vectors and returns the exact answer, by
definition. Searching every partition of this index compares the query
against 64-byte approximations of those vectors. The candidate set is
the whole table either way. Only the scoring differs, so only the
scoring can account for the gap.

So the cause is the compression. Distances computed from 64 bytes are
close enough to gather a plausible set of candidates and too coarse to
order them correctly. `nprobes` chooses which candidates are considered,
not how they are scored, and no setting of it corrects a scoring error.

Two things this does not settle.

It does not measure whether a re-scoring pass against the stored
full-precision vectors would recover the missing rows. That pass is not
part of this measurement.

It does not carry to the million-row index. That one holds 122
partitions and the default `nprobes` searches 20 of them, so a narrow
search and a coarse score are both in play there and this run does not
separate them.

## A cheaper way to see the same thing

The million-row run takes a few hours end to end. The 100,000-row run
above takes a few minutes: one shard to download, one shard to load, one
index build, one ground-truth scan.

That smaller run carries the same finding. Recall at 100,000 rows is
0.5880 to 0.5935 across three builds, against 0.559 over a million rows
at the same setting. The flat response to `nprobes` is already complete
there, and so is the per-query evidence that specific true neighbours
are never returned at any setting.

## Superseded 100k runs

The raw directory retains earlier 100,000-row runs for auditability.
One unreproducible build reached 0.6945 Recall@10. Its environment and
index state were not recorded well enough to explain the difference.
No conclusion in this report uses that run, its latency or its id list.

The maintained result is the 2026-09-01 validation above. It includes a
fresh full-scan control, three fresh index builds, a new exhaustive
sweep, fresh per-query ids, exact dependency pins, result checksums and
a machine-readable environment record.

## Limits

- MinIO on loopback, not a real object store. The latency columns are
  lower than they would be against S3. The recall column does not depend
  on the storage backend.
- One dataset, two corpus sizes, one index configuration. Nothing here
  varies `num_partitions`, `num_sub_vectors` or `num_bits`. Three builds
  of that one configuration were measured; every other figure in this
  file comes from a single build.
- Single-vector namespaces only. Nothing here touches the multivector
  path measured in `beir_multivector_objcache.md`.
- Single-client, sequential queries. This is not a throughput
  measurement.
- The validated run used an Intel i7-13700H system with 20 logical CPUs
  and 15.9 GB RAM. The server, MinIO and harness ran on the same machine.
- Runs on different hardware are not latency comparisons. The recall
  figures within each run are.

## refine_factor recall improvement (2026-09-03)

- **Firn version**: v0.9.7
- **Setup**: shard 0 corpus (100,000 rows), shard 1 query vectors (200 queries), k=10, nprobes=20, one index build
- **Note**: different query shard from the validated runs above (those used shard 10). The baseline here is one build and cannot be averaged with the 0.5880-0.5935 range. The refine_factor columns are the new measurement.
- **Raw data**: [`single_vector_recall_raw/refine_factor_sweep_100k.json`](single_vector_recall_raw/refine_factor_sweep_100k.json), [`single_vector_recall_raw/refine_factor_baseline_100k.json`](single_vector_recall_raw/refine_factor_baseline_100k.json)

`refine_factor` was added in v0.9.7. After the IVF_PQ search the server fetches `refine_factor * k` candidates, re-scores them against the full stored vectors, and returns the true top-k from that re-scored set. At 100,000 rows with 12 partitions and nprobes=20, the index already searches every partition, so every row is a candidate. The remaining recall gap is in the PQ scoring step. Re-scoring against full-precision vectors closes most of it.

| refine_factor | Recall@10 | p50 ms | p95 ms | queries | cache_hits |
| ------------: | --------: | -----: | -----: | ------: | ---------: |
| 0 (baseline)  |     0.637 |  21.11 |  25.97 |     200 |          0 |
| 5             |     0.953 |  40.94 |  53.67 |     200 |          0 |
| 10            |     0.990 |  53.62 |  69.41 |     200 |          0 |
| 20            |    0.9985 |  73.62 |  93.49 |     200 |          0 |

`refine_factor=10` takes recall from 0.637 to 0.990 at roughly 2.5x the query latency. `refine_factor=20` reaches 0.9985 at 3.5x. The baseline of 0.637 is from a single build with a different query shard; the validated range of 0.5880-0.5935 from three builds earlier in this file is the more reliable baseline figure for IVF_PQ-only recall.

The harness script for this sweep is [`bench/recall/refine_factor_sweep.py`](../recall/refine_factor_sweep.py). It accepts a comma-separated list of refine_factor values and follows the same cache-hit rejection and response-integrity checks as `recall_sweep.py`.

## Reproducing

[`bench/recall/README.md`](../recall/README.md) has the full procedure:
download the shards, load ten of them, build the index, compute the
exact answers from the eleventh, and score the index against them.

The embeddings ship pre-computed with the dataset, so the run needs no
embedding model and no API key.
