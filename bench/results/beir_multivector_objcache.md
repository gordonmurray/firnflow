# Firn multivector retrieval on object storage: BEIR quality, bulk import, and the object cache

This report measures Firn's late-interaction (multivector) search path against
real AWS S3: retrieval quality across eight BEIR datasets, the bulk-import path
for large first loads, and what the on-disk object cache actually does for query
cost and latency.

## Background (for any reader)

- **Firn** (`firnflow`) is a vector and full-text search engine that keeps its
  data on object storage (S3 / MinIO / R2 / GCS) and puts a RAM + local-NVMe
  cache in front of it. The point is to run search directly on cheap, elastic
  object storage instead of a fleet of always-on disks.
- **BEIR** is a standard benchmark suite for retrieval: a set of datasets, each
  with a document corpus, a query set, and human relevance judgments, used to
  compare search systems on the same footing. Scores here are nDCG and Recall at
  cut-offs 10 and 100, plus MAP.
- **Late-interaction / multivector retrieval** (ColBERT-style): instead of one
  vector per document, the model produces *one vector per token*, so a document
  is a small bag of vectors. A query scores against a document by taking, for
  each query token, its best match among that document's vectors, then summing
  those best matches (this is "MaxSim"). It is more expressive than single-vector
  search, at the cost of more compute and more storage per document.
