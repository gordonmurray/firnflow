# Implementation plan — query-time metadata filtering ([#84])

[#84]: https://github.com/gordonmurray/firnflow/issues/84

Adds **filtered retrieval** — running a vector / FTS / hybrid query scoped to a
predicate over row attributes (e.g. nearest neighbours *where* `section =
'warnings'` and `route = 'ORAL'`). Per the issue, this is two layered gaps,
split into two PRs because they are different sizes:

- **Part 1 — the filter mechanism.** A `filter` predicate on `/query`, threaded
  to LanceDB's prefilter. Shippable now against the existing `id` /
  `_ingested_at` columns. One PR.
- **Part 2 — arbitrary scalar columns.** Carry user-defined scalar attributes
  through upsert/import → schema → results, so there is something beyond `id` /
  `_ingested_at` to filter on. Has real design questions; design-first, build
  after sign-off.

## Decisions locked

- **Scope:** plan only for now; Part 1 is the next code PR.
- **Malformed predicate → HTTP 400** (`InvalidRequest`), not 500.
- **Python parity:** Part 1 includes a `search(filter=...)` parameter on the
  embedded `firn` package.

## What was verified in the code

- `QueryRequest` (`crates/firnflow-core/src/query.rs:34`) has no predicate field
  today — `vector` / `vectors` / `k` / `nprobes` / `text` / `include_vector` /
  `semantic_cache`.
- `NamespaceManager::query` (`crates/firnflow-core/src/manager.rs:986`) builds
  two LanceDB query paths — the vector/hybrid builder (`manager.rs:1109-1137`)
  and the FTS-only builder (`manager.rs:1138-1151`). Neither sets a predicate.
- `/list` already does `scan.filter(...)` (`manager.rs:1425`), confirming the
  DataFusion SQL dialect.
- The exact-cache key is `hash_query_for_cache` (`service.rs:475`), a bincode
  tuple of the cacheable request fields.
- Semantic eligibility is checked in **two** places that must stay in lockstep:
  `validate_semantic_cache_request` (`query.rs:134`) and the `semantic_eligible`
  flag (`service.rs:281`).
- LanceDB pinned `=0.29.0`, lance `=6.0.0`. `only_if` is not used anywhere yet.
- Error → status mapping: `InvalidRequest → 400`, `Unsupported → 501`,
  `Backend → 500` (`crates/firnflow-api/src/error.rs:90-112`).

## Test-quality bar (the target to meet or exceed)

The closest shipped analog is `include_vector` / result projection. Its coverage
envelope was **6 tests**: core manager round-trip + 3 core service
cache-key/semantic-interaction tests + API wire-format + semantic interaction.

House style for tests in this repo:

- Integration tests are `#[tokio::test] #[ignore]` (MinIO-gated), driven through
  the axum router with tower `oneshot`, isolated via `unique_namespace(prefix)`.
- `crates/firnflow-api/tests/common/mod.rs` provides `test_state()` (MinIO),
  `test_state_offline()` (no S3), `unique_namespace`, `minio_options`.
- Error assertions: `match err { FirnflowError::InvalidRequest(msg) =>
  assert!(msg.contains(..)), other => panic!(..) }`.
- HTTP assertions: `assert_eq!(status, StatusCode::OK)` then
  `body["field"].as_array()/.as_i64()/.as_str()`.
- No rstest / proptest / table-driven macros — one function per scenario, with
  pure validators (`validate_ivf_pq_options`, `validate_semantic_cache_request`,
  `validate_scalar_index_column`, `validate_arrow_import_schema`) unit-tested
  inline.
- Run: `docker compose up -d minio minio-init` then
  `./scripts/cargo test -- --ignored`. CI runs
  `cargo test --workspace -- --ignored --skip _aws --skip _100_runs_`.

---

## Part 1 — the filter mechanism (one PR)

### Step 0 — prove the API

Confirm `only_if(impl Into<String>)` compiles on lancedb `0.29`'s `QueryBase`
for **both** the `VectorQuery` (vector/hybrid) and `Query` (FTS-only) builders.
First commit's smoke check.

### Production changes

| # | File:line | Change |
|---|---|---|
| 1 | `query.rs:34` | Add `#[serde(default)] pub filter: Option<String>` to `QueryRequest`. Document it as a DataFusion SQL predicate, same dialect as `/list`. Note prefilter semantics: `only_if` filters **before** kNN, so you get *k neighbours satisfying the predicate*, not a filtered top-k. |
| 2 | `query.rs:134` | In `validate_semantic_cache_request`, reject `semantic_cache.enabled && filter.is_some()` with `InvalidRequest` (mirror the existing `text` rejection). The doc-comment at `query.rs:91` already promises "no text/filters" — now honored. |
| 3 | `query.rs:286` | Update the `req_vector_only()` test helper for the new field. |
| 4 | `service.rs:475` | Add `filter` to the `Canonical` struct in `hash_query_for_cache` — filtered / unfiltered / differently-filtered queries must not collide. |
| 5 | `service.rs:281` | Add `&& req.filter.is_none()` to `semantic_eligible` (two-layer defense, matching the existing pattern). |
| 6 | `service.rs:346` | Pass `req.filter.clone()` into `manager.query(...)`. |
| 7 | `manager.rs:986` | Add `filter: Option<String>` to `query()`; apply `.only_if(f)` to the vector/hybrid builder (`~1128`) **and** the FTS-only builder (`~1141`). |
| 8 | `manager.rs:1135` / `1148` | **Malformed-predicate → 400:** when `filter.is_some()` and `execute()` errors, remap to `FirnflowError::InvalidRequest` (carrying Lance's message) instead of `Backend`. Keep `Backend` for the unfiltered path. |
| 9 | `python/src/lib.rs:412` | Add `filter` to the `QueryRequest` literal; add `filter: Option<String>` to `op_search` and a `filter=None` keyword to `search()` (`lib.rs:623-624` signature + `#[pyo3(signature=...)]`). |
| 10 | API handler | **No change** — `handlers.rs:187` deserializes `QueryRequest` directly; the field flows through. |
| 11 | docs | `docs/api.html` query section, README query example, `CHANGELOG.md` `[Unreleased]` → Added. |

### Test envelope (Part 1)

~15 tests vs the 6 the analogous feature shipped — extra weight on the new
behavior (predicate across all three modes, cache-key splitting, semantic
ineligibility, the 400 mapping).

| Layer | File | Tests |
|---|---|---|
| Unit | `query.rs` (inline) | `semantic_cache_rejects_filter` — enabled+filter → `InvalidRequest`, message mentions filter |
| Unit | `service.rs` (inline) | `filter_changes_cache_key` — `hash_query_for_cache` differs across `None` / `"id < 5"` / `"id > 5"` |
| Core / manager | **new** `tests/manager_query_filter.rs` | (a) filtered vector query returns only matching ids; (b) filter on `id`; (c) `_ingested_at` range filter; (d) filter narrows **FTS-only**; (e) filter narrows **hybrid**; (f) zero-match predicate → empty; (g) **malformed predicate → `InvalidRequest`** (pins the 400 decision) |
| Core / service | **new** `tests/service_query_filter.rs` | (a) filtered vs unfiltered miss independently, then both `ExactCache`-hit on repeat; (b) two distinct filters don't collide; (c) filtered query is **semantic-ineligible** → `Backend`, never `SemanticCache` |
| API | **new** `tests/api_query_filter.rs` | (a) filter narrows results over the wire; (b) `400` on malformed filter; (c) `filter`+`semantic_cache` → `400`; (d) filter coexists with `include_vector:false` |
| Python | `python/tests/` (or example) | `search(filter=...)` narrows results; parity with the HTTP path |

---

## Part 2 — arbitrary scalar columns (design; build after sign-off)

The issue author asked to align on the approach before coding. This is the
expanded design, framed as decisions needing sign-off.

### Surfaces touched

API DTO `UpsertRow` (`handlers.rs:123`) + core `UpsertRow` (`manager.rs:144`) +
its `From` impl (`manager.rs:160`) + the upsert mapping (`handlers.rs:168`);
`schema_for_kind` (`manager.rs:389`, no longer static); `rows_to_batch`
(`~manager.rs:1836`); the result row struct + `batches_to_results`; the
projection logic (`manager.rs:1081`); `read_schema_facts` (`manager.rs:1596`);
`validate_arrow_import_schema` (`manager.rs:1698`); `validate_scalar_index_column`
(`manager.rs:105`). Part 1's `filter` then references these columns for free.

### Decisions (with recommendations)

- **A. Value typing.** JSON upsert carries `attributes: { name: scalar }`.
  Recommend documented inference for v1: JSON integer → `Int64`, float →
  `Float64`, bool → `Boolean`, string → `Utf8`. **No** implicit timestamp
  parsing (store epoch as int, or use the typed `/import` Arrow path).
- **B. Schema establishment & evolution.** First upsert fixes the base + the
  attribute columns it carries. A later upsert introducing a *new* attribute →
  **additive evolution** via Lance `add_columns`, existing rows backfilled null.
  Evolution is a commit, so it must invalidate the cached `schema_info` + pooled
  handle (same as delete/index). **Type conflict** (column seen as `Int64`,
  later `Utf8`) → `400 InvalidRequest`, first type wins.
- **C. Nullability.** All attribute columns nullable; omitted → null. Document
  that upsert is a **full-row replace** (Lance merge-insert), so omitting a
  previously-set attribute nulls it — consistent with `vector` / `text` today.
- **D. `schema_info` caching.** Extend `NamespaceSchemaInfo` + `read_schema_facts`
  to carry attribute names+types, driving batch construction, projection, and
  conflict validation.
- **E. Projection.** Replace the hardcoded `["id","text",_ingested_at]`
  projection (`manager.rs:1081`) with "all columns except `vector`" when
  `include_vector:false`, so attributes survive the vector-light shape.
- **F. Result wire shape & cache.** Result rows gain `attributes`; this changes
  the bincode result-cache payload format. The existing
  `undecodable_cached_payload_is_a_miss` self-healing absorbs the bump — note it
  in CHANGELOG.
- **G. Reserved names.** Reject attribute names colliding with `id` / `vector` /
  `vectors` / `text` / `_ingested_at`, the `_` prefix, or non-SQL-identifier
  names (so the filter dialect can reference them unquoted). Pure validator → 400.
- **H. `/import` first.** Relax `validate_arrow_import_schema` to accept extra
  scalar columns (reject nested/list types beyond vectors). Arrow types are
  explicit, so this is the less ambiguous place to land typed attributes —
  possibly before the JSON path.
- **I. Scalar index on attributes.** Extend `validate_scalar_index_column` to
  allow any existing attribute column, so filters on high-cardinality attributes
  are index-accelerated. Natural follow-on (same or later PR).

### Test envelope (Part 2, when built)

Mirror Part 1 per concern: upsert-with-attributes round-trip (manager);
schema-evolution add-column + null backfill; type-conflict → 400; reserved-name
→ 400; attributes materialized on results **and** under `include_vector:false`;
**filter-on-attribute end-to-end** (ties Part 1 + Part 2); import-with-attributes;
scalar-index-on-attribute; cache-payload format bump. Plus **pure unit tests**
for `infer_attribute_schema` (JSON→Arrow) and `validate_attribute_names`, matching
the repo's pattern of densely unit-testing pure validators without MinIO.

---

## Sequencing

1. **PR 1 — Part 1**: filter mechanism + Python `filter=` + the ~15-test
   envelope. Closes the small half of [#84].
2. **Design sign-off** on Part 2 decisions A–I (post to [#84] as the alignment
   the author asked for).
3. **PR 2 — Part 2**: likely Arrow-import-first (H), then JSON path + evolution.
