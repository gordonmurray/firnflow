# Firn

**Firn** is a multi-tenant vector and full-text search engine backed by object storage (AWS S3, MinIO, Cloudflare R2, Tigris, DigitalOcean Spaces, Google Cloud Storage). It is a credible open-source alternative to proprietary object-storage-backed search services, showing that a tiered storage architecture (**RAM, then NVMe, then object storage**) can be built entirely from open-source components. See [Storage backends](#storage-backends) for the full compatibility matrix.

It pairs LanceDB (vector and BM25 search that runs directly on object storage) with foyer (a RAM + NVMe cache), so your data sits on cheap object storage while repeated queries are served from cache without a backend round-trip.

## Performance

Benchmarked at 100,000 vectors of 1536 dimensions (OpenAI embedding size) against AWS S3 in `eu-west-1`:

| Query path | p50 latency |
| --- | --- |
| Cold, no index (brute-force scan over S3) | ~25.1 s |
| Cold, IVF_PQ index (first run of a given query) | ~979 ms |
| Warm (byte-identical repeat, served from cache) | ~72 µs |
| End-to-end HTTP, warm | < 5 ms |

What decides whether Firn fits your workload:

*   **An IVF_PQ index makes search on object storage practical.** With no index every query is a brute-force scan at ~25 s; with one, a cold query is ~979 ms. Build it (`POST /ns/{ns}/index`) after your first batch of writes.
*   **The result cache accelerates queries that repeat, not queries that are new.** A warm hit is a byte-identical repeat against the same namespace version; it returns in microseconds and, once the handle is warm, makes zero backend requests. A novel query misses and pays the cold cost above. See [What the cache does and does not do](#what-the-cache-does-and-does-not-do).
*   **Two optional layers widen what skips the backend.** The [semantic cache](#opt-in-semantic-cache) reuses a recent result when an incoming query vector is close enough (an approximate reuse, so it is opt-in). The object cache keeps the object-storage bytes Lance reads on local NVMe, so even a genuinely new query over already-read data avoids repeating the S3 round-trips.

## What the cache does and does not do

The **result cache** stores a complete serialised query result set, keyed on the namespace, its current Lance table version, and a hash of the full query. A hit needs an exact repeat of the same query against the same version. Any committed write advances the version and makes that namespace's cached results unreachable, so a running server never returns stale results after a write, and because the version is persisted that holds across a restart. Forming the key reads the version, so the first query to a namespace in a process opens its table handle (one manifest read) even on a cache hit; later hits read it from memory.

A query Firn has not seen before misses the result cache and pays the full LanceDB-over-object-storage cost. That cost is already low once an IVF_PQ index exists, and LanceDB's own indexes, the per-namespace connection pool, and OS page caching reduce it further. Two **opt-in** layers go further still:

*   **Semantic cache.** Widens hits to *near-duplicate* queries, returning an approximate result that was not freshly searched. See [Opt-in semantic cache](#opt-in-semantic-cache).
*   **Object cache.** Keeps the immutable object-storage bytes Lance reads (data fragments and index files) on local NVMe, so a cold or genuinely novel query over data already pulled once is served from disk instead of repeating the small S3 GETs that dominate cold latency. It caches only write-once objects (manifests and any conditional or versioned read always pass through), so it needs no invalidation step: a write, delete, compaction, or index build is reflected immediately. Disk use is a byte budget with LRU eviction, held across restarts. Off by default; set `FIRNFLOW_OBJECT_CACHE_ENABLED=true` and point `FIRNFLOW_OBJECT_CACHE_DIR` at fast local disk. The `firnflow_object_cache_*` metrics show its effectiveness, and [configuration](https://firnflow.io/configuration.html#object-cache) documents the byte budget and per-entry limits.

If your traffic is mostly unique queries the result-cache hit rate is low by design. The value there is the cost and multi-tenant model of search on object storage, optionally with the object cache absorbing the repeated byte reads underneath.

### Demo

Cold query, warm query, full-text search, and cache proof in 60 seconds, against local MinIO with no index, so the cold query is a fast ~109 ms here. On real S3 an unindexed cold query is closer to ~25 s, an IVF_PQ index brings that to ~979 ms, and repeated queries return from cache in microseconds.

![Firn demo](bench/demo.gif)

## Python package

The engine also ships as [`firn` on PyPI](https://pypi.org/project/firn/), embedding Firn in your Python process with no server to run. Vector, BM25 full-text, and hybrid search work against a local directory or any supported object-storage backend.

```bash
pip install firn
```

```python
import firn

db = firn.connect("./firn_data")  # a local folder; or storage_url="s3://bucket"

db.add([
    {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "the quick brown fox"},
    {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0], "text": "a lazy dog sleeps"},
    {"id": 3, "vector": [0.0, 0.0, 1.0, 0.0], "text": "the fox runs fast"},
])

# full-text + vector, fused in one call
for hit in db.search("fox", vector=[1.0, 0.0, 0.0, 0.0], limit=3):
    print(hit.id, hit.score, hit.text)
```

![firn Python package demo](bench/python-demo.gif)

`tenant="customer-42"` on any call selects a physically separate namespace, the same isolation the server provides. Wheels cover Linux x86_64 and aarch64 and macOS, on Python 3.10 and newer; the package versions independently of the server on its own `firn-v*` tags (current: `firn 0.1.0`). Runnable examples, including image search with CLIP embeddings on object storage, live in [`examples/`](examples/).

The v0.1 scope is embedded use only: the package does not connect to a running Firn server, every row carries a vector (`text` rides along for full-text and hybrid search), and deletes are namespace-level rather than per row.

## Architecture

**Firn** is built on a "Tiered Storage" philosophy:

1.  **L1: RAM Cache** (via foyer): Microsecond-scale reads for the most frequent queries.
2.  **L2: NVMe Cache** (via foyer): Fast, durable cache for high-volume search results.
3.  **L3: Object Storage** (via LanceDB on AWS S3 / MinIO / R2 / Tigris / Spaces / native GCS): The "Source of Truth" where every namespace is isolated under its own object-storage prefix.

An optional **object cache** sits between LanceDB and object storage, keeping the byte ranges Lance reads on local NVMe. This is distinct from the foyer result cache above, which stores whole query results. Off by default; see [What the cache does and does not do](#what-the-cache-does-and-does-not-do).

### Key Technologies
*   **axum:** High-performance async REST API.
*   **LanceDB:** Vector and BM25 search engine that runs natively on object storage.
*   **foyer:** Advanced hybrid cache (RAM + NVMe) with LFU/LRU eviction.
*   **Prometheus:** Full operational visibility into cache hits, misses, and object-storage request savings.

## Storage backends

Firn's correctness depends on the underlying object store offering a strictly linearisable compare-and-swap across concurrent writers. LanceDB's commit protocol uses this guarantee to serialise manifest updates, so a backend that ignores or incorrectly handles the conditional-write contract will silently lose writes. For S3-family backends the contract is `If-None-Match: *`; for native Google Cloud Storage it is the generation precondition (`x-goog-if-generation-match: 0` on the GCS XML API), which lancedb's `gcs` feature wires through transparently. Every provider below has been tested with the same shape: a sequential conditional-PUT pre-flight, an 8-writer x 100-row concurrent stress, and (for the passing backends) 100 consecutive runs of that stress against a real bucket. The test harness is in `crates/firnflow-core/tests/`.

| Provider | Supported | Reason |
| --- | :---: | --- |
| **AWS S3** (`eu-west-1` validated) | ✅ | Strict CAS, clean pass on 100-run stress. |
| **MinIO** (self-hosted / local) | ✅ | Reference implementation for the S3 protocol; clean pass on 100-run stress. |
| **Cloudflare R2** | ✅ | `If-None-Match: *` honoured correctly; 100-run stress clean. Per-iteration latency is roughly 7x AWS due to R2's multi-region commit path, but correctness is what the gate checks. Zero egress makes this the most interesting non-AWS target. Use path-style addressing. |
| **Backblaze B2** (S3 compat layer) | ❌ | Returns `HTTP 501 NotImplemented` on the first PutObject with `If-None-Match: *`. B2's native API supports conditional writes via `X-Bz-*` headers, but the S3-compat gateway does not translate them. Loud failure: easy to detect, not usable for Firn. |
| **Tigris** (dual-region + single-region) | ✅ | `If-None-Match: *` honoured on concurrent commits; 100-run stress clean on both dual-region and single-region buckets as of 2026-04-19 after an upstream CAS fix. Use path-style addressing on `t3.storage.dev`. |
| **DigitalOcean Spaces** (`lon1` validated) | ✅ | Strict CAS, 100-run stress clean. Per-iteration latency ~3.10s, in the same band as AWS `eu-west-1` and the fastest non-AWS backend tested. Use the regional endpoint (`https://<region>.digitaloceanspaces.com`), not the virtual-hosted form, with path-style addressing. |
| **Google Cloud Storage** (native, `europe-west1` validated) | ✅ | Routed through lancedb's native `gcs` feature and `object_store::gcp`, which use the GCS XML API's generation precondition (`x-goog-if-generation-match: 0`) instead of `If-None-Match: *`. 100-run Lance-level concurrent-writer stress passes cleanly against `firn-gcs-bucket-europe-west1`; an 8-writer barrier-gated contended-key microstress and a sequential pre-flight also pass (see `crates/firnflow-core/tests/lance_concurrent_writes.rs` and `s3_conditional_writes.rs`). Auth is service-account JSON via the standard `GOOGLE_*` environment variables. Use a `gs://...` URI; the GCS S3-interop endpoint (reached via an `s3://` URI plus a custom `GCS_ENDPOINT`) remains unsupported because that path silently drops `If-None-Match: *` and loses writers under contention. |

The two dedicated tests live at `crates/firnflow-core/tests/s3_conditional_writes.rs` and `crates/firnflow-core/tests/lance_concurrent_writes.rs`. Both are `#[ignore]`'d and require credentials to run. If you want to evaluate a backend not in the table, copy a block from either file and point it at your own bucket.

## Backend Configuration

Backend choice is an operator config decision, not a recompile. Set `FIRNFLOW_STORAGE_URI` to point Firn at the bucket you want. Switching between any two validated backends is an env-var change. `FIRNFLOW_S3_BUCKET` remains supported as a legacy S3-only fallback; if both are set they must agree, or startup fails.

`FIRNFLOW_STORAGE_URI` accepts an `s3://` or `gs://` URI with an optional fixed prefix, e.g. `s3://shared-bucket/tenants/acme/prod` or `gs://shared-bucket/tenants/acme/prod`. The prefix is useful when several deployments share a single bucket; namespace tables live at `{root}/{namespace}/`.

### AWS S3

```bash
FIRNFLOW_STORAGE_URI=s3://my-firn-bucket
FIRNFLOW_S3_REGION=eu-west-1
# Credentials picked up from the standard AWS chain (instance profile,
# AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, ~/.aws/credentials).
```

`FIRNFLOW_S3_REGION` is optional: if it is unset, Firn falls back to the standard `AWS_REGION`, then `AWS_DEFAULT_REGION`, and only then to `us-east-1`. Set `FIRNFLOW_S3_REGION` to pin the region for Firn explicitly, or rely on the `AWS_*` variables your host already exports (the usual case on EC2/ECS). A region that does not match the bucket's region fails the request, so this matters for any backend that enforces region (real AWS S3 does; MinIO and most emulators ignore it).

### MinIO (local / self-hosted)

```bash
FIRNFLOW_STORAGE_URI=s3://firnflow
FIRNFLOW_S3_ENDPOINT=http://localhost:9000
FIRNFLOW_S3_ACCESS_KEY=minioadmin
FIRNFLOW_S3_SECRET_KEY=minioadmin
```

### Cloudflare R2

```bash
FIRNFLOW_STORAGE_URI=s3://firn-r2
FIRNFLOW_S3_ENDPOINT=https://<account-id>.r2.cloudflarestorage.com
FIRNFLOW_S3_ACCESS_KEY=<r2-access-key>
FIRNFLOW_S3_SECRET_KEY=<r2-secret-key>
FIRNFLOW_S3_REGION=auto
```

### Tigris

```bash
FIRNFLOW_STORAGE_URI=s3://firn-tigris
FIRNFLOW_S3_ENDPOINT=https://t3.storage.dev
FIRNFLOW_S3_ACCESS_KEY=<tigris-access-key>
FIRNFLOW_S3_SECRET_KEY=<tigris-secret-key>
FIRNFLOW_S3_REGION=auto
```

### DigitalOcean Spaces

```bash
FIRNFLOW_STORAGE_URI=s3://firn-spaces
FIRNFLOW_S3_ENDPOINT=https://<region>.digitaloceanspaces.com
FIRNFLOW_S3_ACCESS_KEY=<spaces-access-key>
FIRNFLOW_S3_SECRET_KEY=<spaces-secret-key>
FIRNFLOW_S3_REGION=<region>
```

### Google Cloud Storage (native)

```bash
FIRNFLOW_STORAGE_URI=gs://my-firn-bucket
GOOGLE_APPLICATION_CREDENTIALS=/etc/firnflow/gcp-sa.json
# Alternatively: GOOGLE_SERVICE_ACCOUNT_PATH=/etc/firnflow/gcp-sa.json,
# or GOOGLE_SERVICE_ACCOUNT_KEY=<inline service-account JSON>.
```

Use `gs://...` rather than reaching for the GCS S3-interop endpoint. The interop path silently drops `If-None-Match: *` and is not supported.

## Features

*   **Multi-tenant by Design:** Each namespace maps to an isolated object-storage prefix under the configured `FIRNFLOW_STORAGE_URI` (e.g. `s3://bucket/namespace/` or `gs://bucket/namespace/`) with near-zero idle cost.
*   **Instant Invalidation:** Cached results are keyed on the Lance table version, so a write advances the version and makes that namespace's stale results unreachable in $O(1)$ time, with no separate bookkeeping.
*   **Optional Object Cache:** A byte-range cache on local NVMe beneath the storage engine. When enabled, the object-storage reads behind cold and novel queries are served from disk, not just exact-repeat queries. Off by default ([details](#what-the-cache-does-and-does-not-do)).
*   **CAS Consistency:** Verified concurrency safety using the backend's conditional-write primitive (`If-None-Match: *` for S3-family backends, the generation precondition for native GCS) to prevent data loss when multiple writers fight for the same bucket.
*   **Late-Interaction Search:** Each namespace is either single-vector (one dense vector per row) or multivector (a bag of small vectors per row, scored via MaxSim). The multivector shape is what ColBERT, ColPali, and ColQwen2 produce, and is what compositional queries like *"a man with a logo on his shirt"* need to match each element independently. See [Multivector namespaces](#multivector-namespaces) below.
*   **Compact Serialization:** Query results are serialized with `bincode`, with a path to `rkyv` if a workload needs zero-copy.
*   **Operational Excellence:** Native Prometheus metrics tracking cache hit rates and backend request count (the primary signal for cost savings).

## Quickstart

### 1. Launch the Stack
Everything you need (MinIO storage + Firn API) is orchestrated via Docker Compose:

```bash
git clone https://github.com/gordonmurray/firnflow
cd firnflow
docker compose up --build
```

### 2. Upsert a Vector

The API is live at `http://localhost:3000`. Save a vector to the `demo` namespace:

```bash
curl -X POST http://localhost:3000/ns/demo/upsert \
     -H 'Content-Type: application/json' \
     -d '{
       "rows": [
         {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]}
       ]
     }'
```

Upsert is keyed by `id` and is latest-write-wins: re-sending a row whose `id` already exists replaces the stored row in full rather than adding a second copy, so retries and genuine updates are both safe. Ids must be unique within a single request. The `_ingested_at` timestamp tracks the most recent write to a row, not its first insert. The first write to a namespace builds a BTree index on `id` so later batches find their matches through the index instead of scanning every data file. See [Loading data at scale](#loading-data-at-scale) for first-load guidance.

### 3. Perform a Search
Query the same namespace for the nearest neighbor:

```bash
curl -X POST http://localhost:3000/ns/demo/query \
     -H 'Content-Type: application/json' \
     -d '{"vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], "k": 1}'
```

Hits carry the stored vector by default. Add `"include_vector": false` to the request if you only need ids, scores, and text. At realistic dimensions the vectors are most of the response bytes, so skipping them shrinks the response and the cached result, and can cut the object-storage read for the returned rows' vectors. It is response projection, not a scan optimisation: Lance still reads whatever it needs to score the query.

Add `"filter": "id > 1000"` or an `_ingested_at` predicate to scope vector, full-text, or hybrid search to matching rows. Filters use the same DataFusion SQL predicate dialect as `/list` and are applied before nearest-neighbour ranking, so vector queries return up to `k` neighbours that satisfy the predicate.

### 4. Check the Savings
See how much object-storage traffic you've avoided:

```bash
curl http://localhost:3000/metrics | grep s3_requests
```

(The metric is named `firnflow_s3_requests_total` for dashboard continuity but counts requests against whichever backend the deployment is configured for.)

## Authentication

Firn ships with optional bearer-token authentication on the REST API. Both keys are opt-in:

| Env var | Tier | Routes |
| :--- | :--- | :--- |
| `FIRNFLOW_API_KEY` | read/write | `upsert`, `query`, `list`, `warmup` |
| `FIRNFLOW_ADMIN_API_KEY` | admin (destructive) | `delete`, `index`, `fts-index`, `scalar-index`, `compact` |
| `FIRNFLOW_METRICS_TOKEN` | metrics | `/metrics` (otherwise public) |

Header format on every protected request: `Authorization: Bearer <token>` (generate a key with e.g. `openssl rand -hex 32`).

If `FIRNFLOW_ADMIN_API_KEY` is unset, the read/write key authorises admin routes too (single-key fallback). Set both keys to a different value to lock destructive operations behind a separate credential. If neither key is set the API stays open and logs a single startup `WARN`, preserving the default-open posture of the local-dev compose stack.

**This is service-level authentication.** Any holder of `FIRNFLOW_API_KEY` can read or write any namespace; any holder of `FIRNFLOW_ADMIN_API_KEY` can additionally delete or rebuild indexes on any namespace. If you need per-tenant namespace isolation, place Firn behind an authenticating gateway that enforces tenant-to-namespace authorisation. See [`docs/configuration.html`](https://firnflow.io/configuration.html) for the rate-limiting knobs (`FIRNFLOW_RATE_LIMIT_RPS`, `FIRNFLOW_RATE_LIMIT_BURST`, `FIRNFLOW_PREAUTH_IP_LIMIT_RPS`) and the `FIRNFLOW_TRUST_PROXY_HEADERS` switch for deployments behind a load balancer.

## API Surface

| Endpoint | Method | Auth | Description |
| :--- | :--- | :--- | :--- |
| `/health` | `GET` | open | Liveness check |
| `/metrics` | `GET` | metrics or open | Prometheus exposition format |
| `/ns/{ns}` | `GET` | read/write | Namespace metadata (row count, fragment count, indexes, table version); 404 if it has no data yet |
| `/ns/{ns}` | `DELETE` | admin | Removes all data (object storage + cache) for a namespace |
| `/ns/{ns}/upsert` | `POST` | read/write | Insert or update vectors and data (latest-write-wins by `id`) |
| `/ns/{ns}/import` | `POST` | read/write | Bulk-ingest an Arrow IPC stream (binary, insert-only, async 202). For large first loads; bypasses the JSON body limit |
| `/ns/{ns}/query` | `POST` | read/write | Vector, FTS, or hybrid search |
| `/ns/{ns}/list` | `GET` | read/write | Cursor-paginated list ordered by `_ingested_at` |
| `/ns/{ns}/warmup` | `POST` | read/write | Non-blocking cache pre-warm hint |
| `/ns/{ns}/index` | `POST` | admin | Build IVF_PQ vector index (async, returns 202) |
| `/ns/{ns}/fts-index` | `POST` | admin | Build BM25 full-text search index (async, returns 202) |
| `/ns/{ns}/scalar-index` | `POST` | admin | Build a BTree index on a column (async, returns 202). Optional body `{"column": "id"}` for write-path merge-insert lookups; defaults to `_ingested_at` to accelerate `/list` |
| `/ns/{ns}/compact` | `POST` | admin | Compact and prune data files (async, returns 202) |
| `/operations/{id}` | `GET` | read/write | Status of a background operation by the `operation_id` from its 202; 404 if unknown or evicted |

The async endpoints (`import`, `warmup`, `index`, `fts-index`, `scalar-index`, `compact`) return an opaque `operation_id` in their `202`; poll `GET /operations/{id}` to see whether the work is `running`, `succeeded`, or `failed` instead of inferring it from metrics.

Auth column: `open` = no header required; `read/write` = `FIRNFLOW_API_KEY` (or `FIRNFLOW_ADMIN_API_KEY`); `admin` = `FIRNFLOW_ADMIN_API_KEY` if configured, otherwise `FIRNFLOW_API_KEY` via the single-key fallback. `/metrics` is `metrics` when `FIRNFLOW_METRICS_TOKEN` is set, otherwise `open`.

## Loading data at scale

Two ingest shapes have different cost profiles, and it helps to treat them differently.

**Idempotent updates** (retries, re-ingesting changed documents) are what `/upsert` is built for. It is keyed by `id` and latest-write-wins, so re-sending a row is safe. The first write to a namespace builds a BTree on `id`, which is what the per-batch merge-insert uses to find existing rows instead of scanning every data file.

**A large first load** (millions of rows into a fresh namespace over S3) should use `POST /ns/{ns}/import`. The `/upsert` JSON path has two costs at this scale: every call is a separate Lance commit, so a long run of small batches piles up commit and small-fragment bookkeeping, and JSON encodes each `f32` as decimal text (~3x the bytes of the 4-byte binary form), which also runs into the `FIRNFLOW_MAX_BODY_BYTES` limit. `/import` avoids both: the body is an [Arrow IPC stream](https://arrow.apache.org/docs/format/Columnar.html#ipc-streaming-format) (binary, columnar, no float inflation), the route is not bound by the body limit (so a whole corpus can stream in one request), and the entire stream is appended in a single commit.

It is insert-only: a repeated `id` makes a second row, so `/import` is for first-loads or known-new ids, not idempotent updates (use `/upsert` for those). The request streams its body to disk and returns `202` with an `operation_id` once validated; poll `GET /operations/{id}` for completion. The byte cap is `FIRNFLOW_IMPORT_MAX_BYTES` (default 8 GiB; `0` disables it) and the spool directory is `FIRNFLOW_IMPORT_TMP_DIR` (default the system temp dir).

A recommended recipe for a first load:

1. **Load** the data with `POST /ns/{ns}/import` (one Arrow IPC stream, or a few large ones).
2. **Compact** once the load is done: `POST /ns/{ns}/compact`. This merges data files into fewer large ones.
3. **Build the query indexes**: `POST /ns/{ns}/index` (vector), `POST /ns/{ns}/fts-index` (full-text), `POST /ns/{ns}/scalar-index` with `{"column": "id"}` if you will then do idempotent `/upsert` updates, and `{"column": "_ingested_at"}` if you page with `/list`. These are async; poll `GET /operations/{id}`.
4. **Query.**

If you stay on the JSON `/upsert` path (smaller loads, or idempotent updates), size batches up toward `FIRNFLOW_MAX_BODY_BYTES` rather than sending rows a handful at a time, and build indexes after the load rather than during it.

## Multivector namespaces

Each namespace is one of two **vector kinds**, fixed by the shape of the first upsert and immutable thereafter:

- **Single-vector** (the default). One dense vector per row. Used for CLIP, OpenAI `text-embedding-3-*`, sentence-transformers (anything that pools a piece of content into a single embedding).
- **Multivector**. A variable-length bag of small vectors per row, scored with MaxSim (for each query sub-vector, find its best match anywhere in the document, then sum the matches). This is what ColBERT, ColPali, and ColQwen2 produce, and what compositional queries like *"a man with a logo on his shirt"* need: each query element finds its own best match independently, instead of the whole query collapsing into one summary vector that smears the concepts together.

The wire shape determines the kind. A single-vector upsert uses `vector: [f32, ...]`; a multivector upsert uses `vectors: [[f32, ...], [f32, ...], ...]`. Queries follow the same convention:

```bash
# single-vector upsert + query
curl -X POST http://localhost:3000/ns/photos/upsert \
     -H 'Content-Type: application/json' \
     -d '{"rows": [{"id": 1, "vector": [0.1, 0.2, 0.3, 0.4]}]}'
curl -X POST http://localhost:3000/ns/photos/query \
     -H 'Content-Type: application/json' \
     -d '{"vector": [0.1, 0.2, 0.3, 0.4], "k": 5}'

# multivector upsert + query
curl -X POST http://localhost:3000/ns/photos-mv/upsert \
     -H 'Content-Type: application/json' \
     -d '{"rows": [{"id": 1, "vectors": [[0.1, 0.2, 0.3, 0.4], [0.5, 0.6, 0.7, 0.8]]}]}'
curl -X POST http://localhost:3000/ns/photos-mv/query \
     -H 'Content-Type: application/json' \
     -d '{"vectors": [[0.1, 0.2, 0.3, 0.4]], "k": 5}'
```

The handler returns 400 if the payload shape does not match the namespace's kind, for example a `vector:` payload sent to a multivector namespace, or vice versa. The error response names the expected shape.

**Constraints to know before adopting multivector:**

- **Cosine only.** Lance's late-interaction index supports cosine distance exclusively. Firn does not expose a per-request metric option on the API surface; `create_index` constructs the IVF_PQ builder with cosine internally for multivector namespaces and with L2 for single-vector namespaces.
- **Storage is materially larger.** A single-vector CLIP entry is ~2 KB per row. A multivector ColPali entry is closer to ~500 KB per row (around 1030 sub-vectors × 128 floats). Budget S3 footprint and index-build wall-clock time accordingly.
- **Build an index for tractable latency.** Lance answers multivector queries on an un-indexed namespace via brute-force scan, fine for tiny development corpora but painfully slow on anything real. Build the IVF_PQ index (`POST /ns/{ns}/index`) after the first batch of upserts. Same trade-off as single-vector queries.
- **The result cache does not accelerate novel multivector queries.** Every tokenised query is unique, so result-cache hit rate is near zero here; it still accelerates exact repeats (useful for benchmarks, not production retrieval). The object cache, if enabled, still helps by serving the underlying byte reads from NVMe.
- **New-namespace only.** A namespace that started as single-vector cannot be converted to multivector in place. Create a new namespace with a multivector first upsert.

**Encoders that produce vectors in the right shape:** ColBERTv2 (text passages), ColPali (documents, slides, PDFs), ColQwen2 / ColIDEFICS (multimodal: natural images and documents). Firn stays model-agnostic: the caller computes the bag of small vectors and POSTs it.

## Opt-in semantic cache

The exact result cache only helps when the same JSON request repeats verbatim. When users phrase the same intent differently ("holiday photos" vs "photos of my holidays", so the query vectors are very close but not byte-identical), the opt-in `semantic_cache` block on `POST /ns/{ns}/query` sits behind the exact cache and lets a near-duplicate query reuse a previous result.

```bash
curl -X POST http://localhost:3000/ns/photos/query \
     -H 'Content-Type: application/json' \
     -d '{
       "vector": [0.10, 0.21, 0.29, 0.40],
       "k": 10,
       "semantic_cache": {
         "enabled": true,
         "min_similarity": 0.995
       }
     }'
```

The read path is:

1. Compute the exact-cache key (which does **not** include the `semantic_cache` block, so toggling the option does not split otherwise-identical entries).
2. On an exact hit, return the cached bytes; the semantic layer is not consulted.
3. On an exact miss, scan the per-namespace semantic sidecar for a cached query whose vector cosine-similarity is at least `min_similarity` and whose `k` / `nprobes` / `include_vector` match. If something clears the bar, return its bytes.
4. Otherwise run the backend query, populate both layers, and return the fresh result.

**v1 boundaries** (returns 400 otherwise):

- single-vector queries only: `vectors`, `text`, `filter`, and hybrid shapes are rejected when `semantic_cache.enabled` is true;
- `min_similarity` must be in `(0.0, 1.0]`. Omitting picks a deliberately strict default of `0.995`;
- the sidecar is in-memory, single-process, and bounded to 1024 entries per namespace generation. Any committed change drops both layers for the namespace: writes, deletes, and compactions, and also index builds, since an index build is itself a Lance commit that advances the table version the cache keys on.

**Why opt-in.** A semantic hit is an *approximate* result reuse, not proof that Firn searched the corpus for the new query. High vector similarity does not guarantee an identical top-k under strict ranking. Three counters (`firnflow_semantic_cache_hits_total`, `_misses_total`, `_rejections_total{reason=…}`) make the behaviour visible so operators can judge whether the latency win is worth the approximation.

## Development and Benchmarking

**Firn** uses a containerized toolchain. No local Rust installation is required.

```bash
# Run the full test suite (requires MinIO)
./scripts/cargo test --workspace -- --ignored

# Run the cold-vs-warm latency benchmark
./scripts/cargo run --release -p firnflow-bench
```

Benchmark results are committed at `bench/results/cold_vs_warm.md`.
