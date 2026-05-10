# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Two ignored tests that verify GCS's native conditional-write precondition (`x-goog-if-generation-match: 0` on the GCS XML API) through the `object_store::gcp` client with `PutMode::Create`, both passing cleanly against `firn-gcs-bucket` on 2026-05-10. `crates/firnflow-core/tests/s3_conditional_writes.rs` now contains a sequential pre-flight (two creates against the same key — the first must succeed, the second must surface as `object_store::Error::AlreadyExists`) and a contended-key microstress (8 writers, gated on a `tokio::sync::Barrier`, race the same key for 100 iterations; exactly one wins, the other seven each see `AlreadyExists`). Distinct-key stress is omitted on purpose — every writer succeeds when there is no contention, so it does not exercise CAS at all. The tests run against a service-account JSON via `GOOGLE_APPLICATION_CREDENTIALS` (or `GOOGLE_SERVICE_ACCOUNT_PATH` / `GOOGLE_SERVICE_ACCOUNT_KEY`) plus `GCS_BUCKET`. They share the same code path that future native-GCS Firn support will rely on, so the verification exercises the production mechanism rather than a third-party HTTP shape. The `gcp` feature on `object_store` is enabled as a dev-only dependency in `firnflow-core`; the production feature surface stays on `aws` until native GCS support lands. `scripts/cargo` now mounts a service-account JSON into the dev container and passes the standard `GOOGLE_*` env vars through, so the new tests can be run end-to-end through the existing toolchain wrapper. The README compatibility matrix is updated to record the empirical result rather than asserting "works correctly" on Google's docs alone. GCS remains ❌ in the supported-backend column until a follow-up release routes Firn's writes through LanceDB's native GCS backend instead of the S3-interop endpoint. Closes #36.

## [0.5.0] - 2026-05-04

### Added
- Bearer-token authentication on the REST API. Two static keys, both opt-in via env: `FIRNFLOW_API_KEY` for the read/write tier (`upsert`, `query`, `list`, `warmup`) and `FIRNFLOW_ADMIN_API_KEY` for the destructive tier (`delete`, `index`, `fts-index`, `scalar-index`, `compact`). When neither is set the API stays open with a single startup `WARN`, preserving 0.4.x behaviour for existing dev compose stacks. Header format is the standard `Authorization: Bearer <token>`; comparisons are constant-time via `subtle::ConstantTimeEq`. If only `FIRNFLOW_API_KEY` is configured, it authorises admin routes too (single-key fallback). Status codes: missing/malformed/unknown token → 401 with `WWW-Authenticate: Bearer realm="firnflow"`; valid token but insufficient scope → 403. **Service-level only** — any holder of `FIRNFLOW_API_KEY` can read or write any namespace. Per-tenant namespace isolation requires an authenticating gateway in front of firnflow.
- Optional bearer-token gate for `/metrics` via `FIRNFLOW_METRICS_TOKEN`. Same Bearer parser, so Prometheus's `bearer_token` / `bearer_token_file` scrape config works unchanged. `/metrics` stays public when the token is unset (preserves 0.4.x behaviour).
- Optional per-principal token-bucket rate limiting via `tower-governor`. `FIRNFLOW_RATE_LIMIT_RPS` (sustained rate) and `FIRNFLOW_RATE_LIMIT_BURST` (default 30) bucket on the validated `Principal` extension that the auth middleware attaches; this means a bogus token never reaches the limiter — auth has already returned 401 — so an attacker cannot mint fresh buckets by rotating tokens. `/health` and `/metrics` are exempt. Rejected requests return 429 with `Retry-After`.
- Optional pre-auth IP-keyed limiter via `FIRNFLOW_PREAUTH_IP_LIMIT_RPS`. Wraps the protected sub-router as the outermost layer and caps credential-stuffing throughput per peer IP. Off by default because most operators front firnflow with a CDN or API gateway that already does this.
- `FIRNFLOW_TRUST_PROXY_HEADERS` (default `false`). When `true`, the IP extractor trusts the leftmost entry in `X-Forwarded-For` / `X-Real-IP`. Default is to read peer IP from the connection only — a deployment exposed directly cannot have its bucket key forged.
- New Prometheus counter `firnflow_auth_rejections_total{reason}` with reasons `missing` (no Authorization header), `invalid` (header present, token does not match), `forbidden` (valid token, insufficient scope), `rate_limited` (shed by either limiter). Use this to detect misconfigured keys after a rotation or credential-stuffing pressure.
- Startup-time validation: `firnflow-api` refuses to start when `FIRNFLOW_API_KEY` and `FIRNFLOW_ADMIN_API_KEY` are configured to the same value. The auth middleware checks the admin key first, so identical bytes would silently classify every authenticated request as admin and collapse the scope split. The error message tells the operator to either set a distinct admin key or unset `FIRNFLOW_ADMIN_API_KEY` to engage the documented single-key fallback. Validation runs at the very top of `build_state`, before metrics, manager, cache directory, or cache initialisation, so a config error is reported directly rather than being masked by an unrelated infrastructure failure.
- `AppConfig` now has a custom `Debug` impl that redacts values for credential-bearing entries in `storage_options` (anything whose key contains `secret`, `password`, `token`, `access_key`, or `credential`, case-insensitive). `aws_secret_access_key` and `aws_access_key_id` are masked; non-sensitive options like `aws_endpoint`, `aws_region`, and `allow_http` remain visible because they are useful for diagnosing config. The integration regression test now exercises a populated `storage_options` map to pin this.
- Closes #2.

