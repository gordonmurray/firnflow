//! Namespace manager — the production shim over lancedb.
//!
//! Each namespace maps to its own Lance table rooted at
//! `s3://{bucket}/{namespace}/`.
//!
//! **Per-namespace dimensions (slice 6a):** the manager no longer
//! carries a global `vector_dim`. Instead, dimensions are:
//!
//! - **inferred** from the first upsert into a fresh namespace
//!   (row[0]'s vector length), or
//! - **read from the Lance table schema** when re-opening an
//!   existing namespace.
//!
//! Resolved dimensions are cached in a `DashMap<NamespaceId, usize>`
//! so the schema-read / first-row-inference happens at most once per
//! namespace per process lifetime. Entries stay until the process
//! restarts or the namespace is deleted (in which case the stale
//! entry is evicted lazily on next use).
//!
//! **Connection pooling (issue #1):** each namespace's
//! `lancedb::Connection` + `lancedb::Table` are cached in a
//! `DashMap<NamespaceId, NamespaceHandle>` after the first
//! successful open. Subsequent upserts, queries, index builds, and
//! compactions reuse the cached handle, skipping the
//! S3-credential-resolution and manifest-read cost of a fresh
//! `lancedb::connect()` + `open_table()`.
//!
//! The pool is invalidated on namespace delete and on operations
//! that change the table's manifest (index build, compaction). A
//! regular append-only upsert does **not** evict — Lance appends
//! are visible through the existing handle.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::builder::{FixedSizeListBuilder, Float32Builder, StringBuilder};
use arrow_array::{
    Array, ArrayRef, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray, TimestampMicrosecondArray, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use dashmap::DashMap;
use futures::{StreamExt, TryStreamExt};
use lance::dataset::scanner::ColumnOrdering;
use lancedb::index::scalar::{FtsIndexBuilder, FullTextSearchQuery};
use lancedb::index::vector::IvfPqIndexBuilder;
use lancedb::index::Index;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::table::OptimizeAction;
use lancedb::DistanceType;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectStorePath;
use object_store::ObjectStore;

use crate::metrics::CoreMetrics;
use crate::query::DEFAULT_NPROBES;
use crate::result::{ListOrder, ListPage, ListRow};
use crate::{FirnflowError, NamespaceId, QueryResult, QueryResultSet};

const TABLE_NAME: &str = "data";
const DISTANCE_COLUMN: &str = "_distance";
const SCORE_COLUMN: &str = "_score";
const RELEVANCE_COLUMN: &str = "_relevance_score";
const INGESTED_AT_COLUMN: &str = "_ingested_at";

/// Maximum `limit` the `list` endpoint will honour per request.
/// Hard-capped to bound in-memory sort cost while the v1 endpoint
/// lacks scalar-index pushdown.
pub const LIST_MAX_LIMIT: usize = 500;

/// Per-namespace schema facts cached after the first table open.
/// Both the resolved vector dimension and whether the table
/// carries the `_ingested_at` system column come from the same
/// schema read and are stored together.
#[derive(Debug, Clone, Copy)]
struct NamespaceSchemaInfo {
    dim: usize,
    has_ingested_at: bool,
}

/// A single row for upsert into a namespace.
#[derive(Debug, Clone)]
pub struct UpsertRow {
    /// Stable row identifier.
    pub id: u64,
    /// The vector — length must match the namespace's dimension.
    pub vector: Vec<f32>,
    /// Optional text payload for BM25 full-text search.
    pub text: Option<String>,
}

impl From<(u64, Vec<f32>)> for UpsertRow {
    fn from((id, vector): (u64, Vec<f32>)) -> Self {
        Self {
            id,
            vector,
            text: None,
        }
    }
}

/// Cached per-namespace backend handles. Both members are cheap to
/// hold: `Connection` is an S3 client + config; `Table` is an
/// in-memory metadata handle referencing the connection. Storing
/// both explicitly (rather than leaning on `Table`'s internal
/// reference to its connection) keeps the slow-path logic
/// self-contained and leaves room for a future code path that
/// wants to re-open a table against the cached connection.
struct NamespaceHandle {
    #[allow(dead_code)]
    conn: lancedb::Connection,
    table: lancedb::Table,
}

/// Namespace manager over an S3-backed set of Lance tables.
///
/// Each namespace independently determines its own vector dimension
/// — either inferred from the first upsert or read from the
/// existing Lance table schema. A single manager instance can serve
/// namespaces with different dimensions simultaneously.
///
/// The manager caches an `lancedb::Connection` + `lancedb::Table`
/// per namespace in an internal `DashMap` so repeat operations on
/// the same namespace avoid the S3 credential-resolution and
/// manifest-read round-trip of a fresh `lancedb::connect()`.
pub struct NamespaceManager {
    bucket: String,
    storage_options: HashMap<String, String>,
    /// Per-namespace schema facts. Populated on first interaction
    /// (upsert or query). Carries the resolved vector dimension and
    /// a flag for whether the underlying Lance table has the
    /// `_ingested_at` column that the `list` endpoint relies on.
    schema_info: DashMap<NamespaceId, NamespaceSchemaInfo>,
    /// Per-namespace connection + table handles. Populated lazily
    /// by [`NamespaceManager::get_or_open_table`] and evicted on
    /// namespace delete / index build / compaction.
    handles: DashMap<NamespaceId, NamespaceHandle>,
    metrics: Arc<CoreMetrics>,
}

impl NamespaceManager {
    /// Construct a new manager.
    ///
    /// * `bucket` – S3 bucket that roots every namespace this
    ///   manager serves. Namespace tables live at
    ///   `s3://{bucket}/{namespace}/`.
    /// * `storage_options` – `object_store`-style key/value options
    ///   passed verbatim to lancedb's connection builder. Use
    ///   `aws_endpoint` / `aws_access_key_id` / etc. keys.
    /// * `metrics` – process-wide metrics registry; the manager
    ///   adjusts `firnflow_cached_handles` as connection pool
    ///   entries are added and removed.
    ///
    /// **Credential rotation note:** `storage_options` is captured
    /// once and reused for the lifetime of every cached connection.
    /// If the deployment rotates credentials at runtime, every
    /// cached handle must be flushed when `storage_options` changes
    /// — no such mechanism exists today. For V1 we document this
    /// as a known single-process assumption.
    pub fn new(
        bucket: impl Into<String>,
        storage_options: HashMap<String, String>,
        metrics: Arc<CoreMetrics>,
    ) -> Self {
        Self {
            bucket: bucket.into(),
            storage_options,
            schema_info: DashMap::new(),
            handles: DashMap::new(),
            metrics,
        }
    }

    /// Resolved vector dimension for a namespace, if known. Returns
    /// `None` for namespaces the manager has not yet interacted
    /// with.
    pub fn dim_for(&self, ns: &NamespaceId) -> Option<usize> {
        self.schema_info.get(ns).map(|r| r.dim)
    }

    /// Whether the namespace's Lance table carries the
    /// `_ingested_at` system column required by the `list` endpoint.
    /// Returns `None` for namespaces the manager has not yet
    /// interacted with. `Some(false)` is returned for namespaces
    /// whose tables were created before `_ingested_at` existed; the
    /// list endpoint surfaces this as HTTP 501.
    pub fn supports_list(&self, ns: &NamespaceId) -> Option<bool> {
        self.schema_info.get(ns).map(|r| r.has_ingested_at)
    }

    /// Number of namespaces currently holding a pooled
    /// `lancedb::Connection` + `lancedb::Table` handle. Mirrors the
    /// `firnflow_cached_handles` gauge and is exposed for tests
    /// that need to assert pool-hit / pool-miss behaviour directly.
    pub fn pool_size(&self) -> usize {
        self.handles.len()
    }

    /// Whether a pooled handle exists for `ns`. Useful for tests
    /// that want to confirm a specific namespace was (or was not)
    /// evicted.
    pub fn is_pooled(&self, ns: &NamespaceId) -> bool {
        self.handles.contains_key(ns)
    }

    fn uri(&self, ns: &NamespaceId) -> String {
        format!("s3://{}/{}", self.bucket, ns.as_str())
    }

    /// Build the Arrow schema for a namespace's Lance table.
    ///
    /// `with_ingested_at` controls whether the trailing
    /// `_ingested_at` system column is included. Fresh namespaces
    /// always use `true`; appends into pre-existing tables pass
    /// whatever the live table schema reports so the batch matches.
    fn schema_for_dim(dim: usize, with_ingested_at: bool) -> Arc<Schema> {
        let mut fields = vec![
            Field::new("id", DataType::UInt64, false),
            Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    dim as i32,
                ),
                false,
            ),
            Field::new("text", DataType::Utf8, true),
        ];
        if with_ingested_at {
            fields.push(Field::new(
                INGESTED_AT_COLUMN,
                DataType::Timestamp(TimeUnit::Microsecond, None),
                false,
            ));
        }
        Arc::new(Schema::new(fields))
    }

    async fn connect(&self, ns: &NamespaceId) -> Result<lancedb::Connection, FirnflowError> {
        lancedb::connect(&self.uri(ns))
            .storage_options(self.storage_options.clone())
            .execute()
            .await
            .map_err(|e| FirnflowError::Backend(format!("lancedb connect: {e}")))
    }

    /// Insert a freshly opened `NamespaceHandle` into the pool and
    /// bump the `cached_handles` gauge. If a handle for `ns` already
    /// exists (race between two concurrent openers), the second
    /// insert overwrites the first — both are valid, the first is
    /// simply dropped.
    fn cache_handle(&self, ns: &NamespaceId, conn: lancedb::Connection, table: lancedb::Table) {
        let previous = self
            .handles
            .insert(ns.clone(), NamespaceHandle { conn, table });
        if previous.is_none() {
            self.metrics.inc_cached_handles();
        }
    }

    /// Drop a namespace's cached handle and decrement the gauge.
    /// Called after operations that change the table's manifest or
    /// remove its data: delete, index build, compaction.
    fn evict_handle(&self, ns: &NamespaceId) {
        if self.handles.remove(ns).is_some() {
            self.metrics.dec_cached_handles();
        }
    }

    /// Return a cached `lancedb::Table` for `ns`, opening (and if
    /// necessary, creating) one on a cache miss. This is the single
    /// entry point every public method uses to obtain a table
    /// handle — removing the old "new connection per call" cost.
    ///
    /// On a miss the table is opened; if it does not yet exist a
    /// fresh one is created with the `dim`-shaped schema. The
    /// resulting handle is cached in `self.handles` so the next
    /// caller hits the fast path.
    /// Return a cached `lancedb::Table` for `ns`, opening (and if
    /// necessary, creating) one on a cache miss.
    ///
    /// When the table must be created, it is built with the current
    /// schema (including `_ingested_at`). If the table already
    /// exists it is opened as-is — no schema migration is attempted.
    async fn get_or_open_table(
        &self,
        ns: &NamespaceId,
        dim: usize,
    ) -> Result<lancedb::Table, FirnflowError> {
        if let Some(entry) = self.handles.get(ns) {
            return Ok(entry.table.clone());
        }

        let conn = self.connect(ns).await?;
        let table = match conn.open_table(TABLE_NAME).execute().await {
            Ok(tbl) => tbl,
            Err(_) => {
                // Fresh namespace: always create with the current
                // schema, which includes `_ingested_at`.
                let schema = Self::schema_for_dim(dim, true);
                let empty = rows_to_batch(&schema, dim, Vec::new(), true)?;
                let reader: Box<dyn RecordBatchReader + Send> =
                    Box::new(RecordBatchIterator::new(vec![Ok(empty)], schema));
                conn.create_table(TABLE_NAME, reader)
                    .execute()
                    .await
                    .map_err(|e| FirnflowError::Backend(format!("create_table: {e}")))?
            }
        };

        let cloned = table.clone();
        self.cache_handle(ns, conn, table);
        Ok(cloned)
    }

    /// Try to open an existing table for `ns` without creating one.
    /// Used by [`resolve_schema_info`] to discover a namespace's
    /// dimension and ingested-at support from its persisted schema.
    /// On success the handle is cached so the subsequent operation
    /// avoids a second `open_table`.
    async fn open_existing(
        &self,
        ns: &NamespaceId,
    ) -> Result<Option<(lancedb::Table, NamespaceSchemaInfo)>, FirnflowError> {
        if let Some(entry) = self.handles.get(ns) {
            let tbl = entry.table.clone();
            drop(entry);
            let info = read_schema_info_from_table(&tbl).await?;
            return Ok(Some((tbl, info)));
        }

        let conn = self.connect(ns).await?;
        match conn.open_table(TABLE_NAME).execute().await {
            Ok(tbl) => {
                let info = read_schema_info_from_table(&tbl).await?;
                self.cache_handle(ns, conn, tbl.clone());
                Ok(Some((tbl, info)))
            }
            Err(_) => Ok(None),
        }
    }

    /// Resolve the schema facts for a namespace:
    /// 1. Check the in-memory cache.
    /// 2. Try reading from the existing Lance table schema.
    /// 3. Return `None` if the namespace doesn't exist yet.
    async fn resolve_schema_info(
        &self,
        ns: &NamespaceId,
    ) -> Result<Option<NamespaceSchemaInfo>, FirnflowError> {
        if let Some(info) = self.schema_info.get(ns) {
            return Ok(Some(*info));
        }
        if let Some((_tbl, info)) = self.open_existing(ns).await? {
            self.schema_info.insert(ns.clone(), info);
            return Ok(Some(info));
        }
        Ok(None)
    }

    /// Append rows to the namespace's table.
    ///
    /// On a fresh namespace the dimension is inferred from the first
    /// row's vector length. All subsequent rows in the request are
    /// validated against it. On an existing namespace the dimension
    /// is read from the Lance table schema, and every row must
    /// match.
    pub async fn upsert(
        &self,
        ns: &NamespaceId,
        rows: Vec<UpsertRow>,
    ) -> Result<(), FirnflowError> {
        if rows.is_empty() {
            return Ok(());
        }

        // Determine schema facts: cached → live schema → infer for
        // a fresh namespace. Fresh namespaces always get the current
        // schema (with `_ingested_at`); pre-upgrade tables keep their
        // legacy shape so appends continue to work.
        let info = match self.resolve_schema_info(ns).await? {
            Some(info) => info,
            None => {
                let dim = rows[0].vector.len();
                if dim == 0 {
                    return Err(FirnflowError::InvalidRequest(
                        "row id 0: vector is empty".into(),
                    ));
                }
                NamespaceSchemaInfo {
                    dim,
                    has_ingested_at: true,
                }
            }
        };

        for row in &rows {
            if row.vector.len() != info.dim {
                return Err(FirnflowError::InvalidRequest(format!(
                    "row id {}: vector length {}, expected {}",
                    row.id,
                    row.vector.len(),
                    info.dim,
                )));
            }
        }

        let tbl = self.get_or_open_table(ns, info.dim).await?;
        let schema = Self::schema_for_dim(info.dim, info.has_ingested_at);
        let batch = rows_to_batch(&schema, info.dim, rows, info.has_ingested_at)?;
        let reader: Box<dyn RecordBatchReader + Send> =
            Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema));
        tbl.add(reader)
            .execute()
            .await
            .map_err(|e| FirnflowError::Backend(format!("table.add: {e}")))?;

        self.schema_info.insert(ns.clone(), info);
        Ok(())
    }

    /// Remove every object under a namespace prefix from the
    /// underlying object store.
    ///
    /// Also evicts the cached dimension **and** the pooled
    /// connection/table handle for this namespace so that a
    /// subsequent upsert can establish a new dimension against a
    /// fresh Lance table.
    ///
    /// Returns the number of objects deleted. The caller
    /// (`NamespaceService::delete`) is responsible for invalidating
    /// the namespace cache entries after a successful delete — we
    /// intentionally do not couple the foyer cache to the manager.
    pub async fn delete(&self, ns: &NamespaceId) -> Result<usize, FirnflowError> {
        let store = self.build_object_store()?;
        let prefix = ObjectStorePath::from(ns.as_str());

        let mut list_stream = store.list(Some(&prefix));
        let mut count: usize = 0;
        while let Some(result) = list_stream.next().await {
            let meta = result.map_err(|e| FirnflowError::Backend(format!("list {ns}: {e}")))?;
            store
                .delete(&meta.location)
                .await
                .map_err(|e| FirnflowError::Backend(format!("delete {}: {e}", meta.location)))?;
            count += 1;
        }

        // Evict cached schema info + pooled handle so a fresh
        // upsert can pick a new dim.
        self.schema_info.remove(ns);
        self.evict_handle(ns);

        Ok(count)
    }

    fn build_object_store(&self) -> Result<Arc<dyn ObjectStore>, FirnflowError> {
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(&self.bucket);

        for (key, value) in &self.storage_options {
            builder = match key.as_str() {
                "aws_access_key_id" => builder.with_access_key_id(value),
                "aws_secret_access_key" => builder.with_secret_access_key(value),
                "aws_region" => builder.with_region(value),
                "aws_endpoint" => builder.with_endpoint(value),
                "allow_http" => builder.with_allow_http(value == "true"),
                "aws_virtual_hosted_style_request" => {
                    builder.with_virtual_hosted_style_request(value == "true")
                }
                _ => builder,
            };
        }

        let store = builder
            .build()
            .map_err(|e| FirnflowError::Backend(format!("build object store: {e}")))?;
        Ok(Arc::new(store))
    }

    /// Run a search query. Supports three modes:
    ///
    /// - **Vector-only** (`vector` non-empty, `text` is `None`):
    ///   nearest-neighbour search via `nearest_to`.
    /// - **FTS-only** (`vector` empty, `text` is `Some`): BM25
    ///   full-text search via `full_text_search`.
    /// - **Hybrid** (`vector` non-empty, `text` is `Some`): combined
    ///   vector + FTS via Reciprocal Rank Fusion (lancedb handles
    ///   the fusion internally when both are set on a VectorQuery).
    ///
    /// `nprobes` controls how many IVF partitions are searched for
    /// vector queries. Defaults to [`DEFAULT_NPROBES`] (20).
    pub async fn query(
        &self,
        ns: &NamespaceId,
        vector: Vec<f32>,
        k: usize,
        nprobes: Option<usize>,
        text: Option<String>,
    ) -> Result<QueryResultSet, FirnflowError> {
        let dim = match self.resolve_schema_info(ns).await? {
            Some(info) => info.dim,
            None => {
                return Ok(QueryResultSet {
                    query_id: String::new(),
                    results: Vec::new(),
                });
            }
        };

        let has_vector = !vector.is_empty();
        let has_text = text.is_some();

        if !has_vector && !has_text {
            return Err(FirnflowError::InvalidRequest(
                "query must have at least a vector or a text field".into(),
            ));
        }

        if has_vector && vector.len() != dim {
            return Err(FirnflowError::InvalidRequest(format!(
                "query vector length {}, expected {dim}",
                vector.len(),
            )));
        }

        let nprobes = nprobes.unwrap_or(DEFAULT_NPROBES);
        let tbl = self.get_or_open_table(ns, dim).await?;

        let stream = if has_vector {
            // Vector-only or hybrid (lancedb auto-detects hybrid when
            // both nearest_to and full_text_search are set).
            let mut vq = tbl
                .query()
                .nearest_to(vector)
                .map_err(|e| FirnflowError::Backend(format!("query.nearest_to: {e}")))?
                .nprobes(nprobes)
                .limit(k);
            if let Some(ref t) = text {
                vq = vq.full_text_search(FullTextSearchQuery::new(t.clone()));
            }
            vq.execute()
                .await
                .map_err(|e| FirnflowError::Backend(format!("query.execute: {e}")))?
        } else {
            // FTS-only
            let t = text.unwrap();
            tbl.query()
                .full_text_search(FullTextSearchQuery::new(t))
                .limit(k)
                .execute()
                .await
                .map_err(|e| FirnflowError::Backend(format!("fts.execute: {e}")))?
        };

        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| FirnflowError::Backend(format!("query.collect: {e}")))?;

        let results = batches_to_results(&batches)?;
        Ok(QueryResultSet {
            query_id: String::new(),
            results,
        })
    }

    /// Build an IVF_PQ index on the namespace's vector column.
    ///
    /// This is a potentially expensive operation — minutes for large
    /// tables on S3. The caller (service/handler) is responsible for
    /// running it in a background task if non-blocking behaviour is
    /// desired.
    ///
    /// Index build does **not** invalidate the cache — cached query
    /// results are still correct post-build. See PHASE6_PLAN.md §
    /// "Cache invalidation and index rebuild" for the rationale.
    pub async fn create_index(
        &self,
        ns: &NamespaceId,
        num_partitions: Option<u32>,
        num_sub_vectors: Option<u32>,
    ) -> Result<(), FirnflowError> {
        let dim = self
            .resolve_schema_info(ns)
            .await?
            .ok_or_else(|| {
                FirnflowError::InvalidRequest(format!(
                    "cannot index namespace {ns}: no data has been upserted yet"
                ))
            })?
            .dim;

        let tbl = self.get_or_open_table(ns, dim).await?;

        let mut builder = IvfPqIndexBuilder::default().distance_type(DistanceType::L2);
        if let Some(n) = num_partitions {
            builder = builder.num_partitions(n);
        }
        if let Some(m) = num_sub_vectors {
            builder = builder.num_sub_vectors(m);
        }

        tbl.create_index(&["vector"], Index::IvfPq(builder))
            .execute()
            .await
            .map_err(|e| FirnflowError::Backend(format!("create_index: {e}")))?;

        // Evict the pooled handle: building an index bumps the
        // table manifest, so the next operation should open a fresh
        // table view rather than reuse metadata captured before the
        // build.
        self.evict_handle(ns);
        Ok(())
    }

    /// Build a BM25 full-text search index on the namespace's `text`
    /// column. Requires that at least some rows have been upserted
    /// with non-null `text` values.
    pub async fn create_fts_index(&self, ns: &NamespaceId) -> Result<(), FirnflowError> {
        let dim = self
            .resolve_schema_info(ns)
            .await?
            .ok_or_else(|| {
                FirnflowError::InvalidRequest(format!(
                    "cannot create FTS index on namespace {ns}: no data has been upserted yet"
                ))
            })?
            .dim;

        let tbl = self.get_or_open_table(ns, dim).await?;
        tbl.create_index(&["text"], Index::FTS(FtsIndexBuilder::default()))
            .execute()
            .await
            .map_err(|e| FirnflowError::Backend(format!("create_fts_index: {e}")))?;

        // Same manifest-bump rationale as `create_index`.
        self.evict_handle(ns);
        Ok(())
    }

    /// Compact the namespace's Lance table — merge small data files
    /// into fewer, larger ones.
    ///
    /// Uses `OptimizeAction::Compact` with default
    /// `CompactionOptions` (target 1M rows per fragment). Returns
    /// the number of fragments removed and added so the caller can
    /// report the delta.
    ///
    /// Like `create_index`, this is a potentially expensive
    /// operation and the caller should run it in a background task.
    pub async fn compact(&self, ns: &NamespaceId) -> Result<CompactResult, FirnflowError> {
        let dim = self
            .resolve_schema_info(ns)
            .await?
            .ok_or_else(|| {
                FirnflowError::InvalidRequest(format!(
                    "cannot compact namespace {ns}: no data has been upserted yet"
                ))
            })?
            .dim;

        let tbl = self.get_or_open_table(ns, dim).await?;
        let stats = tbl
            .optimize(OptimizeAction::default())
            .await
            .map_err(|e| FirnflowError::Backend(format!("optimize: {e}")))?;

        let (removed, added) = stats
            .compaction
            .map(|c| (c.fragments_removed, c.fragments_added))
            .unwrap_or((0, 0));

        // Compaction rewrites fragments: any cached Table view is
        // pointing at file offsets that no longer exist.
        self.evict_handle(ns);

        Ok(CompactResult {
            fragments_removed: removed,
            fragments_added: added,
        })
    }

    /// List rows from a namespace in `_ingested_at` order.
    ///
    /// This path deliberately does **not** participate in the foyer
    /// cache — paginated scans would push hot query results out
    /// with cold, one-shot list entries. Callers are expected to
    /// invoke the manager directly.
    ///
    /// `limit` is clamped to [`LIST_MAX_LIMIT`]; `cursor` is the
    /// `(timestamp_micros, id)` pair taken from the last row of the
    /// previous page and filters out rows that appeared earlier in
    /// the chosen order.
    ///
    /// Returns [`FirnflowError::Unsupported`] on namespaces whose
    /// tables pre-date the `_ingested_at` column (HTTP 501 at the
    /// API layer).
    ///
    /// **V1 performance note:** LanceDB 0.27's high-level query
    /// API does not expose scalar-index ordering. The implementation
    /// drops through to `lance::Dataset::scan()` with
    /// `order_by`, which triggers a full scan of the fragments
    /// matching the cursor filter before returning the first batch.
    /// Acceptable for small-to-medium namespaces; a scalar index on
    /// `_ingested_at` is the follow-up for scale.
    pub async fn list(
        &self,
        ns: &NamespaceId,
        limit: usize,
        order: ListOrder,
        cursor: Option<(i64, u64)>,
    ) -> Result<ListPage, FirnflowError> {
        let limit = limit.clamp(1, LIST_MAX_LIMIT);

        let info = match self.resolve_schema_info(ns).await? {
            Some(i) => i,
            None => {
                return Ok(ListPage {
                    rows: Vec::new(),
                    next_cursor: None,
                });
            }
        };
        if !info.has_ingested_at {
            return Err(FirnflowError::Unsupported(format!(
                "namespace {ns} pre-dates the _ingested_at column; \
                 recreate the namespace or wait for the migration follow-up"
            )));
        }

        let tbl = self.get_or_open_table(ns, info.dim).await?;
        let dataset_wrapper = tbl
            .dataset()
            .ok_or_else(|| FirnflowError::Backend("list requires a native lance table".into()))?;
        let dataset = dataset_wrapper
            .get()
            .await
            .map_err(|e| FirnflowError::Backend(format!("resolve dataset: {e}")))?;

        let mut scan = dataset.scan();

        if let Some((ts, id)) = cursor {
            let filter = match order {
                ListOrder::Desc => format!(
                    "({INGESTED_AT_COLUMN} < to_timestamp_micros({ts})) \
                     OR ({INGESTED_AT_COLUMN} = to_timestamp_micros({ts}) AND id < {id})"
                ),
                ListOrder::Asc => format!(
                    "({INGESTED_AT_COLUMN} > to_timestamp_micros({ts})) \
                     OR ({INGESTED_AT_COLUMN} = to_timestamp_micros({ts}) AND id > {id})"
                ),
            };
            scan.filter(&filter)
                .map_err(|e| FirnflowError::Backend(format!("scan.filter: {e}")))?;
        }

        let ordering = match order {
            ListOrder::Desc => vec![
                ColumnOrdering::desc_nulls_last(INGESTED_AT_COLUMN.to_string()),
                ColumnOrdering::desc_nulls_last("id".to_string()),
            ],
            ListOrder::Asc => vec![
                ColumnOrdering::asc_nulls_first(INGESTED_AT_COLUMN.to_string()),
                ColumnOrdering::asc_nulls_first("id".to_string()),
            ],
        };
        scan.order_by(Some(ordering))
            .map_err(|e| FirnflowError::Backend(format!("scan.order_by: {e}")))?;

        // Pull `limit + 1` so we can derive the next cursor and
        // flag "no more pages" in one pass.
        scan.limit(Some((limit + 1) as i64), None)
            .map_err(|e| FirnflowError::Backend(format!("scan.limit: {e}")))?;

        let stream = scan
            .try_into_stream()
            .await
            .map_err(|e| FirnflowError::Backend(format!("scan.try_into_stream: {e}")))?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| FirnflowError::Backend(format!("scan.collect: {e}")))?;

        let mut rows = batches_to_list_rows(&batches)?;

        let next_cursor = if rows.len() > limit {
            rows.truncate(limit);
            rows.last()
                .map(|r| encode_list_cursor(r.ingested_at_micros, r.id))
        } else {
            None
        };

        Ok(ListPage { rows, next_cursor })
    }
}

