# Firn

**Firn** is a high-performance, multi-tenant vector and full-text search engine backed by object storage (AWS S3, MinIO, Cloudflare R2, Tigris, DigitalOcean Spaces, Google Cloud Storage). It is designed as a credible open-source alternative to turbopuffer, proving that a professional-grade tiered storage architecture (**RAM → NVMe → object storage**) is achievable entirely from open-source components. See [Storage backends](#storage-backends) for the full compatibility matrix.

The cost efficiency of object storage with the speed of local RAM. A multi-tenant vector and full-text search engine backed by any S3-compatible bucket or native Google Cloud Storage. Built with LanceDB and Foyer for microsecond-scale search latency on top of object storage.

## 25 Seconds to 72 Microseconds

On real-world cloud infrastructure (AWS S3), a raw linear scan of 100,000 vectors can take **25 seconds** per query. By pairing **LanceDB** with a tiered **foyer** (RAM + NVMe) cache, **Firn** collapses that bottleneck:

*   **Cold Query (S3 Linear Scan):** ~25.1s
*   **Cold Query (ANN Indexed):** ~979ms (**25x faster**)
*   **Warm Query (Internal Engine):** **~72µs** (**350,000x faster**)
*   **End-to-End HTTP (Warm):** **< 5ms** (including network RTT and JSON overhead)

Every cache hit results in **zero** object-storage requests, directly reducing your cloud bill while providing "instant" search response times.

### Demo

Cold query, warm query, full-text search, and cache proof, all in 60 seconds. This demo runs against local MinIO; on real AWS S3 the cold query takes 25 seconds instead of 109ms, making the cache speedup even more dramatic.

![Firn demo](bench/demo.gif)

## Architecture

**Firn** is built on a "Tiered Storage" philosophy:

1.  **L1: RAM Cache** (via foyer): Sub-microsecond access for the most frequent queries.
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
*   **Instant Invalidation:** A "Generation Counter" strategy ensures that after a write, all stale search results for that namespace are invalidated in $O(1)$ time.
*   **CAS Consistency:** Verified concurrency safety using the backend's conditional-write primitive — `If-None-Match: *` for S3-family backends, the generation precondition for native GCS — to prevent data loss when multiple writers fight for the same bucket.
*   **Zero-Copy Ready:** Optimized serialization via `bincode` (with architectural triggers to move to `rkyv` if needed).
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

### 3. Perform a Search
Query the same namespace for the nearest neighbor:

```bash
curl -X POST http://localhost:3000/ns/demo/query \
     -H 'Content-Type: application/json' \
     -d '{"vector": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], "k": 1}'
```

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
| `/ns/{ns}` | `DELETE` | admin | Removes all data (object storage + cache) for a namespace |
| `/ns/{ns}/upsert` | `POST` | read/write | Insert/update vectors and data |
| `/ns/{ns}/query` | `POST` | read/write | Vector, FTS, or hybrid search |
| `/ns/{ns}/list` | `GET` | read/write | Cursor-paginated list ordered by `_ingested_at` |
| `/ns/{ns}/warmup` | `POST` | read/write | Non-blocking cache pre-warm hint |
| `/ns/{ns}/index` | `POST` | admin | Build IVF_PQ vector index (async, returns 202) |
| `/ns/{ns}/fts-index` | `POST` | admin | Build BM25 full-text search index (async, returns 202) |
| `/ns/{ns}/scalar-index` | `POST` | admin | Build BTree index on `_ingested_at` to accelerate `/list` (async, returns 202) |
| `/ns/{ns}/compact` | `POST` | admin | Compact and prune data files (async, returns 202) |

Auth column: `open` = no header required; `read/write` = `FIRNFLOW_API_KEY` (or `FIRNFLOW_ADMIN_API_KEY`); `admin` = `FIRNFLOW_ADMIN_API_KEY` if configured, otherwise `FIRNFLOW_API_KEY` via the single-key fallback. `/metrics` is `metrics` when `FIRNFLOW_METRICS_TOKEN` is set, otherwise `open`.

## Development and Benchmarking

**Firn** uses a containerized toolchain. No local Rust installation is required.

```bash
# Run the full test suite (requires MinIO)
./scripts/cargo test --workspace -- --ignored

# Run the cold-vs-warm latency benchmark
./scripts/cargo run --release -p firnflow-bench
```

Benchmark results are committed at `bench/results/cold_vs_warm.md`.
