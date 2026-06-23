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

- **Model / encoding:** `lightonai/LateOn` (ColBERT-style late interaction),
  128-dim per-token vectors, via PyLate `1.6.0` (sentence-transformers `5.3.0`),
  encoder document length `300` tokens. One vector per token, so the per-document
  vector count varies by dataset (dataset means range ~16 to ~237; see §1/§2).
- **Engine:** Firn `0.9.2`. Storage on **real AWS S3** in `eu-west-1` (not
  loopback MinIO). All eight quality datasets were loaded and scored on this one
  version via `/import`, so the quality table is single-version and internally
  consistent. (An earlier draft mixed `0.9.0` and `0.9.2` numbers; the small
  datasets were re-run on `0.9.2` to remove that confound — see Caveats.)
- **Index / query:** IVF_PQ vector index, `num_sub_vectors=64`, `num_bits=8`
  (default), queried at `nprobes=20`, `k=100`. Late-interaction (MaxSim) scoring.
- **Object cache:** read-through byte-range cache on local NVMe
  (`FIRNFLOW_OBJECT_CACHE_DIR=/object-cache`), enabled with
  `FIRNFLOW_OBJECT_CACHE_ENABLED=true`, a 100 GiB byte budget
  (`FIRNFLOW_OBJECT_CACHE_BYTES=107374182400`), and the per-entry cap left at its
  256 MiB default (`FIRNFLOW_OBJECT_CACHE_MAX_ENTRY_BYTES`). The `/import` path ran
  with `FIRNFLOW_IMPORT_MAX_BYTES=0` (no cap) and `FIRNFLOW_MAX_BODY_BYTES` at
  32 MiB.
- **Host:** all numbers reported here were measured on a single box
  (g4dn.8xlarge: 32 vCPU, 128 GiB RAM, 1×T4 GPU) in `eu-west-1`, reading from an
  S3 bucket in the same region. Quality is host-independent; latency/QPS are
  CPU-bound, so treat the absolute QPS as host-specific (see Caveats).
- **Instrumentation:** the cache numbers come in two batches. The fiqa cache A/B
  (§2) was measured on released `0.9.2`, whose object-cache counters only exist
  when the cache is on. The 1M-document NQ A/B (§2) was measured on an
  instrumented build — `0.9.2` plus an always-on backend read counter
  (`firnflow_object_store_get_bytes_total`) that records bytes fetched from S3
  whether the cache is on *or* off. That counter is what lets the NQ run report a
  real cache-*off* byte figure; quality numbers are unaffected by it.

## What this does and does not show

A one-screen summary before the detail, since quality and performance live on
different axes here:

- **Shows (quality, host-independent):** Firn's multivector path reaches the
  expected BEIR quality — it matches published ColBERT-style baselines on the
  control sets (scifact, nfcorpus) and tracks each task's known difficulty across
  eight datasets (§1).
- **Shows (cost, the headline cache result):** on object storage, the local NVMe
  object cache cuts S3 read **bytes** for repeated query traffic — ~3× cold-to-warm
  on fiqa (57k docs) and **~130×** cache-off-vs-cache-on on NQ (1M docs), measured
  directly off the backend byte counter (§2).
- **Shows (ingest):** `/import` loads a multi-hundred-thousand-document corpus in
  one storage commit, ~3 orders of magnitude faster than per-batch `/upsert` (§3).
- **Does not show a latency win from the cache on multivector.** MaxSim scoring is
  CPU-bound, so query latency is flat whether the cache is on or off (§2). The
  cache is a storage-*cost* lever here, not a latency lever. (Single-vector search
  is I/O-bound and does see cache latency wins; that is a different workload.)
- **Does not claim IVF_PQ is free.** At moderate corpus sizes the approximate index
  can cost real recall versus exact MaxSim, and the cost grows with corpus size
  (§1, Caveats). Measure exact-vs-indexed before assuming the index is lossless.
- **Latency/QPS numbers are from one 32-vCPU box and are not portable.** They show
  where the time goes, not a tuned throughput ceiling (Caveats).