/// Encode a `(timestamp_micros, id)` pair as a 32-character hex
/// cursor. Opaque on the wire; the handler round-trips it via
/// [`decode_list_cursor`].
pub fn encode_list_cursor(ts_micros: i64, id: u64) -> String {
    format!("{:016x}{:016x}", ts_micros as u64, id)
}

/// Decode a cursor produced by [`encode_list_cursor`]. Returns an
/// [`FirnflowError::InvalidRequest`] on malformed input so the API
/// layer can return a 400 verbatim.
pub fn decode_list_cursor(cursor: &str) -> Result<(i64, u64), FirnflowError> {
    if cursor.len() != 32 || !cursor.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(FirnflowError::InvalidRequest(format!(
            "malformed cursor {cursor:?}: expected 32 hex characters"
        )));
    }
    let ts = u64::from_str_radix(&cursor[..16], 16)
        .map_err(|e| FirnflowError::InvalidRequest(format!("cursor timestamp: {e}")))?;
    let id = u64::from_str_radix(&cursor[16..], 16)
        .map_err(|e| FirnflowError::InvalidRequest(format!("cursor id: {e}")))?;
    Ok((ts as i64, id))
}

/// Result of a compaction operation, exposing the fragment delta.
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// Number of old fragments merged away.
    pub fragments_removed: usize,
    /// Number of new (larger) fragments written.
    pub fragments_added: usize,
}

