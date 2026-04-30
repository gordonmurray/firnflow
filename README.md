# Firn

**Firn** is a high-performance, multi-tenant vector and full-text search engine backed by object storage (AWS S3, MinIO, Cloudflare R2, Tigris, DigitalOcean Spaces). It is designed as a credible open-source alternative to turbopuffer, proving that a professional-grade tiered storage architecture (**RAM → NVMe → S3**) is achievable entirely from open-source components. See [Storage backends](#storage-backends) for the full compatibility matrix.

The cost efficiency of S3 with the speed of local RAM. A multi-tenant vector and full-text search engine backed by S3. Built with LanceDB and Foyer for microsecond-scale search latency on top of object storage.

## 25 Seconds to 72 Microseconds

On real-world cloud infrastructure (AWS S3), a raw linear scan of 100,000 vectors can take **25 seconds** per query. By pairing **LanceDB** with a tiered **foyer** (RAM + NVMe) cache, **Firn** collapses that bottleneck:

*   **Cold Query (S3 Linear Scan):** ~25.1s
*   **Cold Query (ANN Indexed):** ~979ms (**25x faster**)
*   **Warm Query (Internal Engine):** **~72µs** (**350,000x faster**)
*   **End-to-End HTTP (Warm):** **< 5ms** (including network RTT and JSON overhead)

Every cache hit results in **zero** S3 requests, directly reducing your cloud bill while providing "instant" search response times.

### Demo

Cold query, warm query, full-text search, and cache proof, all in 60 seconds. This demo runs against local MinIO; on real AWS S3 the cold query takes 25 seconds instead of 109ms, making the cache speedup even more dramatic.

![Firn demo](bench/demo.gif)

## Architecture

**Firn** is built on a "Tiered Storage" philosophy:

1.  **L1: RAM Cache** (via foyer): Sub-microsecond access for the most frequent queries.
2.  **L2: NVMe Cache** (via foyer): Fast, durable cache for high-volume search results.
3.  **L3: Object Storage** (via LanceDB + S3/MinIO): The "Source of Truth" where every namespace is isolated under its own S3 prefix.

### Key Technologies
*   **axum:** High-performance async REST API.
*   **LanceDB:** Vector and BM25 search engine that runs natively on object storage.
*   **foyer:** Advanced hybrid cache (RAM + NVMe) with LFU/LRU eviction.
*   **Prometheus:** Full operational visibility into cache hits, misses, and S3 request savings.

## Storage backends

Firn's correctness depends on S3's `If-None-Match: *` precondition behaving as a strictly linearisable compare-and-swap across concurrent writers. LanceDB's commit protocol uses it to serialise manifest updates, so a backend that ignores or incorrectly handles this header will silently lose writes. Every provider below has been tested with the same two checks: a sequential conditional-PUT pre-flight, and an 8-writer x 100-row concurrent stress (the passing backends additionally survive 100 consecutive runs). The test harness is in `crates/firnflow-core/tests/`.

| Provider | Supported | Reason |
| --- | :---: | --- |
| **AWS S3** (`eu-west-1` validated) | ✅ | Strict CAS, clean pass on 100-run stress. |
| **MinIO** (self-hosted / local) | ✅ | Reference implementation for the S3 protocol; clean pass on 100-run stress. |
| **Cloudflare R2** | ✅ | `If-None-Match: *` honoured correctly; 100-run stress clean. Per-iteration latency is roughly 7x AWS due to R2's multi-region commit path, but correctness is what the gate checks. Zero egress makes this the most interesting non-AWS target. Use path-style addressing. |
| **Backblaze B2** (S3 compat layer) | ❌ | Returns `HTTP 501 NotImplemented` on the first PutObject with `If-None-Match: *`. B2's native API supports conditional writes via `X-Bz-*` headers, but the S3-compat gateway does not translate them. Loud failure: easy to detect, not usable for Firn. |
| **Tigris** (dual-region + single-region) | ✅ | `If-None-Match: *` honoured on concurrent commits; 100-run stress clean on both dual-region and single-region buckets as of 2026-04-19 after an upstream CAS fix. Use path-style addressing on `t3.storage.dev`. |
| **DigitalOcean Spaces** (`lon1` validated) | ✅ | Strict CAS, 100-run stress clean. Per-iteration latency ~3.10s, in the same band as AWS `eu-west-1` and the fastest non-AWS backend tested. Use the regional endpoint (`https://<region>.digitaloceanspaces.com`), not the virtual-hosted form, with path-style addressing. |
| **Google Cloud Storage** (via S3 interop) | ❌ | Silently ignores `If-None-Match: *`: the second conditional PUT returns `200 OK` instead of `412`. Concurrent stress loses 6/8 writers every run, deterministically. GCS's native `x-goog-if-generation-match: 0` works correctly; only the S3-interop translation is broken. A future release could support GCS via LanceDB's native GCS backend rather than the S3 layer. |

The two dedicated tests live at `crates/firnflow-core/tests/s3_conditional_writes.rs` and `crates/firnflow-core/tests/lance_concurrent_writes.rs`. Both are `#[ignore]`'d and require credentials to run. If you want to evaluate a backend not in the table, copy a block from either file and point it at your own bucket.

## Features

*   **Multi-tenant by Design:** Each namespace maps to an isolated S3 prefix (`s3://bucket/namespace/`) with near-zero idle cost.
*   **Instant Invalidation:** A "Generation Counter" strategy ensures that after a write, all stale search results for that namespace are invalidated in $O(1)$ time.
*   **CAS Consistency:** Verified concurrency safety using S3's `If-None-Match: *` to prevent data loss when multiple writers fight for the same bucket.
*   **Zero-Copy Ready:** Optimized serialization via `bincode` (with architectural triggers to move to `rkyv` if needed).
*   **Operational Excellence:** Native Prometheus metrics tracking cache hit rates and S3 request count (the primary signal for cost savings).

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
See how much S3 traffic you've avoided:

```bash
curl http://localhost:3000/metrics | grep s3_requests
```

## API Surface

| Endpoint | Method | Description |
| :--- | :--- | :--- |
| `/health` | `GET` | Liveness check |
| `/metrics` | `GET` | Prometheus exposition format |
| `/ns/{ns}` | `DELETE` | Removes all data (S3 + Cache) for a namespace |
| `/ns/{ns}/upsert` | `POST` | Insert/update vectors and data |
| `/ns/{ns}/query` | `POST` | Vector, FTS, or hybrid search |
| `/ns/{ns}/list` | `GET` | Cursor-paginated list ordered by `_ingested_at` |
| `/ns/{ns}/warmup` | `POST` | Non-blocking cache pre-warm hint |
| `/ns/{ns}/index` | `POST` | Build IVF_PQ vector index (async, returns 202) |
| `/ns/{ns}/fts-index` | `POST` | Build BM25 full-text search index (async, returns 202) |
| `/ns/{ns}/compact` | `POST` | Compact and prune data files (async, returns 202) |

## Development and Benchmarking

**Firn** uses a containerized toolchain. No local Rust installation is required.

```bash
# Run the full test suite (requires MinIO)
./scripts/cargo test --workspace -- --ignored

# Run the cold-vs-warm latency benchmark
./scripts/cargo run --release -p firnflow-bench
```

Benchmark results are committed at `bench/results/cold_vs_warm.md`.