- The model is `lightonai/LateOn`, run through [PyLate](https://github.com/lightonai/pylate).

## Setup

- **Model / encoding:** LateOn (ColBERT-style late interaction), 128-dim
  per-token vectors, via PyLate. One vector per token, so the per-document vector
  count varies by dataset (dataset means range ~16 to ~237; see §1/§2).
- **Engine:** Firn `0.9.2`. Storage on **real AWS S3** in `eu-west-1` (not
  loopback MinIO). All eight quality datasets were loaded and scored on this one
  version via `/import`, so the quality table is single-version and internally
  consistent. (An earlier draft mixed `0.9.0` and `0.9.2` numbers; the small
  datasets were re-run on `0.9.2` to remove that confound — see Caveats.)
- **Index / query:** IVF_PQ vector index, `num_sub_vectors=64`, `num_bits=8`
  (default), queried at `nprobes=20`, `k=100`. Late-interaction (MaxSim) scoring.
- **Object cache:** read-through byte-range cache on local NVMe, 100 GiB budget,
  enabled with `FIRNFLOW_OBJECT_CACHE_ENABLED=true`.
- **Host:** all numbers reported here were measured on a single 32-vCPU box
  (g4dn.8xlarge, 1×T4) in `eu-west-1`. Quality is host-independent; latency/QPS
  are CPU-bound, so treat the absolute QPS as host-specific (see Caveats).

## 1. Retrieval quality (eight BEIR datasets)

All eight datasets were loaded and scored on a single Firn version (`0.9.2`) via
`/import`, so the table is internally consistent. The `vec/doc` column is the mean
per-document vector count (one vector per token); it is the main driver of query
*latency* (§2), but, as it turns out, not of the quality differences (see Caveats).

| Dataset | docs | queries | vec/doc | ndcg@10 | ndcg@100 | recall@10 | recall@100 | map |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| scifact | 5.2k | 300 | n/a | 0.7533 | 0.7700 | 0.9036 | 0.9767 | 0.7046 |
| nfcorpus | 3.6k | 323 | 237 | 0.3763 | 0.3367 | 0.1800 | 0.3225 | 0.1807 |
| arguana | 8.7k | 1406 | 177 | 0.5404 | 0.5771 | 0.8065 | 0.9687 | 0.4656 |
| scidocs | 25k | 1000 | 188 | 0.2100 | 0.2855 | 0.2196 | 0.4388 | 0.1478 |
| fiqa | 57k | 648 | 134 | 0.4124 | 0.4752 | 0.4705 | 0.7076 | 0.3555 |
| trec-covid | 171k | 50 | 170 | 0.5794 | 0.4564 | 0.0162 | 0.1171 | 0.0784 |
| webis-touche2020 | 382,545 | 49 | 153 | 0.2682 | 0.3493 | 0.1645 | 0.4139 | 0.1586 |
| quora | 522,931 | 10000 | 16 | 0.8309 | 0.8497 | 0.9196 | 0.9873 | 0.7939 |

(`vec/doc` is the mean over each corpus, computed from the encoded embeddings on
S3; scifact's embeddings were not uploaded to S3, so its mean is `n/a` here.)

The scores track each task's known difficulty: high on scifact and quora, low on
the genuinely hard ones (scidocs, webis-touche2020 argument retrieval,
trec-covid where each query has many relevant documents so a top-k list cannot
cover them). On the smaller datasets the pipeline calibrates cleanly against
published ColBERT-style numbers: scifact (0.7533) and nfcorpus (0.3763) line up
with the PLAID baselines (SciFact ~0.766, NFCorpus ~0.378). The two largest-corpus
datasets with a like-for-like history (fiqa, 57k docs, and trec-covid, 171k) scored
below their earlier 0.9.0 numbers (fiqa 0.4563→0.4124, trec-covid 0.8367→0.5794); a
direct exact-vs-indexed check on fiqa shows the **IVF_PQ index itself** is the main
cause, giving up ~22% of ndcg@10 versus exact (un-indexed) MaxSim search (see
Caveats). This is an index-recall effect, not
a document-length one: per-document vector counts do not predict it (nfcorpus has
the most vectors per document of any set here, yet is unaffected).

**Calibration against published baselines.** For the two datasets with external
reference points:

| dataset | PLAID nDCG@10 | earlier Firn (0.7.1 repro) | this run (0.9.2) |
|---|---:|---:|---:|
| scifact | 0.7661 | 0.7575 | 0.7533 |
| nfcorpus | 0.3779 | n/a | 0.3763 |

This run matches the earlier Firn SciFact reproduction (0.7575 on 0.7.1 → 0.7533
here) and lands within ~1–2% of the PLAID baselines on both, so quality holds on the
control datasets. (NFCorpus has no earlier Firn number — the prior SciFact gists
were SciFact-only. PLAID figures are the published ColBERT-style BEIR numbers
Antoine cited: SciFact 76.61, NFCorpus 37.79.)

## 2. The object cache: a storage-cost win, not a latency win (for this workload)

This is the question that matters for an object-storage-backed engine: when you
run real, novel queries against data on S3, does the local NVMe cache help, and
is the measurement honest (not a result already sitting fully warm in a result
cache)?

**Method.** One host, one single-fragment fiqa namespace, IVF_PQ, `nprobes=20`,
`k=100`, concurrency 32. The query set is split in two disjoint halves: split A
warms the cache, split B (324 novel queries) is measured. Because B is disjoint
from A, B never hits Firn's exact-result cache, so this measures real query work,
not a cache replay. **The exact-result-cache hit counter was 0 in every measured
cell** (the guardrail). Four cache cells plus a warm-process cell:

| cell | object cache | process | QPS | p50 | p95 | obj-cache hits / misses / S3 bytes |
|---|---|---|---:|---:|---:|---|
| cold-off | off | cold | 1.80 | 16.9 s | 21.6 s | — (cache off) |
| cold-on | on, empty | cold | 1.81 | 17.1 s | 25.1 s | 16,884 / 20,937 / 1.99 GB |
| warm-off | off | cold | 1.77 | 17.5 s | 22.9 s | — (cache off) |
| warm-on | on, warm | cold | 1.77 | 17.6 s | 25.4 s | 27,810 / 9,831 / 0.69 GB |
| warm-process | on, warm | warm | 1.83 | 16.8 s | 25.6 s | 27,649 / 9,832 / 0.69 GB |

**Reading the cells.** "off" / "on" is the object cache; "cold" / "warm" is the
*procedure*. Cold cells measure on a fresh process; warm cells first run split A
and then restart Firn before measuring split B. So `warm-off` runs the identical
warm procedure as `warm-on` but with the cache off: it is the control that
isolates the object cache as the only difference between the two, and its "warm"
refers to the procedure, not a warmed cache (the cache is off). `warm-process` is
the one cell that does *not* restart before measuring, so the process itself
stays warm.

**The cache works at the I/O layer.** The robust signal is bytes fetched from S3:
for the same 324 queries it dropped from 1.99 GB (cold-on) to 0.69 GB (warm-on), a
~3× reduction. In counter terms, `misses` (cacheable reads that were fetched and
admitted to the cache) fell from 20,937 to 9,831 while `hits` rose to 27,810, so in
the warm pass ~74% of cacheable reads were served from NVMe. (The true backend-read
counter is `inner_gets` = misses + a small number of uncacheable passthroughs;
cold-on recorded `inner_gets`=21,001 vs `misses`=20,937, i.e. ~64 passthroughs, so
the two are within ~0.3% here.) The cold cells confirm the working set genuinely
hits storage: cold-on made ~21k backend GETs (`inner_gets`=21,001) pulling ~2 GB.

**What the ~3× is and is not.** This figure is the *instrumented cold-to-warm byte
reuse with the cache on*: cold-on starts with an empty cache, so its 1.99 GB is the
first-touch volume, and warm-on serves most of it from NVMe on the second pass
(0.69 GB). The object-cache byte counters only exist when the cache is enabled, so
there is no literal cache-*off* byte arm to difference against; cold-on's empty-cache
pass is the closest equivalent to the cache-off volume. The clean cache-off vs
cache-on comparison is the **latency** A/B (warm-off vs warm-on), and that is flat
(~17 s both), which is the actual finding: the cache reuses storage reads, it does
not speed up this workload.

**Why latency is flat.** Every cell lands at ~17 s p50 regardless of cache
state, and a warm process makes no difference either. For this multivector
workload, latency is bound by the CPU cost of MaxSim scoring, not by storage
I/O, so the object cache reduces S3 access (and therefore cost) without speeding
up queries. This is the opposite of single-vector search, which is I/O-bound and
does see large cache speed-ups (Firn's single-vector first-query profile showed
roughly 30× on warm-novel queries). The honest summary: **on multivector, the
object cache is a storage-cost lever, not a latency lever.**

**`nprobes` is flat at this scale.** Sweeping `nprobes` over 8, 20, 50, 100 on
fiqa (full 648-query set) gave **identical quality** (ndcg@10 0.4110 at every
setting) and ~17 s latency throughout. So `nprobes=20` is a fine default here;
probing more partitions neither helps quality nor changes the CPU-bound latency.
This matches the earlier small-scale (SciFact) finding rather than shifting with
dataset size, at least up to fiqa's 57k documents.

**One honesty note.** A single run (nprobes=20 on a heavily-warmed process)
recorded ~2× throughput and a sub-second p50. A controlled re-test (the
warm-process cell, and nprobes 50/100 on the same warm process) did not
reproduce it: all six controlled measurements sit at ~17 s. We report the
consistent ~1.8 QPS and flag that single fast run as unexplained rather than
featuring it.

### Storage footprint (cache size vs data size)

A fair cache benchmark also needs the data-to-cache ratio. On S3, each
single-fragment `/import` namespace:

| dataset | docs | namespace on S3 | IVF_PQ index | object-cache budget |
|---|---:|---:|---:|---:|
| fiqa | 57k | 4.45 GB | 0.51 GB | 100 GiB |
| quora | 523k | 4.79 GB | 0.55 GB | 100 GiB |
| webis-touche2020 | 382k | 34.0 GB | 3.89 GB | 100 GiB |

Every namespace fits inside the 100 GiB cache budget with room to spare, and the
main A/B recorded 0 evictions. So the headline cache numbers are the
**no-eviction (capacity-resident)** regime: the budget exceeds the working set, so
nothing is forced out. (Within a single warm pass the cache is still filling — the
warm-on run had 9,831 misses and fetched 0.69 GB — so "resident" means it can hold
the working set, not that every read hit.)

To exercise the opposite regime, we re-ran the warm fiqa A/B with the cache
budget set to **256 MiB**, well below the ~0.69 GB a single pass reads:

| cache budget | obj-cache hit ratio | S3 bytes (324 queries) | evictions | p50 |
|---|---:|---:|---:|---:|
| 100 GiB (fits) | 74% | 0.69 GB | 0 | 17.6 s |
| 256 MiB (thrashes) | 29% | 1.90 GB | 26,660 | 16.9 s |

Under-sizing the cache below the working set collapses the storage savings: the
hit ratio falls from 74% to 29%, evictions climb, and S3 bytes return to ~1.9 GB,
close to the cache-off baseline of ~2 GB. Query latency is unchanged (~17 s)
because it was never I/O-bound. So the object cache's ~3× S3 reduction depends on
sizing it to hold the working set; sized below it, the cache thrashes and the
storage savings largely disappear, while latency (CPU-bound) is unaffected either
way.

### Query latency at scale: mostly document length, corpus size second

The same warm measurement procedure (cache on, result-cache hits at 0) run across
quora, fiqa, and webis, with the mean per-document vector count (computed from
the encoded embeddings — one vector per token):

| dataset | docs | vectors/doc (mean, median) | p50 | p95 | QPS |
|---|---:|---:|---:|---:|---:|
| quora | 523k | 16 (14) | 4.2 s | 5.8 s | 7.5 |
| fiqa | 57k | 134 (108) | 17.0 s | 25.1 s | 1.8 |
| webis-touche2020 | 382k | 153 (145) | 35.2 s | 39.0 s | 0.61 |

The dominant factor is document length — the per-document vector count the MaxSim
step has to score. Quora has by far the *most* documents (523k) yet is the
*fastest* (4.2 s), because its documents are short (~16 vectors each), so corpus
size on its own is clearly not the driver. Corpus size is a secondary factor:
fiqa and webis have similar per-document vector counts (134 vs 153) but webis is
~2× slower with a ~7× larger corpus, so the larger candidate set the index
returns adds on top of the per-document cost. So the QPS lever is document length
first, candidate-set size second; raw row count on its own is not it. All three
fit in the cache with 0 evictions.

### Index configuration (`num_sub_vectors`, `num_bits`)

Sweeping the IVF_PQ index on fiqa (rebuild + measure quality at `nprobes=20`):

| num_sub_vectors | num_bits | ndcg@10 |
|---:|---:|---:|
| 32 | 8 | 0.4212 |
| 64 | 8 | 0.4106 |
| 64 | 4 | 0.4129 |
| 32 | 4 | 0.3922 |

4-bit *can* cost quality, but in this sweep it only clearly did with fewer
sub-vectors: 32/4 falls to 0.392, while 64/4 (0.4129) was on par with 64/8
(0.4106). 4-bit roughly halves per-vector index storage, so it trades some quality
for size and the size win is most worthwhile when the quality cost is small (the
64 case here). `num_sub_vectors=32` edges out 64 at 8-bit. The
default `num_sub_vectors=64`, `num_bits=8` is a safe choice; 32/8 is marginally
better on quality here, and 4-bit is worth it only when index storage is the
constraint.

## 3. Bulk ingest at scale: `/import`

The per-batch `/upsert` path does not scale to large multivector corpora: each
batch is its own storage commit, and the per-commit and small-fragment
bookkeeping makes throughput decay as the namespace grows. On webis-touche2020
(382k documents) it sustained only ~2–6 docs/s and falling, on track for roughly
**30 hours** for the full corpus.

The `/import` endpoint (added in 0.9.2) sends the whole corpus as one Arrow IPC
stream, appended in a **single commit**, binary and columnar so the embeddings
avoid JSON's ~3× decimal-text inflation. The default `/import` size cap is 8 GiB
(`FIRNFLOW_IMPORT_MAX_BYTES`), so the 30 GB webis stream needs the cap raised; these
runs set `FIRNFLOW_IMPORT_MAX_BYTES=0` to disable it:

| dataset | docs | Arrow stream | server-side import | rate |
|---|---:|---:|---:|---:|
| webis-touche2020 | 382,545 | 30 GB | 135 s (one commit) | ~2,830 docs/s |
| quora | 522,931 | 4.2 GB | 35 s (one commit) | ~14,900 docs/s |

Both landed as a single Lance fragment. That is roughly three orders of magnitude
faster than the per-batch path, and it is what makes a multi-hundred-thousand-
document first load practical at all.

## 4. Practical guidance

- For large multivector first loads, use `/import` (single commit), not
  per-batch `/upsert`.
- The object cache's value on multivector search is cutting S3 request and byte
  cost (~3× cold-to-warm byte reuse with the cache on, measured on fiqa); it does
  not lower multivector query latency, which is CPU-bound. For genuinely I/O-bound
  workloads (single-vector, or an index that does not fit in RAM) the cache also
  cuts latency.
- `nprobes=20` is a reasonable default at this scale (quality flat from 8 to 100).
  To improve multivector QPS, the lever is the MaxSim / candidate-set cost
  (centroid pruning before full scoring, scoring-path parallelism), not the cache
  or `nprobes`.
- **Multivector QPS is set mostly by document length, then corpus size.** A large
  corpus of short documents is fast (quora, 523k, ~16 vectors/doc, 4.2 s p50);
  longer documents and a larger candidate set both slow it (webis, ~154
  vectors/doc, 35 s). Plan capacity around document length and candidate-set size,
  not row count alone.
- **Size the object cache to hold the working set.** Its ~3× S3 saving needs
  `FIRNFLOW_OBJECT_CACHE_BYTES` to cover the index byte-ranges plus the candidate
  vectors a query reads; below that the cache thrashes and the saving disappears.
- **Index config:** `num_sub_vectors=64`, `num_bits=8` is a safe default.
  `num_bits=4` roughly halves index storage; in our fiqa sweep its quality cost
  was clear only at fewer sub-vectors (32/4 dropped to 0.392, while 64/4 at 0.4129
  was on par with 64/8 at 0.4106), so it *can* cost quality — validate the recall
  cost per corpus before using it.
- **At this scale, measure exact (un-indexed) search before defaulting to IVF_PQ.**
  On fiqa the IVF_PQ index cost ~22% of ndcg@10 for no latency benefit (exact and
  indexed per-query p50 within ~8%); exact retrieved materially better. The index
  earns its keep once the corpus is large enough that an exact scan is prohibitive,
  so for moderate-scale multivector it is worth comparing exact vs indexed rather
  than assuming the index is free (see Caveats).

## Caveats

- **Latency / QPS are host-specific.** All measurements here are from one 32-vCPU
  box; MaxSim scoring is CPU-bound, so the absolute QPS scales with core count and
  is not portable. Quality is host-independent.
- **Multivector QPS is modest (~1.8 on this box) and CPU-bound.** These numbers
  are not a tuned latency result; they characterise where the time goes.
- **The IVF_PQ index loses significant recall on fiqa (measured); the broader
  pattern is not fully pinned down.** A direct exact-vs-indexed check on fiqa
  (identical data, full 648-query set, `nprobes=20`, `k=100`) measured **exact
  MaxSim ndcg@10 0.5264 (recall@10 0.609) vs IVF_PQ 0.4084 (recall@10 0.468) — the
  index gives up ~22% of ndcg@10 and ~23% of recall@10**, at no latency benefit
  (exact vs indexed per-query p50 18.4 s vs 19.9 s). Exact (0.5264) is above *both*
  earlier indexed fiqa runs (a 0.9.0 load at 0.4563 and a 0.9.2 re-load at ~0.41),
  so those were just different lossy IVF_PQ training draws under the exact ceiling,
  not a version regression (both versions pin the same vector-index stack). Index
  parameters do not rescue it (`num_sub_vectors` / `num_bits` moved fiqa only within
  0.392–0.421; `nprobes` 8–100 made no difference). **This exact-vs-indexed test was
  run only on fiqa.** Across the suite, the two datasets that fell across the
  0.9.0→0.9.2 reload (fiqa 0.4563→0.4124, trec-covid 0.8367→0.5794) are the two
  largest corpora with a like-for-like baseline, while the smaller sets (≤25k docs)
  are stable (within ±0.006). It does **not** track
  document length: per-document vector counts (nfcorpus 237, scidocs 188, arguana
  177, trec-covid 170, fiqa 134) put the *stable* nfcorpus above the *affected*
  fiqa, so an earlier "long-document" framing was wrong. The likely common cause is
  IVF_PQ recall loss that worsens with corpus/index size, but it is not isolated
  per-dataset and is a follow-up. The id-mapping was audited and is sound, so this
  is an index-level effect, not a harness artifact. **Practical takeaway: at this
  scale, exact (un-indexed) multivector search can retrieve materially better than
  IVF_PQ for no extra latency (shown on fiqa); the table reports the default
  IVF_PQ-indexed numbers.** The cache and latency results in §2/§3 compare a
  namespace against itself, so they are unaffected by this.
- **The cache figures are for a single-fragment namespace** (the recommended
  state after `/import`). A heavily fragmented namespace would show a larger
  cache effect, but that is the anti-pattern `/import` exists to avoid.

## Reusable artifact

Encoded LateOn embeddings for seven of the eight datasets (all except scifact) are
on S3, so future runs (and CI) can skip GPU encoding and load straight through
`/import`.

- **Location:** a private S3 bucket in `eu-west-1`, under an `embeddings/<dataset>/`
  prefix (not public — access needs bucket credentials; available on request).
- **Model / format:** `lightonai/LateOn`, 128-dim per-token vectors. Each document
  is one `doc_{i}.npy` (float32 array of shape `(n_tokens, 128)`); queries are
  `query_{i}.npy` in the same shape.
- **Per-dataset files:** `doc_{i}.npy` / `query_{i}.npy` (one per item, `i` = 0-based
  row position), `doc_count.txt` / `query_count.txt`, `doc_ids.npz` /
  `query_ids.npz` (original BEIR ids, ordered to match row positions), `qrels.json`
  (relevance judgments), and `id_map.json` (row position → BEIR id, written at
  import time).
- **Counts:** as in the §1 table (e.g. fiqa 57,638 docs / 648 queries; webis
  382,545 / 49; quora 522,931 / 10,000).
- **Config to reproduce:** IVF_PQ `num_sub_vectors=64`, `num_bits=8`, `nprobes=20`,
  `k=100`, cosine distance, firnflow 0.9.2.
- **Checksums:** not yet published; a per-object manifest (key, size, sha256) can be
  generated from the prefix on request.

scifact (the small control set) was not uploaded; re-encoding it is cheap.

## Provenance: the exact-vs-indexed fiqa measurement

The index-recall result in the Caveats is the one conclusion that shifts the story,
so its raw outputs are committed next to this report:

- `fiqa_exact_vs_indexed/fiqa_brute.json` — exact MaxSim (no vector index).
- `fiqa_exact_vs_indexed/fiqa_indexed.json` — IVF_PQ `num_sub_vectors=64`,
  `num_bits=8`.

Both are the **full 648-query** official fiqa set, `nprobes=20`, `k=100`, firnflow
0.9.2, real S3 in `eu-west-1`, scored through the same query and BEIR-eval path as
the §1 table. Method: re-import `beir-fiqa` via `/import` (single fragment) with the
vector index skipped and score it (exact), then build the IVF_PQ index on the same
data and score the identical query set again; the result-cache hit counter was 0
throughout. (The bench harness defaults to a 50-query quick diagnostic; this run
used the full 648-query set, which is what the committed JSON and the ~22% figure
reflect.)