/// Read the schema facts (vector dimension, `_ingested_at` presence)
/// from a Lance table in one pass. The dimension comes from the
/// `FixedSizeList.list_size` of the `vector` column; the flag is set
/// when a column named `_ingested_at` with a `Timestamp(Microsecond)`
/// type is present.
async fn read_schema_info_from_table(
    tbl: &lancedb::Table,
) -> Result<NamespaceSchemaInfo, FirnflowError> {
    let schema = tbl
        .schema()
        .await
        .map_err(|e| FirnflowError::Backend(format!("read schema: {e}")))?;
    let mut dim: Option<usize> = None;
    let mut has_ingested_at = false;
    for field in schema.fields() {
        match field.name().as_str() {
            "vector" => {
                if let DataType::FixedSizeList(_, size) = field.data_type() {
                    dim = Some(*size as usize);
                }
            }
            INGESTED_AT_COLUMN => {
                if matches!(
                    field.data_type(),
                    DataType::Timestamp(TimeUnit::Microsecond, _)
                ) {
                    has_ingested_at = true;
                }
            }
            _ => {}
        }
    }
    let dim = dim.ok_or_else(|| {
        FirnflowError::Backend("table schema has no FixedSizeList 'vector' column".into())
    })?;
    Ok(NamespaceSchemaInfo {
        dim,
        has_ingested_at,
    })
}

