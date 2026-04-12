# firnflow

**firnflow** is a high-performance, multi-tenant vector and full-text search engine backed by object storage (S3 / MinIO / R2 / GCS). It is designed as a credible open-source alternative to turbopuffer, proving that a professional-grade tiered storage architecture (**RAM -> NVMe -> S3**) is achievable entirely from open-source components.

The cost efficiency of S3 with the speed of local RAM. A multi-tenant vector and full-text search engine backed by S3. Built with LanceDB and Foyer for sub-microsecond search latency on top of object storage.

## The Headline: 5,616x Faster Search

By pairing **LanceDB** (search on S3) with **foyer** (RAM + NVMe hybrid cache), firnflow delivers the cost efficiency of object storage with the latency of local memory:

*   **Cold Query (S3):** ~15ms
*   **Warm Query (Cache):** ~0.002ms
*   **S3 Savings:** Every cache hit results in **zero** S3 requests, directly reducing your cloud bill.

## Architecture

firnflow is built on a "Tiered Storage" philosophy:

1.  **L1: RAM Cache** (via foyer) — Sub-microsecond access for the most frequent queries.
2.  **L2: NVMe Cache** (via foyer) — Fast, durable cache for high-volume search results.
3.  **L3: Object Storage** (via LanceDB + S3/MinIO) — The "Source of Truth" where every namespace is isolated under its own S3 prefix.

### Key Technologies
*   **axum:** High-performance async REST API.
*   **LanceDB:** Vector and BM25 search engine that runs natively on object storage.
*   **foyer:** Advanced hybrid cache (RAM + NVMe) with LFU/LRU eviction.
*   **Prometheus:** Full operational visibility into cache hits, misses, and S3 request savings.

## Features

*   **Multi-tenant by Design:** Each namespace maps to an isolated S3 prefix (`s3://bucket/namespace/`) with near-zero idle cost.
*   **Instant Invalidation:** A "Generation Counter" strategy ensures that after a write, all stale search results for that namespace are invalidated in $O(1)$ time.
*   **CAS Consistency:** Verified concurrency safety using S3's `If-None-Match: *` to prevent data loss when multiple writers fight for the same bucket.
*   **Zero-Copy Ready:** Optimized serialization via `bincode` (with architectural triggers to move to `rkyv` if needed).
*   **Operational Excellence:** Native Prometheus metrics tracking cache hit rates and S3 request count (the primary signal for cost savings).

## Quickstart

### 1. Launch the Stack
Everything you need (MinIO storage + Firnflow API) is orchestrated via Docker Compose:

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
| `/ns/{ns}/warmup` | `POST` | Non-blocking cache pre-warm hint |

## Development and Benchmarking

firnflow uses a containerized toolchain. No local Rust installation is required.

```bash
# Run the full test suite (requires MinIO)
./scripts/cargo test --workspace -- --ignored

# Run the cold-vs-warm latency benchmark
./scripts/cargo run --release -p firnflow-bench
```

Benchmark results are committed at `bench/results/cold_vs_warm.md`.
