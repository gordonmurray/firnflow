# Firn

**Firn** is a multi-tenant vector and full-text search engine backed by object storage (AWS S3, MinIO, Cloudflare R2, Tigris, DigitalOcean Spaces, Google Cloud Storage). It is designed as a credible open-source alternative to turbopuffer, showing that a tiered storage architecture (**RAM → NVMe → object storage**) can be built entirely from open-source components. See [Storage backends](#storage-backends) for the full compatibility matrix.

It pairs LanceDB (vector and BM25 search that runs directly on object storage) with foyer (a RAM + NVMe cache), so your data sits on cheap object storage while repeated queries are served from cache without a backend round-trip.

## Performance

Benchmarked at 100,000 vectors of 1536 dimensions (OpenAI embedding size) against AWS S3 in `eu-west-1`:

| Query path | p50 latency |
| --- | --- |
| Cold, no index (brute-force scan over S3) | ~25.1 s |
| Cold, IVF_PQ index (first run of a given query) | ~979 ms |
| Warm (byte-identical repeat, served from cache) | ~72 µs |
| End-to-end HTTP, warm | < 5 ms |

Two numbers decide whether Firn fits your workload:

*   **The IVF_PQ index is what makes search on object storage practical.** With no index every query is a brute-force scan at ~25 s; with one, a cold query is ~979 ms. Build the index (`POST /ns/{ns}/index`) after your first batch of writes.
*   **The cache accelerates queries that repeat, not queries that are new.** A warm hit is a byte-identical repeat of an earlier query against the same namespace generation. It returns in microseconds and, once the namespace's handle is warm, makes zero backend requests. A query Firn has not seen before misses the result cache and pays the cold cost above. See [What the cache does and does not do](#what-the-cache-does-and-does-not-do).
*   **Near-duplicate queries can skip the backend too.** The opt-in [semantic cache](#opt-in-semantic-cache) reuses a recent result when an incoming query vector is close enough, so paraphrases of the same search return from memory instead of re-running against the backend. The reused result is approximate, not a fresh search, which is why it is opt-in.

## What the cache does and does not do

Firn caches a complete serialised query result set, keyed on the namespace, its current Lance table version, and a hash of the full query. A hit needs an exact repeat of the same query against the same version. Any committed write advances the version and makes the namespace's cached results unreachable, so a running server does not return stale results after a write. Because the version is persisted, that holds across a restart too. Forming the key reads that version, so the first query to a namespace in a process opens its table handle (one manifest read) even on a cache hit; later hits read it from memory.

Novel queries always miss this cache and pay the full LanceDB-over-S3 cost. That cost is already low once an IVF_PQ index exists, and LanceDB's own IVF_PQ / FTS indexes, the per-namespace connection pool, and OS page caching reduce it further, but Firn's result cache does not. If your traffic is mostly unique queries the result-cache hit rate will be low by design, and the value is the cost and multi-tenant operational model of search on object storage rather than a microsecond latency on every call. The opt-in [semantic cache](#opt-in-semantic-cache) widens hits to near-duplicate queries, in exchange for returning an approximate result that was not freshly searched.

### Demo

Cold query, warm query, full-text search, and cache proof, all in 60 seconds. The demo runs against local MinIO with no index, so the cold query is fast here (~109 ms). On real AWS S3 an unindexed cold query is closer to ~25 s; an IVF_PQ index brings cold queries down to ~979 ms, and repeated queries are served from cache in microseconds.

![Firn demo](bench/demo.gif)

## Architecture

**Firn** is built on a "Tiered Storage" philosophy:

1.  **L1: RAM Cache** (via foyer): Microsecond-scale reads for the most frequent queries.
2.  **L2: NVMe Cache** (via foyer): Fast, durable cache for high-volume search results.
3.  **L3: Object Storage** (via LanceDB on AWS S3 / MinIO / R2 / Tigris / Spaces / native GCS): The "Source of Truth" where every namespace is isolated under its own object-storage prefix.

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
| **Google Cloud Storage** (native, `europe-west1` validated) | ✅ | Routed through lancedb's native `gcs` feature and `object_store::gcp`, which use the GCS XML API's generation precondition (`x-goog-if-generation-match: 0`) instead of `If-None-Match: *`. 100-run Lance-level concurrent-writer stress passes cleanly against `firn-gcs-bucket-europe-west1`; an 8-writer barrier-gated contended-key microstress and a sequential pre-flight also pass — see `crates/firnflow-core/tests/lance_concurrent_writes.rs` and `s3_conditional_writes.rs`. Auth is service-account JSON via the standard `GOOGLE_*` environment variables. Use a `gs://...` URI; the GCS S3-interop endpoint (reached via an `s3://` URI plus a custom `GCS_ENDPOINT`) remains unsupported because that path silently drops `If-None-Match: *` and loses writers under contention. |

The two dedicated tests live at `crates/firnflow-core/tests/s3_conditional_writes.rs` and `crates/firnflow-core/tests/lance_concurrent_writes.rs`. Both are `#[ignore]`'d and require credentials to run. If you want to evaluate a backend not in the table, copy a block from either file and point it at your own bucket.

## Backend Configuration

Backend choice is an operator config decision, not a recompile. Set `FIRNFLOW_STORAGE_URI` to point Firn at the bucket you want — switching between any two validated backends is an env-var change. `FIRNFLOW_S3_BUCKET` remains supported as a legacy S3-only fallback; if both are set they must agree, or startup fails.

`FIRNFLOW_STORAGE_URI` accepts an `s3://` or `gs://` URI with an optional fixed prefix, e.g. `s3://shared-bucket/tenants/acme/prod` or `gs://shared-bucket/tenants/acme/prod`. The prefix is useful when several deployments share a single bucket; namespace tables live at `{root}/{namespace}/`.

### AWS S3

```bash
FIRNFLOW_STORAGE_URI=s3://my-firn-bucket
FIRNFLOW_S3_REGION=eu-west-1
# Credentials picked up from the standard AWS chain (instance profile,
# AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, ~/.aws/credentials).
```

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

Use `gs://...` rather than reaching for the GCS S3-interop endpoint — the interop path silently drops `If-None-Match: *` and is not supported.

## Features

*   **Multi-tenant by Design:** Each namespace maps to an isolated object-storage prefix under the configured `FIRNFLOW_STORAGE_URI` (e.g. `s3://bucket/namespace/` or `gs://bucket/namespace/`) with near-zero idle cost.
*   **Instant Invalidation:** Cached results are keyed on the Lance table version, so a write advances the version and makes that namespace's stale results unreachable in $O(1)$ time, with no separate bookkeeping.
*   **CAS Consistency:** Verified concurrency safety using the backend's conditional-write primitive — `If-None-Match: *` for S3-family backends, the generation precondition for native GCS — to prevent data loss when multiple writers fight for the same bucket.
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

Rows are appended. Re-sending a row whose `id` already exists adds another row rather than replacing the old one; idempotent upsert by `id` is planned (tracked in [#31](https://github.com/gordonmurray/firnflow/issues/31)).

### 3. Perform a Search
Query the same namespace for the nearest neighbor:

```bash
curl -X POST http://localhost:3000/ns/demo/query \
     -H 'Content-Type: application/json' \
     -d '{"vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], "k": 1}'
```

Hits carry the stored vector by default. Add `"include_vector": false` to the request if you only need ids, scores, and text — at realistic dimensions the vectors are most of the response bytes, so skipping them shrinks the response and the cached result, and can cut the object-storage read for the returned rows' vectors. It is response projection, not a scan optimisation: Lance still reads whatever it needs to score the query.

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

Header format on every protected request: `Authorization: Bearer <token>`.

If `FIRNFLOW_ADMIN_API_KEY` is unset, the read/write key authorises admin routes too (single-key fallback). Set both keys to a different value to lock destructive operations behind a separate credential. If neither key is set the API stays open and logs a single startup `WARN` — this preserves the default-open posture of the local-dev compose stack.

```bash
export FIRNFLOW_API_KEY=$(openssl rand -hex 32)
curl -X POST http://localhost:3000/ns/demo/upsert \
     -H "Authorization: Bearer $FIRNFLOW_API_KEY" \
     -H 'Content-Type: application/json' \
     -d '{"rows": [...]}'
```

**This is service-level authentication.** Any holder of `FIRNFLOW_API_KEY` can read or write any namespace; any holder of `FIRNFLOW_ADMIN_API_KEY` can additionally delete or rebuild indexes on any namespace. If you need per-tenant namespace isolation, place Firn behind an authenticating gateway that enforces tenant-to-namespace authorisation. See [`docs/configuration.html`](https://firnflow.io/configuration.html) for the rate-limiting knobs (`FIRNFLOW_RATE_LIMIT_RPS`, `FIRNFLOW_RATE_LIMIT_BURST`, `FIRNFLOW_PREAUTH_IP_LIMIT_RPS`) and the `FIRNFLOW_TRUST_PROXY_HEADERS` switch for deployments behind a load balancer.

## API Surface

| Endpoint | Method | Auth | Description |
| :--- | :--- | :--- | :--- |
| `/health` | `GET` | open | Liveness check |
| `/metrics` | `GET` | metrics or open | Prometheus exposition format |
| `/ns/{ns}` | `GET` | read/write | Namespace metadata (row count, fragment count, indexes, table version); 404 if it has no data yet |
| `/ns/{ns}` | `DELETE` | admin | Removes all data (object storage + cache) for a namespace |
| `/ns/{ns}/upsert` | `POST` | read/write | Append vectors and data (not deduplicated by `id`) |
| `/ns/{ns}/query` | `POST` | read/write | Vector, FTS, or hybrid search |
| `/ns/{ns}/list` | `GET` | read/write | Cursor-paginated list ordered by `_ingested_at` |
| `/ns/{ns}/warmup` | `POST` | read/write | Non-blocking cache pre-warm hint |
| `/ns/{ns}/index` | `POST` | admin | Build IVF_PQ vector index (async, returns 202) |
| `/ns/{ns}/fts-index` | `POST` | admin | Build BM25 full-text search index (async, returns 202) |
| `/ns/{ns}/scalar-index` | `POST` | admin | Build BTree index on `_ingested_at` to accelerate `/list` (async, returns 202) |
| `/ns/{ns}/compact` | `POST` | admin | Compact and prune data files (async, returns 202) |
| `/operations/{id}` | `GET` | read/write | Status of a background operation by the `operation_id` from its 202; 404 if unknown or evicted |

The async endpoints (`warmup`, `index`, `fts-index`, `scalar-index`, `compact`) return an opaque `operation_id` in their `202`; poll `GET /operations/{id}` to see whether the work is `running`, `succeeded`, or `failed` instead of inferring it from metrics.

Auth column: `open` = no header required; `read/write` = `FIRNFLOW_API_KEY` (or `FIRNFLOW_ADMIN_API_KEY`); `admin` = `FIRNFLOW_ADMIN_API_KEY` if configured, otherwise `FIRNFLOW_API_KEY` via the single-key fallback. `/metrics` is `metrics` when `FIRNFLOW_METRICS_TOKEN` is set, otherwise `open`.

## Multivector namespaces

Each namespace is one of two **vector kinds**, fixed by the shape of the first upsert and immutable thereafter:

- **Single-vector** (the default). One dense vector per row. Used for CLIP, OpenAI `text-embedding-3-*`, sentence-transformers — anything that pools a piece of content into a single embedding.
- **Multivector**. A variable-length bag of small vectors per row, scored with MaxSim (for each query sub-vector, find its best match anywhere in the document, then add the matches up). This is the shape ColBERT, ColPali, and ColQwen2 produce. It is what compositional queries like *"a man with a logo on his shirt"* need to retrieve well — each element of the query gets to find its own best match independently of the others, instead of the whole query collapsing into one summary vector that smears the constituent concepts together.

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

The handler returns 400 if the payload shape does not match the namespace's kind — for example a `vector:` payload sent to a multivector namespace, or vice versa. The error response names the expected shape.

**Constraints to know before adopting multivector:**

- **Cosine only.** Lance's late-interaction index supports cosine distance exclusively. Firn does not expose a per-request metric option on the API surface; `create_index` constructs the IVF_PQ builder with cosine internally for multivector namespaces and with L2 for single-vector namespaces.
- **Storage is materially larger.** A single-vector CLIP entry is ~2 KB per row. A multivector ColPali entry is closer to ~500 KB per row (around 1030 sub-vectors × 128 floats). Budget S3 footprint and index-build wall-clock time accordingly.
- **Build an index for tractable latency.** Lance answers multivector queries on an un-indexed namespace via brute-force scan — fine for tiny development corpora, painfully slow on anything real. Build the IVF_PQ index (`POST /ns/{ns}/index`) after the first batch of upserts. Same trade-off as single-vector queries.
- **The result cache does not accelerate novel multivector queries.** Every tokenised query is unique, so cache hit rate is near zero on this path. The cache continues to accelerate exact-repeat queries — useful for benchmarks and synthetic load, not for production retrieval workloads.
- **New-namespace only.** A namespace that started as single-vector cannot be converted to multivector in place. Create a new namespace with a multivector first upsert.

**Encoders that produce vectors in the right shape:** ColBERTv2 (text passages), ColPali (documents, slides, PDFs), ColQwen2 / ColIDEFICS (multimodal — natural images and documents). Firn stays model-agnostic: the caller computes the bag of small vectors and POSTs it.

## Opt-in semantic cache

The exact result cache only helps when the same JSON request repeats verbatim. For workloads where users phrase the same intent in slightly different ways — "holiday photos" vs "photos of my holidays", where the query vectors are very close but not byte-identical — `POST /ns/{ns}/query` accepts an opt-in `semantic_cache` block that sits behind the exact cache and lets a near-duplicate query reuse a previous result.

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
2. On an exact hit, return the cached bytes — semantic layer is not consulted.
3. On an exact miss, scan the per-namespace semantic sidecar for a cached query whose vector cosine-similarity is at least `min_similarity` and whose `k` / `nprobes` / `include_vector` match. If something clears the bar, return its bytes.
4. Otherwise run the backend query, populate both layers, and return the fresh result.

**v1 boundaries** (returns 400 otherwise):

- single-vector queries only — `vectors`, `text`, and hybrid shapes are rejected when `semantic_cache.enabled` is true;
- `min_similarity` must be in `(0.0, 1.0]`. Omitting picks a deliberately strict default of `0.995`;
- the sidecar is in-memory, single-process, and bounded to 1024 entries per namespace generation. Any committed change drops both layers for the namespace: writes, deletes, and compactions, and also index builds, since an index build is itself a Lance commit that advances the table version the cache keys on.

**Why opt-in.** A semantic hit is an *approximate* result reuse, not proof that Firn searched the corpus for the new query. High vector similarity does not guarantee an identical top-k, especially under strict ranking. Three Prometheus counters — `firnflow_semantic_cache_hits_total`, `firnflow_semantic_cache_misses_total`, and `firnflow_semantic_cache_rejections_total{reason=…}` — make the behaviour visible so operators can decide whether the latency win is worth the approximation for a given workload.

## Development and Benchmarking

**Firn** uses a containerized toolchain. No local Rust installation is required.

```bash
# Run the full test suite (requires MinIO)
./scripts/cargo test --workspace -- --ignored

# Run the cold-vs-warm latency benchmark
./scripts/cargo run --release -p firnflow-bench
```

Benchmark results are committed at `bench/results/cold_vs_warm.md`.