/// Server-clock reading in microseconds since the Unix epoch.
/// Negative clock skew or pre-epoch system clocks are clamped to 0 —
/// the column is a write-time stamp, not a system clock health check.
fn current_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

fn rows_to_batch(
    schema: &Arc<Schema>,
    dim: usize,
    rows: Vec<UpsertRow>,
    include_ingested_at: bool,
) -> Result<RecordBatch, FirnflowError> {
    let n = rows.len();
    let ids = UInt64Array::from_iter_values(rows.iter().map(|r| r.id));

    let values_builder = Float32Builder::with_capacity(n * dim);
    let mut list_builder = FixedSizeListBuilder::new(values_builder, dim as i32);
    for row in &rows {
        for &v in &row.vector {
            list_builder.values().append_value(v);
        }
        list_builder.append(true);
    }
    let vectors = list_builder.finish();

    let mut text_builder = StringBuilder::with_capacity(n, n * 64);
    for row in &rows {
        match &row.text {
            Some(t) => text_builder.append_value(t),
            None => text_builder.append_null(),
        }
    }
    let texts = text_builder.finish();

    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(ids) as ArrayRef,
        Arc::new(vectors) as ArrayRef,
        Arc::new(texts) as ArrayRef,
    ];

    if include_ingested_at {
        // Stamp every row in the batch with the same server-side
        // timestamp. Because Lance appends are immutable, this
        // becomes the row's permanent "arrival" time — first-write
        // semantics fall out of the append-only model for free.
        let ts = current_micros();
        let ts_array = TimestampMicrosecondArray::from_iter_values(std::iter::repeat_n(ts, n));
        columns.push(Arc::new(ts_array) as ArrayRef);
    }

    RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| FirnflowError::Backend(format!("batch build: {e}")))
}

