# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-05-02

### Added
- `POST /ns/{namespace}/scalar-index` — builds a BTree scalar index on the reserved `_ingested_at` column asynchronously (returns 202 Accepted). With the index in place, `/list` cursor pages do an index range scan instead of a full-fragment scan and the leading `ORDER BY _ingested_at` short-circuits the in-memory sort. The build runs in a tokio task and reports duration via `firnflow_index_build_duration_seconds{kind="scalar"}`. Idempotent (repeat calls rebuild in place); the cached connection/table handle is evicted on success in line with the existing manifest-bump rationale used by `/index` and `/fts-index`. `POST /compact` already runs `optimize_indices` after the file compaction step, so the BTree absorbs new rows incrementally — no separate rebuild trigger is needed after compaction. Closes #24.
- DigitalOcean Spaces is a validated storage backend. The `If-None-Match: *` pre-flight returns 412 on the second PUT and a 100-iteration concurrent-writer stress run produced 800/800 rows on every iteration with zero discrepancies, validated against `firn-sample-bucket` in the London (`lon1`) region. Per-iteration wall time is ~3.10 s, the same performance class as AWS `eu-west-1` and the fastest non-AWS backend tested. The README compatibility matrix is updated; deployment requires the regional endpoint (`https://<region>.digitaloceanspaces.com`) and path-style addressing, the same client-side quirk that affects Cloudflare R2 and Tigris on `object_store` 0.12. Test functions live alongside the existing per-provider blocks in `crates/firnflow-core/tests/s3_conditional_writes.rs` and `crates/firnflow-core/tests/lance_concurrent_writes.rs`. Closes #29.

### Changed
- Tigris is now a validated storage backend. The 2026-04-17 concurrent-stress failure (silent write loss under contention on both dual-region and single-region buckets) was fixed upstream. A 2026-04-19 re-run of the 100-iteration stress passed cleanly on both `firn-tigris-bucket` (dual-region, 375 s) and `firn-tigris-single-region` (291 s). The README compatibility matrix and `notes/providers/tigris.md` are updated; the original failure record is preserved in the provider writeup for provenance.

## [0.3.0] - 2026-04-18

### Added
- `GET /ns/{namespace}/list` — a narrow, cursor-paginated endpoint for "recent content" flows. Ordered by a new reserved system column `_ingested_at` (microsecond timestamp, populated at first write, never mutated). Supports `order_by=_ingested_at` only in v1, `order=asc|desc`, `limit` (default 50, capped at 500), and an opaque hex cursor. Bypasses the foyer cache so pagination tails do not pollute hot query entries (issue #22).
- `NamespaceManager::list` + `encode_list_cursor` / `decode_list_cursor` helpers. Cursor format is a 32-char hex encoding of `(timestamp_micros, id)` for stable continuation under concurrent writes.
- `FirnflowError::Unsupported` variant mapped to HTTP 501 for namespaces whose tables pre-date the `_ingested_at` column.
- `firnflow_s3_requests_total{operation="list"}` is now recorded for every list call so `/list` participates in the cost-visibility story even though it bypasses `NamespaceService`.

### Changed
- `NamespaceManager` now caches `(dim, has_ingested_at)` per namespace (`schema_info` replaces the old `dims` DashMap). Existing namespaces without `_ingested_at` continue to accept upserts against their original schema; only the `/list` endpoint rejects them with 501.

## [0.2.0] - 2026-04-14

### Added
- Per-namespace connection pool inside `NamespaceManager`. The
  `lancedb::Connection` and `Table` handle for each namespace are
  cached after the first open and reused across subsequent
  upserts, queries, index builds, and compactions. The pool is
  evicted on `delete`, `create_index`, `create_fts_index`, and
  `compact`; ordinary append-only upserts do not evict. First run
  against MinIO measured a cold upsert at ~108 ms and the warm
  upsert at ~8 ms on the same hardware (issue #1).
- New Prometheus gauge `firnflow_cached_handles` exposed at
  `/metrics`. Compared against `firnflow_active_namespaces` it
  surfaces namespaces that will still pay the cold-open cost on
  their next request.
- Documentation updates in `docs/monitoring.html`,
  `docs/architecture.html`, and `docs/quickstart.html` describing
  the pool and the new gauge.

### Changed
- `NamespaceManager::new()` now takes `Arc<CoreMetrics>` as a
  third argument so the manager can drive the pool gauge. Every
  call site in `firnflow-api`, the bench harness, and the
  integration tests updated accordingly.
- License corrected to Apache-2.0 across the repository. The
  `LICENSE` file now matches the `license = "Apache-2.0"`
  declaration already present in the workspace `Cargo.toml`,
  replacing the MIT text that was committed in error.

## [0.1.0] - 2026-04-13

Initial public release. The repository had been under active
development through phases 1 through 8 before being made public;
`v0.1.0` marks the first tagged artifact.

### Highlights present at 0.1.0
- Multi-tenant S3-backed vector and full-text search engine
  combining LanceDB (vector + BM25 on object storage) with foyer
  (RAM + NVMe hybrid cache) behind an axum REST API.
- Namespace manager with per-namespace vector dimensions, lazy
  namespace creation, and full cleanup on delete.
- Cache-aside read path with after-success invalidation. Keyed on
  `(namespace, generation, query_hash)` using a per-namespace
  atomic generation counter for O(1) invalidation (ADR-001).
- bincode-2 serialisation path for cached result sets with a
  100-result p99 round-trip well inside the 1 ms budget (ADR-002).
- IVF_PQ vector indexing via `POST /ns/{ns}/index`, BM25 FTS
  indexing via `POST /ns/{ns}/fts-index`, compaction via
  `POST /ns/{ns}/compact`. All three run as non-blocking
  background tasks and return 202.
- Three query modes: vector-only, FTS-only, and hybrid via
  Reciprocal Rank Fusion.
- Prometheus metrics surface: cache hits/misses, S3 request
  counter, query and write duration histograms, index-build and
  compaction duration histograms, active-namespaces gauge.
- Published Docker image at `ghcr.io/gordonmurray/firnflow` and
  documentation at https://firnflow.io.
- Validated against MinIO and real AWS S3 in `eu-west-1`. Honest
  benchmark at dim=1536, 100k rows available at
  `bench/results/cold_vs_warm_aws.md`.

[Unreleased]: https://github.com/gordonmurray/firnflow/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/gordonmurray/firnflow/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/gordonmurray/firnflow/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/gordonmurray/firnflow/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/gordonmurray/firnflow/releases/tag/v0.1.0
