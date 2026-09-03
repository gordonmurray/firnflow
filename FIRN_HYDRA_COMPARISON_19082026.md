# Firn and HydraDB Comparison

Reviewed 19 August 2026 against HydraDB commit [`6a2fbb1`](https://github.com/hydra-db/hydradb/tree/6a2fbb192f37f51a93690a2ae2d2f5e27e6e4219) and Firn commit `a5f0a873964f16280ae8f105d71a4a90833a6bf8`.

The short version: HydraDB does cache extensively, but Firn does not need another cache library or a storage-engine change. Firn already has stronger cache coverage and more realistic cache evidence. HydraDB's most useful lessons are operational: bound every expensive resource, prevent cache pollution and thundering herds, control metric cardinality, test partial object-store failures, and make cache cost visible.

## What HydraDB caches

HydraDB has several distinct cache layers:

- SlateDB's block/object cache on local SSD. SlateDB is compiled with its `foyer` feature, so HydraDB uses Foyer indirectly. Writers can cache newly flushed or compacted SSTs, readers can optionally preload SSTs, and WAL replay concurrency is bounded. [HydraDB cache configuration](https://raw.githubusercontent.com/hydra-db/hydradb/6a2fbb192f37f51a93690a2ae2d2f5e27e6e4219/src/core/config.rs)
- Custom in-memory graph caches for parsed queries, relationship rows, adjacency data, compiled GraphBLAS matrices, and native path results.
- Those custom caches have entry limits, resident-byte limits, per-tenant quotas, LRU eviction, optional pinning, and insertion/eviction/quota metrics. [BoundedGraphCache implementation](https://raw.githubusercontent.com/hydra-db/hydradb/6a2fbb192f37f51a93690a2ae2d2f5e27e6e4219/src/core/cache.rs)
- Large scans normally bypass SlateDB's block cache. Only scans expected to return at most 1,024 items are admitted, preventing one large traversal from evicting a useful working set. [Scan cache admission](https://raw.githubusercontent.com/hydra-db/hydradb/6a2fbb192f37f51a93690a2ae2d2f5e27e6e4219/src/codec.rs)
- Open tenant scopes are retained in an LRU-bounded pool. Only idle scopes are evicted, and new scopes are rejected when every retained scope is actively referenced.
- Hydration, matrix compilation, writes, index generation, and garbage collection have separate semaphores rather than competing through one generic limit.

Firn already has the analogous foundations:

- A shared Foyer RAM and NVMe exact-result cache in [layer.rs](crates/firnflow-core/src/cache/layer.rs#L14).
- A semantic result sidecar in [semantic.rs](crates/firnflow-core/src/cache/semantic.rs#L47).
- A persistent local byte-range object cache with immutable-object filtering and identical-miss singleflight in [object_cache.rs](crates/firnflow-core/src/object_cache.rs#L17).
- Warm Lance connection and table handles in [manager.rs](crates/firnflow-core/src/manager.rs#L195).

So Firn should not add SlateDB, replace LanceDB, or replace its direct Foyer integration. The opportunity is policy around the existing caches.

## Highest-value additions for Firn

| Priority | Addition | Why it matters |
|---|---|---|
| P0 | Resource admission and backpressure | Prevents concurrent queries, warmups, imports, index builds, and compactions from exhausting memory, CPU, local disk, or object-store request budgets. |
| P0 | Bound all namespace-scoped process state | Firn's handle pool, schema map, generation map, semantic namespace map, and Prometheus series can currently grow with every namespace ever touched. |
| P0 | Fix metric cardinality and add exact backend-cost metrics | Current `namespace` labels can create unbounded Prometheus series, while `s3_requests_total` is only a service-level approximation. |
| P1 | Query-result singleflight | A burst of identical cold queries can all miss and perform the same Lance query. Firn already solves this one level lower for identical object ranges. |
| P1 | Operation-aware cache admission | Compaction, indexing, bulk scans, and imports should not be allowed to evict a query working set accidentally. |
| P1 | Graceful shutdown and readiness | Firn has a cache `close()` method, but the server does not call it during signal-driven shutdown or drain background work. |
| P1 | Failure-oriented object-store tests | Deterministic GET, PUT, LIST, and DELETE failures would exercise cache and lifecycle behavior that ordinary in-memory tests miss. |
| P2 | Cache-on-write experiment | Newly created immutable data/index objects could become locally warm without a later read from object storage, but multipart writes and memory use make this an experiment, not an automatic port. |
| P2 | Optional OTLP and centralized redaction | Useful once Firn needs cross-service traces. The cardinality and redaction foundation should come first. |

### 1. Add a process-wide resource policy

Firn's rate limiting controls request arrival rate, not simultaneous resource consumption. Background endpoints call `tokio::spawn` directly, while running operation records are never evicted. See [operations.rs](crates/firnflow-api/src/operations.rs#L1) and [handlers.rs](crates/firnflow-api/src/handlers.rs#L358).

Introduce separate configurable limits for:

- Concurrent foreground queries.
- Concurrent writes/imports.
- Concurrent index builds and compactions.
- Concurrent warmup operations.
- Concurrent object-cache fills.
- Maximum open namespace handles.
- Query runtime and admission wait time.
- Maximum queued and running background operations.

These should not all share one semaphore. A compaction should not consume the final foreground-query permit, and hundreds of warmups should not create an unbounded task queue.

For background work, either reject at admission with `429` or `503` and `Retry-After`, or add a genuinely bounded queue and expose `queued` as an operation state. Acquiring a permit only after spawning would leave the task and operation-record growth problem intact.

### 2. Bound namespace state as one lifecycle

The handle pool and schema cache are unbounded `DashMap`s today. Concurrent opens can also duplicate the expensive `connect` and `open_table` work before one handle wins the insertion race. [Current manager path](crates/firnflow-core/src/manager.rs#L489)

The same namespace also leaves state in:

- `NamespaceManager::handles`
- `NamespaceManager::schema_info`
- `GenerationCounter`
- `SemanticCache::inner`
- `CoreMetrics::seen_namespaces`
- Prometheus vector label sets

Create a bounded namespace-state registry with:

- LRU timestamps.
- Idle-only eviction.
- A per-namespace open gate to prevent duplicate cold opens.
- Removal of schema, semantic, and generation state on deletion or final eviction.
- A hard admission response if every namespace is active.
- Metrics for entries, hits, misses, evictions, admission rejections, and open duration.

HydraDB's implementation is more complicated because it must close SlateDB writers safely. Firn can likely use a simpler version because Lance table handles are cheap and cloneable.

### 3. Singleflight exact-result misses

Firn's object cache correctly serializes concurrent misses for the same byte range, then rechecks after acquiring the gate. The exact-result cache does not. Its flow is currently get, run the backend query, then insert. [Exact cache path](crates/firnflow-core/src/cache/layer.rs#L60)

For a popular query immediately after restart or invalidation, every concurrent request can therefore run the same expensive query. Add a key-scoped in-flight gate:

1. Check Foyer.
2. Acquire the `CacheKey` gate.
3. Recheck Foyer.
4. Let one request run the backend query.
5. Wake the followers to read the populated entry.
6. Remove the gate on success, failure, timeout, or cancellation.

Apply the same pattern to namespace opens. Tests should prove that 100 concurrent identical requests produce one backend execution and that a failed or cancelled leader does not strand followers.

### 4. Add cache fairness and real byte accounting

Firn's exact result cache is globally bounded but has no namespace fairness. One tenant issuing unique queries can churn another tenant's hot results. The semantic cache has a 1,024-entry limit per namespace, but:

- It has no process-wide byte limit.
- The number of namespaces is unbounded.
- Each semantic entry stores another copy of the serialized result.
- Entry count is a weak proxy when dimensions and result sizes differ significantly.

HydraDB's byte and tenant accounting is a useful model, although its implementation currently has a known hole where one result cache gets a second memory budget and is omitted from resident-byte reporting. [HydraDB issue #73](https://github.com/hydra-db/hydradb/issues/73)

For Firn, add:

- Total and per-namespace resident-byte budgets for the semantic cache.
- Per-namespace admission or fair-share protection for the exact-result cache.
- Resident bytes, entry count, capacity, admissions, replacements, and evictions by reason.
- Object-cache bytes served locally, not only bytes fetched remotely.
- Bypass counters by closed reason: mutable object, conditional request, unsupported range, oversized entry, maintenance operation.
- In-flight fill count and wait duration.

Rigidly dividing disk by namespace, as HydraDB sometimes does, can waste space. A global cache with a per-namespace ceiling or protected minimum is preferable for Firn.

### 5. Keep maintenance scans from polluting the object cache

Firn already bypasses the result cache for `/list`, explicitly to avoid filling it with pagination tails. That principle should extend to the object cache.

The object cache currently admits every cacheable immutable range below the entry-size cap. Consequently, an index build, compaction, or large scan can potentially replace query-hot index ranges with data that will not be read again.

Add operation context or separate cached and uncached Lance sessions so policy can distinguish:

- Foreground query reads: normally cache.
- Small metadata-driven reads: cache when immutable.
- Full scans and exports: bypass.
- Compaction source reads: usually bypass.
- Index-build source reads: benchmark both policies.
- New index output: potentially cache on write.

HydraDB's fixed 1,024-item threshold is workload-specific. Firn should derive its threshold from range bytes and measured reuse rather than copying that number.

### 6. Make object-store cost observable in production

Firn's existing `firnflow_s3_requests_total` is intentionally approximate and now also covers GCS-backed operations. [Current metric definition](crates/firnflow-core/src/metrics.rs#L188)

The stronger backend counters used in the one-million-document benchmark were instrumentation-only. Promote that idea into normal production:

- `firnflow_object_store_requests_total{provider,method,outcome}`
- `firnflow_object_store_read_bytes_total{provider}`
- `firnflow_object_store_write_bytes_total{provider}`
- `firnflow_object_store_request_duration_seconds{provider,method}`
- `firnflow_object_cache_served_bytes_total`
- `firnflow_object_cache_bypass_total{reason}`

Keep label vocabularies closed. Do not put namespace, path, query hash, object key, or request ID on metrics.

This matters because cache savings are workload-dependent. Firn's own real-S3 evidence already shows that:

- Single-vector warm novel queries dropped from roughly 214 ms without the object cache to about 7 ms with it. [First-query results](bench/results/first_query_profile_objcache.md#per-case-latency)
- On the one-million-document multivector workload, cache-on reduced backend bytes by about 130x while latency stayed CPU-bound and essentially flat. [Multivector cache study](bench/results/beir_multivector_objcache.md#the-same-ab-at-1m-documents-nq-a-true-cache-off-arm-130-bytes)

That is a much better product story than a universal "cache makes queries faster" claim.

### 7. Fix Prometheus cardinality before adding more metrics

Nearly every major Firn metric carries a raw namespace label, and Prometheus retains every label set created during the process lifetime. `seen_namespaces` also only grows. [Metrics implementation](crates/firnflow-core/src/metrics.rs#L45)

This is the most immediate monitoring concern found in this comparison.

Migrate to aggregate metrics using closed labels such as:

- `query_type`
- `cache_source`
- `operation`
- `outcome`
- `error_class`
- `provider`
- `bypass_reason`

Put namespace, request correlation, query fingerprints, and detailed planner information in structured spans and logs instead.

HydraDB explicitly partitions telemetry attributes into safe metric labels and span-only attributes, with tests enforcing the partition. It also centrally redacts query parameters, properties, credentials, and bookmarks at every telemetry sink. [Cardinality registry](https://raw.githubusercontent.com/hydra-db/hydradb/6a2fbb192f37f51a93690a2ae2d2f5e27e6e4219/crates/telemetry/src/semconv.rs) [Central redaction](https://raw.githubusercontent.com/hydra-db/hydradb/6a2fbb192f37f51a93690a2ae2d2f5e27e6e4219/crates/telemetry/src/redact.rs)

Firn could adopt that pattern without immediately adopting OpenTelemetry.

### 8. Graceful shutdown and readiness

Firn has `NamespaceCache::close()`, which flushes Foyer's NVMe state, but the server simply awaits `axum::serve`; there is no signal handler, admission shutdown, operation drain, or cache close. [main.rs](crates/firnflow-api/src/main.rs#L25)

Add:

- Separate `/health` liveness and `/ready` readiness.
- Signal-driven graceful shutdown.
- Stop accepting new queries and background operations.
- Wait for in-flight work up to a configurable deadline.
- Mark unfinished operations cancelled or failed.
- Call `NamespaceCache::close()`.
- Flush or close other persistent cache state.
- Tests for shutdown during a cache fill, import, and index build.

This is both an operational-correctness improvement and a cache-performance improvement after routine pod termination.

## Tests worth adapting

The most useful HydraDB test technique is a per-operation fault-injecting `ObjectStore` decorator. It can fail the next N or all GET, PUT, LIST, DELETE, or COPY operations independently and counts attempts, including failed ones. That enables partial-failure tests rather than only total outage tests.

For Firn, build an Apache-compatible equivalent and test:

- GET failure during an object-cache miss leaves no admitted partial file.
- The next request retries and succeeds.
- Concurrent followers recover when the singleflight leader fails.
- Conditional and mutable reads always bypass cached bytes.
- Delete and recreate cannot expose the previous table's cached bytes.
- LIST can fail while GET and PUT remain healthy.
- Cache restart removes temporary files and honors the byte cap.
- A query timeout releases all permits and in-flight gates.
- Namespace LRU never evicts an actively referenced handle.
- A tenant at its quota cannot evict every other tenant's working set.
- Graceful shutdown waits for cache persistence and terminates boundedly.

Also adopt HydraDB's telemetry-invariant style:

- Every metric has an allowed, closed label set.
- No newly registered metric may carry namespace.
- Every error increments exactly one coarse error class plus the total.
- Histogram units and bucket definitions are consistent.
- Redaction matches whole names or dotted suffixes, not accidental substrings.

Keep Prometheus's histogram implementation initially. HydraDB's custom lock-free fixed-bucket histogram is thoughtful, but it adds bespoke correctness surface. [HydraDB histogram rationale](https://raw.githubusercontent.com/hydra-db/hydradb/6a2fbb192f37f51a93690a2ae2d2f5e27e6e4219/src/core/histogram.rs) Firn should first set explicit buckets spanning sub-millisecond cache hits through its query deadline, then profile whether metric recording is actually material.

## Benchmark lessons

HydraDB's public cache data shows roughly 25x to 56x cold-to-hot differences for its one-hop workloads, but the benchmark is in-process and uses same-host Docker MinIO. Its own methodology correctly says those results omit network, protocol, authentication, and server overhead, and that local MinIO hides the latency HydraDB is designed to avoid. [Published data](https://raw.githubusercontent.com/hydra-db/benchmark/main/docs/data/minio.json) [Methodology and caveats](https://raw.githubusercontent.com/hydra-db/benchmark/main/METHODOLOGY.md)

Firn should borrow these methodological details:

- Wipe the cache and restart the process for every cold benchmark cell.
- Use an outer process driver, not merely reconstructed Rust objects in one runtime.
- Separate exact-result hits, semantic hits, warm object-cache novel queries, and genuine cold queries.
- Report backend requests and bytes, not only latency.
- Report CPU utilization and RSS alongside concurrency.
- When throughput plateaus, distinguish lock waiting from CPU or memory-bandwidth saturation.
- Preserve raw samples and machine-readable metadata, including an actual engine commit SHA.

Firn is already ahead because its cache work uses real in-region S3, disjoint novel-query sets, cache-hit guardrails, and direct backend-byte measurements. The outstanding gap is the true one-process-per-sample cold harness already identified in Firn's own report.

## What not to bring over

Firn should not adopt:

- SlateDB as a replacement for LanceDB.
- GraphBLAS, OpenCypher, Bolt, WAL overlays, writer leases, or the separate indexer architecture. Those solve HydraDB's graph and distributed-writer problems, not Firn's retrieval workload.
- HydraDB's custom cache implementation verbatim. Firn already has Foyer and a byte-bounded object cache.
- Automatic SST-style preloading. Firn should warm only measured hot query/index ranges.
- Cache-on-write without an experiment. Lance multipart writes and large fragments make naive payload capture risky.
- HydraDB benchmark numbers as evidence for Firn.

HydraDB is also a young project and should be treated as a source of design ideas, not a production baseline. At this snapshot, its public checkout references missing benchmark scripts and absent formal-test targets, and current issues report missing pull-request CI and incomplete cache accounting. [Missing scripts issue](https://github.com/hydra-db/hydradb/issues/88) [Missing CI issue](https://github.com/hydra-db/hydradb/issues/90) Its code is AGPL-3.0, while Firn is Apache-2.0, so concepts can be independently reimplemented, but code and tests should not be copied verbatim without an explicit licensing review. [HydraDB license](https://github.com/hydra-db/hydradb/blob/6a2fbb192f37f51a93690a2ae2d2f5e27e6e4219/LICENSE)

## Recommended sequence

1. Add always-on backend request/byte instrumentation and remove raw namespace labels from new metrics.
2. Introduce separate foreground, maintenance, import, warmup, and hydration admission limits.
3. Add graceful shutdown, `/ready`, deadlines, and operation draining.
4. Add exact-query and namespace-open singleflight.
5. Bound namespace state and semantic-cache resident bytes with tenant fairness.
6. Add operation-aware object-cache bypass and benchmark it.
7. Build the fault-injecting object-store test harness.
8. Run the outer-process cold/cache/concurrency matrix.
9. Only then evaluate cache-on-write and optional OTLP export.