/// Find the score column in a result batch. Lance uses different
/// column names depending on query type:
/// - `_distance` for vector queries
/// - `_score` for FTS queries
/// - `_relevance_score` for hybrid queries
fn find_score_column(batch: &RecordBatch) -> Option<&Float32Array> {
    for name in [RELEVANCE_COLUMN, DISTANCE_COLUMN, SCORE_COLUMN] {
        if let Some(col) = batch.column_by_name(name) {
            if let Some(arr) = col.as_any().downcast_ref::<Float32Array>() {
                return Some(arr);
            }
        }
    }
    None
}

fn batches_to_list_rows(batches: &[RecordBatch]) -> Result<Vec<ListRow>, FirnflowError> {
    let mut out = Vec::new();
    for batch in batches {
        let ids = batch
            .column_by_name("id")
            .ok_or_else(|| FirnflowError::Backend("list: missing id column".into()))?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| FirnflowError::Backend("list: id not UInt64".into()))?;
        let vectors = batch
            .column_by_name("vector")
            .ok_or_else(|| FirnflowError::Backend("list: missing vector column".into()))?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .ok_or_else(|| FirnflowError::Backend("list: vector not FixedSizeList".into()))?;
        let texts = batch
            .column_by_name("text")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let ingested_at = batch
            .column_by_name(INGESTED_AT_COLUMN)
            .ok_or_else(|| FirnflowError::Backend("list: missing _ingested_at column".into()))?
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .ok_or_else(|| {
                FirnflowError::Backend("list: _ingested_at not Timestamp(Microsecond)".into())
            })?;

        for row in 0..batch.num_rows() {
            let vector_arr = vectors.value(row);
            let vec_f32 = vector_arr
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| FirnflowError::Backend("list: vector inner not Float32".into()))?;
            let vector: Vec<f32> = (0..vec_f32.len()).map(|i| vec_f32.value(i)).collect();
            let text = texts.and_then(|t| {
                if t.is_null(row) {
                    None
                } else {
                    Some(t.value(row).to_owned())
                }
            });
            out.push(ListRow {
                id: ids.value(row),
                vector,
                text,
                ingested_at_micros: ingested_at.value(row),
            });
        }
    }
    Ok(out)
}