### Changed
- `ApiError` is now an enum with explicit `Core(FirnflowError)`, `Unauthorized`, `Forbidden`, and `RateLimited(Duration)` variants. `From<FirnflowError>` is preserved so handlers continue to use `?` against core errors. Not a public stability commitment, but flagged for downstream callers.
- `AppState` carries an `Arc<AuthConfig>` plus a `RateLimitSettings` field. Integration tests construct `AppState` through a single `tests/common::test_state()` helper rather than the previous per-test inline literal — adding a future field is a one-line change instead of ten.
- `firnflow-api` now mounts the router with `into_make_service_with_connect_info::<SocketAddr>()` so the auth middleware and IP rate limiter can read peer IPs.

## [0.4.0] - 2026-05-02

### Added
- `POST /ns/{namespace}/scalar-index` — builds a BTree scalar index on the reserved `_ingested_at` column asynchronously (returns 202 Accepted). With the index in place, `/list` cursor pages do an index range scan instead of a full-fragment scan and the leading `ORDER BY _ingested_at` short-circuits the in-memory sort. The build runs in a tokio task and reports duration via `firnflow_index_build_duration_seconds{kind="scalar"}`. Idempotent (repeat calls rebuild in place); the cached connection/table handle is evicted on success in line with the existing manifest-bump rationale used by `/index` and `/fts-index`. `POST /compact` already runs `optimize_indices` after the file compaction step, so the BTree absorbs new rows incrementally — no separate rebuild trigger is needed after compaction. Closes #24.
- DigitalOcean Spaces is a validated storage backend. The `If-None-Match: *` pre-flight returns 412 on the second PUT and a 100-iteration concurrent-writer stress run produced 800/800 rows on every iteration with zero discrepancies, validated against `firn-sample-bucket` in the London (`lon1`) region. Per-iteration wall time is ~3.10 s, the same performance class as AWS `eu-west-1` and the fastest non-AWS backend tested. The README compatibility matrix is updated; deployment requires the regional endpoint (`https://<region>.digitaloceanspaces.com`) and path-style addressing, the same client-side quirk that affects Cloudflare R2 and Tigris on `object_store` 0.12. Test functions live alongside the existing per-provider blocks in `crates/firnflow-core/tests/s3_conditional_writes.rs` and `crates/firnflow-core/tests/lance_concurrent_writes.rs`. Closes #29.

### Changed
- Tigris is now a validated storage backend. The 2026-04-17 concurrent-stress failure (silent write loss under contention on both dual-region and single-region buckets) was fixed upstream. A 2026-04-19 re-run of the 100-iteration stress passed cleanly on both `firn-tigris-bucket` (dual-region, 375 s) and `firn-tigris-single-region` (291 s). The README compatibility matrix is updated.

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
  atomic generation counter for O(1) invalidation.
- bincode-2 serialisation path for cached result sets with a
  100-result p99 round-trip well inside the 1 ms budget.
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

[Unreleased]: https://github.com/gordonmurray/firnflow/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/gordonmurray/firnflow/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/gordonmurray/firnflow/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/gordonmurray/firnflow/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/gordonmurray/firnflow/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/gordonmurray/firnflow/releases/tag/v0.1.0
