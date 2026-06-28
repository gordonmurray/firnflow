# Implementation plan — faceted search (facet counts)

> Companion to [`PLAN.md`](./PLAN.md), which owns query-time filtering ([#84]).
> This plan covers **facet aggregation** — returning counts per distinct value
> for one or more columns, computed over the whole filtered set, not the
> returned top-k. It is a *new* capability beyond #84 and depends on #84 Part 2
> (arbitrary scalar columns) landing first.

[#84]: https://github.com/gordonmurray/firnflow/issues/84

A **facet rail** is a materialised snapshot: "of the rows matching this filter,
how many are `section = warnings` vs `dosage`?" — independent of which `k`
neighbours a vector query ranks highest. This is the standard faceted-search UX
(results + a count rail), and the defining property is that the counts reflect
the *filter*, never the *top-k*.

## Why this depends on scalar columns

Facet counts are only meaningful over columns that carry user values. Today a
namespace has `id` / `vector` / `text` / `_ingested_at` only
(`schema_for_kind`, `manager.rs:389`), so the only facetable columns are `id`
(unique → useless) and `_ingested_at` (continuous → needs bucketing). **#84
Part 2** (arbitrary scalar columns, designed in `PLAN.md`) is the prerequisite.
Hence the sequencing chosen below: scalar columns first, facet counts on top.

## Relationship to #84 / where to file

- PR 2a / 2b below **are** #84 Part 2 — they close the larger half of #84.
- Facet aggregation (PR 3a / 3b) is a distinct capability. File a **new issue**
  on `gordonmurray/firnflow` describing the gap (mirroring the #84 writeup: cite
  the `tbl.dataset()` raw-Lance access in `list()` as the existing plumbing),
  then land the PRs against it. Keeps the contribution paper trail honest.

## Decisions (with recommendations)

Facet-specific design choices, framed like #84 Part 2's A–I — align before
coding 3a.

- **F1. Surface — dedicated `POST /ns/{ns}/facet`, not a field on `/query`.**
  Facet counts depend only on `(generation, filter, fields, top)` — *not* on
  `vector` / `k` / `nprobes` / `text`. Folding them into `/query` would either
  pollute the query cache key (recomputed per distinct vector) or force a second
  cache lookup inside the query path. A dedicated endpoint keeps the cache key
  minimal, keeps the PR small, and matches the endpoint-per-operation style
  (`/list`, `/import`, `/scalar-index`). **Deferred convenience:** a later PR can
  attach a cached facet snapshot to `/query` responses, reusing 3b's cache.
- **F2. Aggregation — in-memory scan + count for v1.** Project only the facet
  columns over the filtered set and count per value in a `HashMap`. Mirrors how
  `list()` sorts/truncates in memory (`manager.rs:1476-1486`). **Scale
  follow-up:** push the `GROUP BY` into DataFusion (Lance integrates with it —
  the filter dialect already is DataFusion). Note it like `list()` notes the
  index range-scan follow-up (`manager.rs:1385-1389`); do not build it in v1.
- **F3. Counts are over the filtered set, never the top-k.** The defining
  semantic. `/facet` ignores ranking entirely: `COUNT(*) … [WHERE filter] GROUP
  BY field`. Document explicitly — this is the "materialised snapshot, not a
  tally of returned rows" contract.
- **F4. Value typing.** Each bucket value is the JSON scalar of the column's
  Arrow type (`Int64` / `Float64` / `Boolean` / `Utf8`, per #84 Part 2 decision
  A). Counting downcasts per column type. Represent the value as
  `serde_json::Value` in the result type so one shape carries all column types.
- **F5. Null handling — null is a bucket.** Rows with no value for a facet
  column count into a `value: null` bucket; faceting commonly surfaces
  "missing". Document it. (Alternative considered: a separate `missing` scalar —
  rejected as a second code path for the same information.)
- **F6. Cardinality bound — per-field `top` (default 100).** Buckets sorted
  count-desc, value-asc as tie-break. When a field has more distinct values than
  `top`, return the top-N **and** flag it (`truncated: true` on that field) — no
  silent cap. High-cardinality facets are a misuse, but the response should say
  so rather than imply completeness.
- **F7. Caching — reuse the `NamespaceCache` generation discipline.** Key =
  `hash(filter, sorted fields, top)` + the namespace generation
  (`service.rs:242`, folds in the Lance manifest commit timestamp). Any commit
  (write / delete / compaction / index build) advances the generation, so facet
  snapshots auto-invalidate exactly like query results — no new invalidation
  logic. Facets are **not** semantic-cacheable (no vector).
- **F8. Facetable columns.** Any scalar attribute column (#84 Part 2) plus
  `id` / `_ingested_at` (allowed but documented as low-value without bucketing).
  Reject `vector` / `vectors` / `text` and unknown columns with `400
  InvalidRequest`, via a pure validator (mirrors `validate_scalar_index_column`).
- **F9. Python parity.** `collection.facet(fields=[...], filter=None, top=None)`
  returning a dict, mirroring the HTTP endpoint — same parity bar #84 Part 1 set
  with `search(filter=...)`.

## What was verified in the code

- `list()` (`manager.rs:1390`) already obtains the raw `lance::Dataset` —
  `tbl.dataset()?.get().await` (`manager.rs:1423-1430`) — and runs
  `dataset.scan()` with `.filter(&predicate)` (`manager.rs:1432-1446`). This is
  the aggregation entry point; `facet()` reuses it with a column projection.
- The DataFusion SQL predicate dialect is proven on both `scan().filter()`
  (`manager.rs:1445`) and the query `only_if` prefilter (#84 Part 1). The same
  `filter` string works for facets for free.
- Malformed-predicate → `400` is already the established mapping: #84 Part 1
  remaps `execute()` errors to `InvalidRequest` when a filter is set
  (`manager.rs:1163`). `facet()` mirrors it.
- Result types live in `result.rs` (`QueryResult` `:16`, `QueryResultSet` `:49`,
  `ListRow` `:73`) and round-trip through bincode for the cache;
  `serde_json::Value` bincode-encodes fine, so a polymorphic bucket value is
  cache-safe.
- Cache-aside is `query_with_cache_source` (`service.rs:224`): `hash` `:232`,
  `generation` `:242`, `try_get` `:253`, `populate_with_generation` `:362`.
  Facet caching follows this shape with a facet-specific hash.
- Routes register in `crates/firnflow-api/src/lib.rs`; `/facet` is a read-group
  route alongside `/query` and `/list` (`lib.rs:71-75`).
- Error → status mapping (`InvalidRequest → 400`, `Unsupported → 501`,
  `Backend → 500`) is unchanged (`firnflow-api/src/error.rs:90-112`).

## Test-quality bar

Same house style as #84 (see `PLAN.md`): integration tests are
`#[tokio::test] #[ignore]` (MinIO-gated), driven through the axum router with
tower `oneshot`, isolated by `unique_namespace(prefix)`; pure validators are
unit-tested inline without MinIO; one function per scenario, no rstest/proptest.
Run: `docker compose up -d minio minio-init` then `./scripts/cargo test --
--ignored`.

---

## Sequencing (small PRs)

The user preference is **smallish, focused PRs** ([[pr-size-preference]] in
memory). Four PRs, each independently reviewable. Boundaries are a
recommendation — 3a/3b can merge if 3a stays small, or docs/python can peel off
3a as its own PR.

| PR | Scope | Closes |
|----|-------|--------|
| **2a** | Scalar columns — import-first then JSON upsert + evolution | #84 Part 2 |
| **2b** | Filter + scalar index over attribute columns | #84 Part 2 |
| **3a** | `/facet` endpoint + aggregation + caching + tests | new issue |
| **3b** | Python `facet()` parity + docs | new issue |

### PR 2a / 2b — scalar columns (prerequisite)

**Do not re-design here.** This is #84 Part 2, fully sketched in `PLAN.md` →
"Part 2 — arbitrary scalar columns", decisions A–I (value typing, schema
evolution via `add_columns`, nullability, `schema_info` extension, projection,
result wire shape + cache bump, reserved names, `/import`-first, scalar index on
attributes). Get sign-off on A–I (post to #84), then:

- **2a** — the surface from `PLAN.md` decision H first (relax
  `validate_arrow_import_schema` for typed Arrow attribute columns), then the
  JSON path + additive evolution (A, B, C, D, E, F, G). Attributes materialise
  back on results and survive `include_vector:false`.
- **2b** — `filter` over attribute columns end-to-end (mostly tests — #84
  Part 1's `filter` references the new columns for free) + scalar index on
  attributes (decision I).

After 2a/2b there are real columns to facet on.

### PR 3a — the facet endpoint

#### Production changes

| # | File | Change |
|---|------|--------|
| 1 | `query.rs` (new `FacetRequest`) | `pub struct FacetRequest { #[serde(default)] filter: Option<String>, fields: Vec<String>, #[serde(default)] top: Option<usize> }`. Document F3 (snapshot, not top-k) and the `filter` dialect (same as `/query`, `/list`). |
| 2 | `query.rs` (new `validate_facet_request`) | Pure validator: non-empty `fields`; each is a known facetable column (resolved against `schema_info`); reject `vector`/`vectors`/`text` and unknown names → `InvalidRequest` (F8); `top` in a sane range. Unit-tested inline. |
| 3 | `result.rs` | `FacetBucket { value: serde_json::Value, count: u64 }` and `FacetResultSet { facets: Vec<FacetField> }` where `FacetField { field: String, buckets: Vec<FacetBucket>, truncated: bool }` (F4, F6). `Serialize + Deserialize`, bincode-safe for the cache. |
| 4 | `manager.rs` (new `facet()`) | `pub async fn facet(&self, ns, filter: Option<String>, fields: &[String], top: usize) -> Result<FacetResultSet>`. Reuse the `list()` dataset access (`manager.rs:1423-1430`): `dataset.scan()`, `.filter(f)` when set, `.project(fields)`, stream batches, count per field per Arrow type into a `HashMap`, sort + truncate to `top` with the `truncated` flag (F2, F5, F6). Remap `execute()` errors to `InvalidRequest` when `filter.is_some()` (mirror `manager.rs:1163`). |
| 5 | `service.rs` (new `facet()`) | v1: validate + delegate to `manager.facet`. **No cache yet** (added in 3a step 6 or deferred to keep this PR minimal — recommend including it; see below). |
| 6 | `service.rs` (cache-aside) | Wrap `facet()` in the `query_with_cache_source` pattern (`service.rs:224-362`): `hash_facet_for_cache(filter, sorted fields, top)`, derive `generation` (`:242`), `try_get` / `populate_with_generation`. Add a `hash_facet_for_cache` next to `hash_query_for_cache` and a `FacetHash` (or reuse `QueryHash`'s newtype) in `cache.rs` (F7). |
| 7 | `handlers.rs` | `facet` handler: deserialize `FacetRequest`, call `service.facet`, return `Json<FacetResultSet>`. Pattern-match #84 Part 1's `query` handler (`handlers.rs:183`). |
| 8 | `lib.rs` | `.route("/ns/{namespace}/facet", post(handlers::facet))` in the read group (`lib.rs:71-75`). |
| 9 | API handler | No error-mapping change — `InvalidRequest → 400` already covers malformed filter and unknown field (`error.rs:90-112`). |

> Caching (steps 5/6): including it in 3a is recommended — it reuses existing
> infra and is the whole value prop of a "snapshot". If 3a grows, peel caching
> into its own PR.

#### Test envelope (3a)

| Layer | File | Tests |
|-------|------|-------|
| Unit | `query.rs` (inline) | `validate_facet_request`: empty `fields` → 400; unknown column → 400; `vector`/`text` rejected → 400; valid passes |
| Core / manager | **new** `tests/manager_facet.rs` | (a) single scalar column → correct counts; (b) multi-field in one call; (c) `filter` narrows the counts (F3); (d) null bucket (F5); (e) `top` truncates + sets `truncated` (F6); (f) malformed filter → `InvalidRequest`; (g) empty namespace → empty; (h) counts reflect the **filtered set, not k** — seed N rows, facet returns all-N grouping regardless of any `k` |
| Core / service | **new** `tests/service_facet.rs` | (a) cache miss then hit on repeat (same generation); (b) differently-filtered facets don't collide; (c) upsert bumps generation → next facet misses (invalidation) |
| API | **new** `tests/api_facet.rs` | (a) facet counts over the wire; (b) 400 on unknown field; (c) 400 on malformed filter |

### PR 3b — Python parity + docs

| # | File | Change |
|---|------|--------|
| 1 | `python/src/lib.rs` | `op_facet` + `facet(fields, *, filter=None, top=None, tenant=None)` on `Collection` and `Client`, mirroring `op_search` / `search` (`lib.rs:385`, `:506`, `:623`). Return a dict keyed by field → list of `{value, count}` (F9). |
| 2 | `python/tests/test_firn.py` | `test_facet`: seed rows with a scalar attribute, assert counts; parity with the HTTP path |
| 3 | `docs/api.html` | New `/facet` endpoint section: request fields (`filter`, `fields`, `top`), response shape, the snapshot-not-top-k note (F3), null + truncation semantics |
| 4 | `README.md` | Facet example under the query section |
| 5 | `CHANGELOG.md` | `[Unreleased]` → Added: `POST /ns/{ns}/facet` + Python `facet()` |

> Docs ideally ride with the feature. #84 Part 1 bundled python + docs into the
> one PR; splitting them here is the lever if 3a gets large. Land 3b promptly
> after 3a either way.

---

## Open questions / deferred

- **`_ingested_at` bucketing.** Faceting a continuous timestamp by exact value
  is useless. Date-bucket facets (`date_trunc('day', _ingested_at)`) are a
  natural v2 — likely a `bucket` spec per field. Out of scope for 3a.
- **Facets-on-`/query` convenience (F1 deferred).** Attach a cached facet
  snapshot to query responses in one round-trip, once 3b's cache exists.
- **DataFusion `GROUP BY` pushdown (F2 scale follow-up).** Replace scan+count
  when filtered sets get large enough that reading the facet column dominates.
- **Range / numeric facets.** Histogram-style buckets over `Int64`/`Float64`
  columns — a separate facet kind from the value-count facet built here.