fn batches_to_results(batches: &[RecordBatch]) -> Result<Vec<QueryResult>, FirnflowError> {
    let mut out = Vec::new();
    for batch in batches {
        let ids = batch
            .column_by_name("id")
            .ok_or_else(|| FirnflowError::Backend("query result: missing id column".into()))?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| FirnflowError::Backend("query result: id not UInt64".into()))?;
        let vectors = batch
            .column_by_name("vector")
            .ok_or_else(|| FirnflowError::Backend("query result: missing vector column".into()))?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .ok_or_else(|| {
                FirnflowError::Backend("query result: vector not FixedSizeList".into())
            })?;
        let scores = find_score_column(batch).ok_or_else(|| {
            FirnflowError::Backend(
                "query result: no score column (_distance, _score, or _relevance_score)".into(),
            )
        })?;

        // Text column is optional — present only if the namespace
        // was upserted with text data.
        let texts = batch
            .column_by_name("text")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());

        for row in 0..batch.num_rows() {
            let vector_arr = vectors.value(row);
            let vec_f32 = vector_arr
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| {
                    FirnflowError::Backend("query result: vector inner not Float32".into())
                })?;
            let vector: Vec<f32> = (0..vec_f32.len()).map(|i| vec_f32.value(i)).collect();
            let text = texts.and_then(|t| {
                if t.is_null(row) {
                    None
                } else {
                    Some(t.value(row).to_owned())
                }
            });
            out.push(QueryResult {
                id: ids.value(row),
                score: scores.value(row),
                vector,
                text,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trip() {
        for (ts, id) in [
            (0_i64, 0_u64),
            (1, 1),
            (i64::MAX, u64::MAX),
            (1_700_000_000_000_000, 42),
        ] {
            let encoded = encode_list_cursor(ts, id);
            assert_eq!(encoded.len(), 32);
            let (ts2, id2) = decode_list_cursor(&encoded).expect("decode");
            assert_eq!((ts, id), (ts2, id2));
        }
    }

    #[test]
    fn cursor_rejects_bad_length() {
        assert!(decode_list_cursor("").is_err());
        assert!(decode_list_cursor("abcd").is_err());
        assert!(decode_list_cursor(&"a".repeat(33)).is_err());
    }

    #[test]
    fn cursor_rejects_non_hex() {
        assert!(decode_list_cursor(&"z".repeat(32)).is_err());
    }
}