## 1. Retrieval quality (eight BEIR datasets)

All eight datasets were loaded and scored on a single Firn version (`0.9.2`) via
`/import`, so the table is internally consistent. Each row is scored over the
**full official BEIR query set** for that dataset (the `queries` column below — not
the split-B subset used for the latency A/B in §2). These quality numbers are
independent of the object cache: it is a byte-range cache *below* the query path,
so it changes only how bytes are fetched, never the result set — quality is
identical with the cache on or off, which is why §2 measures only latency and S3
bytes per cache cell, not quality. The `vec/doc` column is the mean per-document
vector count (one vector per token); it is the main driver of query *latency* (§2),
but, as it turns out, not of the quality differences (see Caveats).

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
direct exact-vs-indexed check shows the **IVF_PQ index itself** is the main cause,
and the loss scales with corpus size (negligible at ≤25k docs, ~22% of nDCG@10 at
fiqa's 57k) — see *Index recall vs corpus size* just below. It is an index-recall
effect, not a document-length one: per-document vector counts do not predict it
(nfcorpus has the most vectors per document of any set here, yet is unaffected).

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

### Index recall vs corpus size (exact vs IVF_PQ)

The IVF_PQ index is an *approximate* nearest-neighbour structure, so the natural
question is how much retrieval quality it gives up versus exact (un-indexed) MaxSim
search on the same data. We measured exact-vs-indexed on three corpora spanning an
order of magnitude in size, identical `nprobes=20` / `k=100` / `0.9.2` / real-S3
setup, scoring the full official query set each time:

| dataset | docs | exact ndcg@10 | IVF_PQ ndcg@10 | Δ | exact recall@10 | IVF_PQ recall@10 |
|---|---:|---:|---:|---:|---:|---:|
| arguana | 8.7k | 0.5095 | 0.5404 (×3: 0.5413/0.5381/0.5417) | **+6%** | 0.786 | 0.806 |
| scidocs | 25k | 0.2180 | 0.2107 | −3% | 0.228 | 0.220 |
| fiqa | 57k | 0.5264 | 0.4084 | **−22%** | 0.609 | 0.468 |

The loss **scales with corpus (index) size**, it is not a per-dataset quirk: at
8.7k documents the index matches exact and even edges slightly above it (the
approximate reordering happens to land higher on nDCG, and three rebuilds agree to
within ~0.004, far inside the exact-vs-indexed gap); at 25k the gap is ~3%, inside
the noise; by 57k the index gives up ~22% of nDCG@10 and ~23% of recall@10. It does
*not* track document length — nfcorpus has the most vectors per document of any set
here (237) and sits in the stable small-corpus regime. The arguana column carries
three independent index rebuilds precisely to show that run-to-run training jitter
(~0.004) is an order of magnitude smaller than the size-driven trend, so the fiqa
drop is a real recall effect and not a single unlucky training draw.

Practically: at moderate multivector scale, **measure exact search before
defaulting to IVF_PQ** — exact retrieved materially better on fiqa for no extra
latency (the per-query cost is dominated by MaxSim either way; see §2). The index
earns its keep once the corpus is large enough that an exact scan is prohibitive.
This is why the §1 table — which reports the default IVF_PQ-indexed numbers — shows
fiqa and trec-covid below their quality ceiling.

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

### The same A/B at 1M documents (NQ): a true cache-off arm, ~130× bytes

The fiqa A/B above could only difference *cache-on* byte counters (cold-on's
empty-cache first touch vs warm-on), because on released `0.9.2` the object-cache
byte counter does not exist when the cache is off. To get a real cache-off
measurement — and to see whether the storage saving holds at scale — the same A/B
was repeated on **NQ (Natural Questions), 1,000,000 multivector documents**, a
single `/import` fragment, on the instrumented build that carries the always-on
backend byte counter `firnflow_object_store_get_bytes_total`. Same shape as fiqa:
IVF_PQ `num_sub_vectors=64`/`num_bits=8`, `nprobes=20`, `k=100`, concurrency 32,
500 measured queries per cell, split B disjoint from the split-A warming set so no
query is a result-cache replay.

| cell | object cache | process | QPS | p50 | p95 | backend S3 reads | backend S3 bytes |
|---|---|---|---:|---:|---:|---:|---:|
| cold-off | off | cold | 0.65 | 48.5 s | 55.0 s | 87,898 | 372 GB |
| cold-on | on, empty | cold | 0.68 | 46.7 s | 53.8 s | 28,114 | 9.70 GB |
| warm-off | off | cold | 0.65 | 48.0 s | 54.8 s | 87,175 | **361 GB** |
| warm-on | on, warm | cold | 0.68 | 46.4 s | 54.0 s | 24,039 | **2.79 GB** |

(The "backend S3 bytes" column is `firnflow_object_store_get_bytes_total` over the
measured pass — the real bytes fetched from S3, recorded identically whether the
cache is on or off. The cold-off cell read 372 GB; warm-off, the clean cache-off
control, read 361 GB.)

**The headline at 1M scale: ~130× fewer S3 bytes with the cache on.** For the same
500 NQ queries, cache-off (`warm-off`) pulled **361 GB** from S3 while cache-on
warm (`warm-on`) pulled **2.79 GB** — a **129.6×** reduction, read straight off the
same backend counter on both arms (no inference from an empty-cache proxy this
time). Even *cold* the cache cuts bytes ~38× (372 GB → 9.70 GB), because within a
single 500-query batch many queries re-read the same IVF centroid, PQ codebook, and
partition byte ranges, and the cache serves the repeats from NVMe after the first
touch. Cache-off has no such reuse: every query re-fetches those ranges from S3,
which is why 500 queries balloon to ~360 GB.

**Latency is still flat, now confirmed at 1M.** Every cell lands at ~46–48 s p50
regardless of cache state — the cache changes the S3 byte bill by two orders of
magnitude and does not move latency at all, because MaxSim over a 1M-document
candidate set is CPU-bound. The result-cache guardrail held: `firnflow_cache_hits_total`
was 0 in all four cells. So the fiqa conclusion is not a small-corpus artifact:
**at 1M documents the object cache is, again, a storage-cost lever and not a
latency lever for multivector search.**

**A note on the absolute latency.** The ~46 s p50 is the *saturated* latency at
concurrency 32 on a 32-vCPU box — every core is busy scoring MaxSim, so queries
queue behind each other. A 10-query probe fired at the same concurrency against a
warmed process (no queueing) measured ~14.5 s per query, so ~14.5 s is closer to
the single-query cost and ~46 s is what sustained 32-way load looks like on this
host. Neither is a tuned number; both are CPU-bound and scale with core count.

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

**Recommended starting config for multivector on object storage:**

| knob | recommended | why |
|---|---|---|
| first load | `/import` (Arrow IPC, one commit) | per-batch `/upsert` decays to ~30 h on 382k docs; `/import` does it in seconds (§3) |
| later updates | `/upsert` (idempotent by id) | `/import` is insert-only; `/upsert` is the in-place update/merge path |
| vector index | IVF_PQ `num_sub_vectors=64`, `num_bits=8` | safe default; 32/8 was marginally better on fiqa, 4-bit only when storage-bound |
| index vs exact | **measure both** at your corpus size | IVF_PQ loss grows with size: none ≤25k, ~22% at 57k (§1) |
| `nprobes` | 20 | quality flat 8→100, latency unchanged (CPU-bound) |
| object cache | on, `FIRNFLOW_OBJECT_CACHE_BYTES` ≥ working set | cuts S3 byte cost (~130× at 1M); does not cut multivector latency |

**Ingest — `/import` vs `/upsert`.** Use `/import` for the *first* bulk load of a
namespace: it streams the whole corpus as one Arrow IPC body and appends it in a
single storage commit, which is what makes a 382k-document load take seconds
instead of ~30 hours (§3). It is insert-only and keyed by row position. For
*incremental* writes after that — updating or re-adding documents by id — use
`/upsert`, which is idempotent by id (latest write wins). Reach for `/import` again
only when reloading a namespace from scratch.

**The object cache is a storage-cost lever, not a latency lever (on multivector).**
Its value here is cutting S3 request and byte cost: **~130× fewer S3 bytes** on the
1M-document NQ A/B (cache-off 361 GB vs cache-on warm 2.79 GB for the same 500
queries, §2), and ~3× cold-to-warm on fiqa. It does **not** lower multivector query
latency, which is CPU-bound on MaxSim — every NQ and fiqa cache cell sits at the
same p50 regardless of cache state. (Single-vector search *is* I/O-bound and does
see large cache latency wins — a different workload; do not carry the latency claim
across.) **Size the cache to hold the working set:** the saving needs
`FIRNFLOW_OBJECT_CACHE_BYTES` to cover the index byte-ranges plus the candidate
vectors a query reads; below that the cache thrashes and the saving largely
disappears (§2, the 256 MiB run).

**Multivector QPS is set mostly by document length, then corpus size.** A large
corpus of short documents is fast (quora, 523k, ~16 vectors/doc, 4.2 s p50); longer
documents and a larger candidate set both slow it (webis, ~154 vectors/doc, 35 s).
Plan capacity around document length and candidate-set size, not row count alone.
To improve QPS the lever is the MaxSim / candidate-set cost (centroid pruning before
full scoring, scoring-path parallelism), not the cache or `nprobes`.

**Index config.** `num_sub_vectors=64`, `num_bits=8` is a safe default. `num_bits=4`
roughly halves index storage; in our fiqa sweep its quality cost was clear only at
fewer sub-vectors (32/4 dropped to 0.392, while 64/4 at 0.4129 was on par with 64/8
at 0.4106), so it *can* cost quality — validate the recall cost per corpus before
using it. At moderate scale, also compare exact (un-indexed) search before
defaulting to IVF_PQ: the index loss grows with corpus size (none at ≤25k docs,
~22% of nDCG@10 at fiqa's 57k, §1), and at that scale exact retrieved materially
better for no extra latency. The index earns its keep once the corpus is large
enough that an exact scan is prohibitive.

## Caveats

- **Latency / QPS are host-specific.** All measurements here are from one 32-vCPU
  box; MaxSim scoring is CPU-bound, so the absolute QPS scales with core count and
  is not portable. Quality is host-independent.
- **Multivector QPS is modest (~1.8 on this box) and CPU-bound.** These numbers
  are not a tuned latency result; they characterise where the time goes.
- **The IVF_PQ index gives up recall versus exact MaxSim, and the loss grows with
  corpus size.** This is measured across three corpora in §1 (*Index recall vs
  corpus size*): arguana (8.7k) shows no loss (indexed even edges ~6% above exact,
  three rebuilds agreeing to ~0.004); scidocs (25k) is within ~3%; fiqa (57k) gives
  up ~22% of nDCG@10 and ~23% of recall@10 (exact 0.5264/0.609 vs IVF_PQ
  0.4084/0.468), at no latency benefit (per-query p50 18.4 s exact vs 19.9 s
  indexed). On fiqa, index parameters do not rescue it (`num_sub_vectors` /
  `num_bits` moved it only within 0.392–0.421; `nprobes` 8–100 made no difference),
  and exact sits above *both* earlier indexed fiqa loads (0.9.0 at 0.4563, 0.9.2 at
  ~0.41), so those were lossy training draws under the exact ceiling, not a version
  regression — both versions pin the same vector-index stack. The id-mapping was
  audited and is sound, so this is an index-level effect, not a harness artifact.
  It does **not** track document length (nfcorpus has the most vectors/doc here, 237,
  yet is in the stable small-corpus regime). The exact-vs-indexed comparisons each
  hold corpus and query set fixed, so the §1 table reports the default
  IVF_PQ-indexed numbers; the cache and latency results in §2/§3 likewise compare a
  namespace against itself and are unaffected. **Practical takeaway: at moderate
  multivector scale, measure exact (un-indexed) search before assuming the index is
  free — it can retrieve materially better for no extra latency.**
- **The cache figures are for a single-fragment namespace** (the recommended
  state after `/import`). A heavily fragmented namespace would show a larger
  cache effect, but that is the anti-pattern `/import` exists to avoid.

## Reusable artifact

Encoded LateOn embeddings for seven of the eight quality datasets (all except
scifact), plus a 1M-document NQ performance fixture, are on S3, so future runs (and
CI) can skip GPU encoding and load straight through `/import`. A committed manifest
([`beir_multivector_embeddings_manifest.jsonl`](beir_multivector_embeddings_manifest.jsonl))
lists each prefix with its document count, mean document length, and byte size.

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
  382,545 / 49; quora 522,931 / 10,000). The NQ fixture is the NQ corpus sliced to
  its first 1,000,000 documents (3,452 queries); it is a *performance* fixture, not
  a quality one — slicing the corpus drops relevant documents, so its scores are not
  BEIR-comparable, and it is used only for the §2 cache A/B.
- **Config to reproduce:** IVF_PQ `num_sub_vectors=64`, `num_bits=8`, `nprobes=20`,
  `k=100`, cosine distance, firnflow 0.9.2 (the NQ cache A/B additionally needs the
  always-on backend byte counter — an instrumented build, see Setup).
- **Checksums:** not yet published; a per-object manifest (key, size, sha256) can be
  generated from the prefix on request.

scifact (the small control set) was not uploaded; re-encoding it is cheap.

## Next steps

- **Wider quality coverage.** The eight datasets here plus the 1M-document NQ
  fixture prove the path at scale; extending the quality sweep to the remaining
  larger BEIR tasks (e.g. HotpotQA) would round out the picture, and the committed
  embeddings make each addition a load-and-score rather than a re-encode.
- **A late-interaction tuning page** mirroring this report on the docs site, written
  for discovery rather than audit, so the practical guidance in §4 is easy to find.
- **Independent comparison.** The embeddings, raw JSON, and config are all committed
  or available on request, so these numbers can be reproduced and checked against
  published PLAID / PyLate baselines on the same datasets. Comparisons and
  contributions are welcome.

## Provenance

The two results that most shift the story — the index-recall size trend and the
1M-document cache A/B — have their raw outputs committed next to this report, so
the numbers can be audited rather than taken on trust. A full table-to-file map is
in [`beir_multivector_raw/README.md`](beir_multivector_raw/README.md).

**Exact vs IVF_PQ across corpus sizes (§1).** Each pair holds the corpus and the
full official query set fixed, `nprobes=20`, `k=100`, firnflow 0.9.2, real S3 in
`eu-west-1`, scored through the same BEIR-eval path as the §1 table. Method:
`/import` the corpus as a single fragment with the vector index skipped and score
it (exact), then build the IVF_PQ index on the same data and score the identical
query set again; the result-cache hit counter was 0 throughout.

- `fiqa_exact_vs_indexed/fiqa_brute.json` vs `…/fiqa_indexed.json` — fiqa (57k,
  full 648-query set): exact 0.5264 vs IVF_PQ 0.4084.
- `beir_multivector_raw/exact_vs_indexed_variance/scidocs_exact.json` vs
  `…/scidocs_indexed_run1.json` — scidocs (25k): 0.2180 vs 0.2107.
- `beir_multivector_raw/exact_vs_indexed_variance/arguana_exact.json` vs
  `…/arguana_indexed_run{1,2,3}.json` — arguana (8.7k): exact 0.5095 vs three index
  rebuilds 0.5413 / 0.5381 / 0.5417 (the rebuilds quantify the ~0.004 training
  jitter).

**1M-document cache A/B (§2).** `beir_multivector_raw/nq_cache_ab/` — the four
NQ cells (`cold-off`, `cold-on`, `warm-off`, `warm-on`) plus their split-A warming
passes and the uncontended probe. Each JSON carries a `/metrics` snapshot before
and after the measured pass, including `firnflow_object_store_get_bytes_total` (the
always-on backend byte counter that gives the cache-off arm its 361 GB figure) and
`firnflow_cache_hits_total` (the result-cache guardrail, 0 in every cell).
