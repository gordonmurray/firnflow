//! Namespace manager — the production shim over lancedb.
//!
//! Each namespace maps to its own Lance table rooted at
//! `s3://{bucket}/{namespace}/`.
//!
//! **Per-namespace dimensions:** the manager no longer
//! carries a global `vector_dim`. Instead, dimensions are:
//!
//! - **inferred** from the first upsert into a fresh namespace
//!   (row[0]'s vector length), or
//! - **read from the Lance table schema** when re-opening an
//!   existing namespace.
//!
//! Resolved schema facts — the vector dimension and whether the
//! table carries the `_ingested_at` system column — are cached in a
//! `DashMap<NamespaceId, NamespaceSchemaInfo>` so the schema-read /
//! first-row-inference happens at most once per namespace per process
//! lifetime. Entries stay until the process restarts or the namespace
//! is deleted (in which case the stale entry is evicted lazily on
//! next use).
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
//! regular upsert does **not** evict — a merge-insert write commits
//! through the cached handle and its result is visible to subsequent
//! reads on that same handle.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::builder::{FixedSizeListBuilder, Float32Builder, ListBuilder, StringBuilder};
use arrow_array::{
    Array, ArrayRef, FixedSizeListArray, Float32Array, ListArray, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray, TimestampMicrosecondArray, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use dashmap::DashMap;
use futures::{StreamExt, TryStreamExt};
use lance::dataset::scanner::ColumnOrdering;
use lancedb::index::scalar::{BTreeIndexBuilder, FtsIndexBuilder, FullTextSearchQuery};
use lancedb::index::vector::IvfPqIndexBuilder;
use lancedb::index::{Index, IndexType};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::table::OptimizeAction;
use lancedb::DistanceType;
use object_store::aws::AmazonS3Builder;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::path::Path as ObjectStorePath;
use object_store::ObjectStore;
use xxhash_rust::xxh3::xxh3_64;

use crate::metrics::CoreMetrics;
use crate::query::DEFAULT_NPROBES;
use crate::result::{ListOrder, ListPage, ListRow, NamespaceInfo};
use crate::storage_root::Scheme;
use crate::vector::VectorKind;
use crate::{FirnflowError, NamespaceId, QueryResult, QueryResultSet, StorageRoot};

const TABLE_NAME: &str = "data";
const DISTANCE_COLUMN: &str = "_distance";
const SCORE_COLUMN: &str = "_score";
const RELEVANCE_COLUMN: &str = "_relevance_score";
const INGESTED_AT_COLUMN: &str = "_ingested_at";

/// Maximum `limit` the `list` endpoint will honour per request.
/// Hard-capped to bound in-memory sort cost while the v1 endpoint
/// lacks scalar-index pushdown.
pub const LIST_MAX_LIMIT: usize = 500;

/// Columns the v1 `/scalar-index` endpoint will build a BTree on.
/// Locked to `_ingested_at` to mirror the same constraint the
/// `/list` endpoint puts on `order_by` — the BTree only earns its
/// keep on columns the query path actually filters or orders by.
/// Future user-column ordering work extends this slice.
const SCALAR_INDEX_COLUMNS: &[&str] = &[INGESTED_AT_COLUMN];

/// Per-namespace schema facts cached after the first table open.
/// The resolved vector dimension, the kind of vector representation
/// (single vs multivector), and whether the table carries the
/// `_ingested_at` system column all come from the same schema read
/// and are stored together.
#[derive(Debug, Clone, Copy)]
struct NamespaceSchemaInfo {
    /// For [`VectorKind::Single`] this is the vector dimension. For
    /// [`VectorKind::Multivector`] this is the inner sub-vector
    /// dimension (each sub-vector is fixed at this width).
    dim: usize,
    kind: VectorKind,
    has_ingested_at: bool,
}

/// A single row for upsert into a namespace.
///
/// The payload shape determines which kind of namespace the row is
/// intended for:
/// - **Single-vector**: set [`vector`](Self::vector) to a slice of
///   length `dim`, leave [`vectors`](Self::vectors) as `None`.
/// - **Multivector**: set [`vectors`](Self::vectors) to a non-empty
///   list of equal-length inner vectors, leave
///   [`vector`](Self::vector) empty.
///
/// At most one of the two fields may be populated; setting both
/// returns 400 at the API boundary.
#[derive(Debug, Clone)]
pub struct UpsertRow {
    /// Stable row identifier.
    pub id: u64,
    /// The single-vector payload. Length must match the namespace's
    /// dimension. Empty means "no single vector" — used when this
    /// row carries a multivector payload instead.
    pub vector: Vec<f32>,
    /// The multivector payload. Each inner vector must have the
    /// namespace's inner sub-vector dimension; the outer list length
    /// is the per-row sub-vector count and may vary between rows.
    /// `None` means "no multivector".
    pub vectors: Option<Vec<Vec<f32>>>,
    /// Optional text payload for BM25 full-text search.
    pub text: Option<String>,
}

impl From<(u64, Vec<f32>)> for UpsertRow {
    fn from((id, vector): (u64, Vec<f32>)) -> Self {
        Self {
            id,
            vector,
            vectors: None,
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

/// Namespace manager over an object-storage-backed set of Lance
/// tables.
///
/// Each namespace independently determines its own vector dimension
/// — either inferred from the first upsert or read from the
/// existing Lance table schema. A single manager instance can serve
/// namespaces with different dimensions simultaneously.
///
/// The manager caches an `lancedb::Connection` + `lancedb::Table`
/// per namespace in an internal `DashMap` so repeat operations on
/// the same namespace avoid the credential-resolution and
/// manifest-read round-trip of a fresh `lancedb::connect()`.
pub struct NamespaceManager {
    storage_root: StorageRoot,
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
    /// * `storage_root` – the parsed [`StorageRoot`] every namespace
    ///   under this manager lives under. Namespace tables live at
    ///   `{root}/{namespace}/`, where `{root}` includes the scheme,
    ///   bucket, and any optional fixed prefix.
    /// * `storage_options` – `object_store`-style key/value options
    ///   passed verbatim to lancedb's connection builder. Use
    ///   `aws_endpoint` / `aws_access_key_id` / etc. keys for S3.
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
        storage_root: StorageRoot,
        storage_options: HashMap<String, String>,
        metrics: Arc<CoreMetrics>,
    ) -> Self {
        Self {
            storage_root,
            storage_options,
            schema_info: DashMap::new(),
            handles: DashMap::new(),
            metrics,
        }
    }

    /// The configured storage root. Exposed for diagnostics and
    /// tests; consumers should not need to reach in this far during
    /// normal operation.
    pub fn storage_root(&self) -> &StorageRoot {
        &self.storage_root
    }

    /// Resolved vector dimension for a namespace, if known. For
    /// multivector namespaces this is the inner sub-vector
    /// dimension. Returns `None` for namespaces the manager has not
    /// yet interacted with.
    pub fn dim_for(&self, ns: &NamespaceId) -> Option<usize> {
        self.schema_info.get(ns).map(|r| r.dim)
    }

    /// Vector representation kind for a namespace, if known.
    /// Returns `None` for namespaces the manager has not yet
    /// interacted with.
    pub fn kind_for(&self, ns: &NamespaceId) -> Option<VectorKind> {
        self.schema_info.get(ns).map(|r| r.kind)
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

    /// Current cache generation for a namespace: a deterministic hash
    /// over the Lance manifest's `version` and commit `timestamp_nanos`.
    ///
    /// The version advances on every commit (append, delete, index
    /// build, compaction) and is persisted in the manifest, so it
    /// survives a process restart — that is what stops a recovered NVMe
    /// entry being served after a write. The commit timestamp is folded
    /// in so that two incarnations of a namespace which reach the same
    /// version after a delete-and-recreate still key differently: a
    /// result cached against the deleted incarnation cannot be re-served
    /// to the new one.
    ///
    /// This reflects only the version this process's handle has
    /// observed. It is read from the cached handle without a
    /// `checkout_latest`, so it does not necessarily see commits made by
    /// another process; multi-replica cache coherence is out of scope
    /// and Firn assumes a single replica per bucket.
    ///
    /// Returns `0` for a namespace that has no table yet (nothing has
    /// been written), which can never collide with a real generation.
    ///
    /// Cheap on the hot path: with a pooled handle the manifest is read
    /// in memory with no object-store round-trip. A cold namespace pays
    /// one table-open — the same cost a backend query would incur — and
    /// the handle is then pooled. Never creates a table: a query against
    /// a never-written namespace must not materialise one.
    pub async fn generation(&self, ns: &NamespaceId) -> Result<u64, FirnflowError> {
        // Warm pool: read the manifest straight off the cached handle.
        // Clone the handle and drop the map guard before awaiting so we
        // never hold a DashMap reference across an `.await`.
        if let Some(entry) = self.handles.get(ns) {
            let tbl = entry.table.clone();
            drop(entry);
            return Self::generation_of(&tbl).await;
        }
        // Cold: open the table if it exists (caching the handle), else
        // report generation 0 without creating anything.
        match self.open_existing(ns).await? {
            Some((tbl, _info)) => Self::generation_of(&tbl).await,
            None => Ok(0),
        }
    }

    /// Hash an open table's manifest `(version, timestamp_nanos)` into a
    /// `u64` cache generation. Both fields live in the in-memory
    /// manifest, so on a warm handle this is an in-memory read.
    async fn generation_of(tbl: &lancedb::Table) -> Result<u64, FirnflowError> {
        let dataset = tbl
            .dataset()
            .ok_or_else(|| {
                FirnflowError::Backend("generation requires a native lance table".into())
            })?
            .get()
            .await
            .map_err(|e| FirnflowError::Backend(format!("resolve dataset: {e}")))?;
        let manifest = dataset.manifest();
        let mut buf = [0u8; 24];
        buf[..8].copy_from_slice(&manifest.version.to_le_bytes());
        buf[8..].copy_from_slice(&manifest.timestamp_nanos.to_le_bytes());
        Ok(xxh3_64(&buf))
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
        self.storage_root.namespace_uri(ns)
    }

    /// Build the Arrow schema for a namespace's Lance table.
    ///
    /// The `vector` column type depends on `kind`:
    /// - [`VectorKind::Single`]: `FixedSizeList<Float32, dim>` — one
    ///   dense vector per row.
    /// - [`VectorKind::Multivector`]:
    ///   `List<FixedSizeList<Float32, dim>>` — a variable-length bag
    ///   of fixed-dimension sub-vectors per row. Lance dispatches the
    ///   late-interaction (MaxSim) scoring path automatically when it
    ///   sees this column shape.
    ///
    /// `with_ingested_at` controls whether the trailing
    /// `_ingested_at` system column is included. Fresh namespaces
    /// always use `true`; appends into pre-existing tables pass
    /// whatever the live table schema reports so the batch matches.
    fn schema_for_kind(kind: VectorKind, dim: usize, with_ingested_at: bool) -> Arc<Schema> {
        let inner_item = Arc::new(Field::new("item", DataType::Float32, true));
        let vector_type = match kind {
            VectorKind::Single => DataType::FixedSizeList(inner_item, dim as i32),
            VectorKind::Multivector => {
                let inner_fsl = DataType::FixedSizeList(inner_item, dim as i32);
                DataType::List(Arc::new(Field::new("item", inner_fsl, true)))
            }
        };
        let mut fields = vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("vector", vector_type, false),
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
    /// fresh one is created with the `kind`-/`dim`-shaped schema. The
    /// resulting handle is cached in `self.handles` so the next
    /// caller hits the fast path.
    ///
    /// When the table must be created, it is built with the current
    /// schema (including `_ingested_at`). If the table already
    /// exists it is opened as-is — no schema migration is attempted.
    async fn get_or_open_table(
        &self,
        ns: &NamespaceId,
        kind: VectorKind,
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
                let schema = Self::schema_for_kind(kind, dim, true);
                let empty = rows_to_batch(&schema, kind, dim, Vec::new(), true)?;
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
            // Only a genuine "table does not exist" means the namespace
            // has no data. Storage, auth, and transient errors must
            // propagate as backend errors, not be misreported as a
            // missing namespace (the `info` endpoint maps `None` to 404).
            Err(lancedb::Error::TableNotFound { .. }) => Ok(None),
            Err(e) => Err(FirnflowError::Backend(format!("open table {ns}: {e}"))),
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

    /// Insert or update rows in the namespace's table, keyed by `id`
    /// (latest-write-wins).
    ///
    /// On a fresh namespace the vector kind and dimension are
    /// inferred from the first row's payload (a non-empty `vector`
    /// field means [`VectorKind::Single`]; a non-empty `vectors`
    /// field means [`VectorKind::Multivector`]). All subsequent rows
    /// in the request are validated against the inferred shape. On
    /// an existing namespace the kind and dimension are read from
    /// the Lance table schema; every row must match.
    ///
    /// Rows are merged by `id` via LanceDB's merge-insert: a row whose
    /// `id` already exists replaces the stored row in full (vector,
    /// text, and `_ingested_at`), and a row whose `id` is new is
    /// inserted. Replaying the same request is therefore idempotent
    /// from the caller's point of view, and `_ingested_at` reflects
    /// the most recent write rather than the first insert. The merge
    /// finds matches by scanning for `id`; there is no scalar index on
    /// `id` yet, so writes to a large namespace pay a full-fragment
    /// scan per batch (see issue #66 for the planned auto-built BTree).
    ///
    /// Lance leaves merge behaviour undefined when several source rows
    /// match the same target row, so duplicate ids within a single
    /// request are rejected with [`FirnflowError::InvalidRequest`]
    /// before any write.
    pub async fn upsert(
        &self,
        ns: &NamespaceId,
        rows: Vec<UpsertRow>,
    ) -> Result<(), FirnflowError> {
        if rows.is_empty() {
            return Ok(());
        }

        // Reject duplicate ids within the request: merge-insert is
        // undefined when more than one source row matches the same
        // target row, so a request that contains its own duplicate
        // would have ambiguous semantics. Catch it before the write.
        let mut seen_ids = HashSet::with_capacity(rows.len());
        for row in &rows {
            if !seen_ids.insert(row.id) {
                return Err(FirnflowError::InvalidRequest(format!(
                    "duplicate id {} in upsert request; ids must be unique within a single request",
                    row.id
                )));
            }
        }

        // Determine schema facts: cached → live schema → infer for
        // a fresh namespace. Fresh namespaces always get the current
        // schema (with `_ingested_at`); pre-upgrade tables keep their
        // legacy shape so writes continue to match.
        let info = match self.resolve_schema_info(ns).await? {
            Some(info) => info,
            None => {
                let (kind, dim) = inspect_row_payload(&rows[0])?;
                NamespaceSchemaInfo {
                    dim,
                    kind,
                    has_ingested_at: true,
                }
            }
        };

        // Validate every row against the namespace's resolved kind
        // and dim. Mixed-shape requests fail at the API boundary
        // with a precise per-row message.
        for row in &rows {
            validate_row_against(row, info.kind, info.dim)?;
        }

        // `get_or_open_table` creates an empty table for a fresh
        // namespace, so by here the table always exists. Merge-insert
        // then handles both cases uniformly: into an empty table every
        // row is "not matched" and inserted; into a populated table
        // matched ids are replaced and new ids inserted.
        let tbl = self.get_or_open_table(ns, info.kind, info.dim).await?;
        let schema = Self::schema_for_kind(info.kind, info.dim, info.has_ingested_at);
        let batch = rows_to_batch(&schema, info.kind, info.dim, rows, info.has_ingested_at)?;
        let reader: Box<dyn RecordBatchReader + Send> =
            Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema));
        let mut merge = tbl.merge_insert(&["id"]);
        merge
            .when_matched_update_all(None)
            .when_not_matched_insert_all();
        merge
            .execute(reader)
            .await
            .map_err(|e| FirnflowError::Backend(format!("table.merge_insert: {e}")))?;

        self.schema_info.insert(ns.clone(), info);
        Ok(())
    }

    /// Remove every object under a namespace prefix from the
    /// underlying object store.
    ///
    /// Also evicts the cached schema info (dimension +
    /// `_ingested_at` flag) **and** the pooled connection/table
    /// handle for this namespace so that a subsequent upsert can
    /// establish a new dimension against a fresh Lance table.
    ///
    /// Returns the number of objects deleted. The caller
    /// (`NamespaceService::delete`) is responsible for invalidating
    /// the namespace cache entries after a successful delete — we
    /// intentionally do not couple the foyer cache to the manager.
    pub async fn delete(&self, ns: &NamespaceId) -> Result<usize, FirnflowError> {
        let store = self.build_object_store()?;
        let prefix = self.namespace_object_path(ns);

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

    /// Object-store-relative path of a namespace as an
    /// [`ObjectStorePath`]. Delegates the prefix stitching to
    /// [`StorageRoot::namespace_object_path`] so the same logic is
    /// unit-tested without needing an `object_store` builder.
    fn namespace_object_path(&self, ns: &NamespaceId) -> ObjectStorePath {
        ObjectStorePath::from(self.storage_root.namespace_object_path(ns))
    }

    fn build_object_store(&self) -> Result<Arc<dyn ObjectStore>, FirnflowError> {
        match self.storage_root.scheme() {
            Scheme::S3 => self.build_s3_object_store(),
            Scheme::Gcs => self.build_gcs_object_store(),
        }
    }

    fn build_s3_object_store(&self) -> Result<Arc<dyn ObjectStore>, FirnflowError> {
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(self.storage_root.bucket());

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

    /// Build a native `object_store::gcp` client for the configured
    /// GCS bucket. Used by the namespace delete path, which lists
    /// and removes objects under the namespace prefix; lancedb's
    /// own GCS routing (via the `gcs` feature) goes through a
    /// separate `lance-io` client keyed off the same credentials.
    ///
    /// Credential resolution mirrors what
    /// `GoogleCloudStorageBuilder::from_env` already does — read
    /// `GOOGLE_APPLICATION_CREDENTIALS` / `GOOGLE_SERVICE_ACCOUNT_PATH`
    /// / `GOOGLE_SERVICE_ACCOUNT_KEY` directly — and additionally
    /// honours any matching keys passed through `storage_options`,
    /// so the same map used by lancedb covers both clients without
    /// the operator having to set credentials twice.
    fn build_gcs_object_store(&self) -> Result<Arc<dyn ObjectStore>, FirnflowError> {
        let mut builder =
            GoogleCloudStorageBuilder::from_env().with_bucket_name(self.storage_root.bucket());

        for (key, value) in &self.storage_options {
            // `google_application_credentials` and
            // `google_service_account_path` are distinct concepts in
            // `object_store::gcp`: the former is an
            // application-default-credentials path (may resolve to
            // user creds, federated identity, etc.), the latter
            // strictly a service-account JSON path. The builder has
            // separate setters for the two — keep them separate here
            // so an ADC file that isn't a raw service-account JSON
            // still authenticates the delete path correctly.
            builder = match key.as_str() {
                "google_service_account" | "google_service_account_path" => {
                    builder.with_service_account_path(value)
                }
                "google_service_account_key" => builder.with_service_account_key(value),
                "google_application_credentials" => builder.with_application_credentials(value),
                _ => builder,
            };
        }

        let store = builder
            .build()
            .map_err(|e| FirnflowError::Backend(format!("build GCS object store: {e}")))?;
        Ok(Arc::new(store))
    }

    /// Run a search query.
    ///
    /// The vector payload uses one of two fields, matching the
    /// namespace's [`VectorKind`]:
    ///
    /// - `vector: Vec<f32>` — single-vector namespaces. Length must
    ///   match the namespace's dimension.
    /// - `vectors: Option<Vec<Vec<f32>>>` — multivector namespaces.
    ///   Each inner vector must match the namespace's inner
    ///   sub-vector dimension. Lance answers multivector queries
    ///   via the IVF_PQ index when one exists; without an index
    ///   it brute-force scans every row, which is fine for tiny
    ///   development corpora but impractical at scale (same
    ///   trade-off as single-vector queries).
    ///
    /// Supported query modes:
    ///
    /// - **Vector-only**: one of `vector` or `vectors` set, `text`
    ///   is `None`. Nearest-neighbour search via `nearest_to`
    ///   (single) or `nearest_to` + `add_query_vector` (multi).
    /// - **FTS-only**: both vector fields empty, `text` is `Some`.
    ///   BM25 full-text search via `full_text_search`.
    /// - **Hybrid**: a vector field set, `text` is `Some`. Combined
    ///   vector + FTS via Reciprocal Rank Fusion (lancedb handles
    ///   the fusion internally when both are set on a `VectorQuery`).
    ///
    /// Setting both `vector` and `vectors` returns 400. A payload
    /// whose shape does not match the namespace's kind returns 400
    /// with the expected shape named in the error.
    ///
    /// `nprobes` controls how many IVF partitions are searched for
    /// vector queries. Defaults to [`DEFAULT_NPROBES`] (20).
    pub async fn query(
        &self,
        ns: &NamespaceId,
        vector: Vec<f32>,
        vectors: Option<Vec<Vec<f32>>>,
        k: usize,
        nprobes: Option<usize>,
        text: Option<String>,
    ) -> Result<QueryResultSet, FirnflowError> {
        let info = match self.resolve_schema_info(ns).await? {
            Some(info) => info,
            None => {
                return Ok(QueryResultSet {
                    query_id: String::new(),
                    results: Vec::new(),
                });
            }
        };

        if !vector.is_empty() && vectors.is_some() {
            return Err(FirnflowError::InvalidRequest(
                "query may set at most one of `vector` or `vectors`".into(),
            ));
        }

        let has_text = text.is_some();
        // Resolve the query payload into one of three shapes:
        //   None              → FTS-only
        //   Single(Vec<f32>)  → single-vector nearest_to
        //   Multi(Vec<Vec<f32>>) → multivector nearest_to + add_query_vector loop
        enum QueryShape {
            Single(Vec<f32>),
            Multi(Vec<Vec<f32>>),
        }
        let shape: Option<QueryShape> = match (
            !vector.is_empty(),
            vectors.as_ref().map(|v| !v.is_empty()).unwrap_or(false),
        ) {
            (false, false) => None,
            (true, false) => {
                if info.kind != VectorKind::Single {
                    return Err(FirnflowError::InvalidRequest(format!(
                        "namespace {ns} is multivector, expected `vectors: [[...], ...]` \
                         but got `vector: [...]`"
                    )));
                }
                if vector.len() != info.dim {
                    return Err(FirnflowError::InvalidRequest(format!(
                        "query vector length {}, expected {}",
                        vector.len(),
                        info.dim,
                    )));
                }
                Some(QueryShape::Single(vector))
            }
            (false, true) => {
                if info.kind != VectorKind::Multivector {
                    return Err(FirnflowError::InvalidRequest(format!(
                        "namespace {ns} is single-vector, expected `vector: [...]` \
                         but got `vectors: [[...], ...]`"
                    )));
                }
                let sub_vectors = vectors.expect("vectors checked non-empty above");
                for (idx, sub) in sub_vectors.iter().enumerate() {
                    if sub.len() != info.dim {
                        return Err(FirnflowError::InvalidRequest(format!(
                            "query sub-vector {idx} length {}, expected {}",
                            sub.len(),
                            info.dim,
                        )));
                    }
                }
                Some(QueryShape::Multi(sub_vectors))
            }
            (true, true) => unreachable!("guarded by the two-fields check above"),
        };

        if shape.is_none() && !has_text {
            return Err(FirnflowError::InvalidRequest(
                "query must have at least a vector field or a text field".into(),
            ));
        }

        let nprobes = nprobes.unwrap_or(DEFAULT_NPROBES);
        let tbl = self.get_or_open_table(ns, info.kind, info.dim).await?;

        let stream = if let Some(shape) = shape {
            // Vector-only or hybrid (lancedb auto-detects hybrid when
            // both nearest_to and full_text_search are set). The
            // shape of the query call depends on the namespace kind:
            //
            // - **Single**: pass the dense vector to `nearest_to`.
            //   lancedb's auto-detection finds the `FixedSizeList`
            //   vector column from the query length.
            // - **Multivector**: pass the first sub-vector to
            //   `nearest_to`, then push each additional sub-vector
            //   via `add_query_vector`. lancedb 0.27 detects this
            //   pattern, sees the column is `List<FixedSizeList>`,
            //   and packs the bag of sub-vectors into a single
            //   late-interaction (MaxSim) query plan. The auto-
            //   detection of which column to query against
            //   only walks top-level `FixedSizeList` columns and
            //   skips `List<FixedSizeList<...>>`, so multivector
            //   namespaces must name the column explicitly.
            let mut vq = match &shape {
                QueryShape::Single(v) => tbl
                    .query()
                    .nearest_to(v.clone())
                    .map_err(|e| FirnflowError::Backend(format!("query.nearest_to: {e}")))?,
                QueryShape::Multi(subs) => {
                    let mut vq = tbl
                        .query()
                        .nearest_to(subs[0].clone())
                        .map_err(|e| FirnflowError::Backend(format!("query.nearest_to: {e}")))?
                        .column("vector");
                    for sub in &subs[1..] {
                        vq = vq.add_query_vector(sub.clone()).map_err(|e| {
                            FirnflowError::Backend(format!("query.add_query_vector: {e}"))
                        })?;
                    }
                    vq
                }
            };
            vq = vq.nprobes(nprobes).limit(k);
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

        let results = batches_to_results(&batches, info.kind)?;
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
        num_bits: Option<u32>,
    ) -> Result<(), FirnflowError> {
        // Reject unsupported PQ tuning combinations before any I/O
        // so direct callers (benches, integration tests) bypassing
        // the API handler still get a synchronous error rather than
        // a deferred Lance failure.
        crate::query::validate_ivf_pq_options(num_bits, num_sub_vectors)?;

        let info = self.resolve_schema_info(ns).await?.ok_or_else(|| {
            FirnflowError::InvalidRequest(format!(
                "cannot index namespace {ns}: no data has been upserted yet"
            ))
        })?;

        let tbl = self.get_or_open_table(ns, info.kind, info.dim).await?;

        // Single-vector namespaces use the historical L2 default;
        // multivector namespaces use cosine — Lance's
        // late-interaction index only supports cosine. The API
        // surface does not expose a metric override on
        // `IndexRequest`, so this is a defaulting choice keyed on
        // the namespace's vector kind, not an override of any
        // caller input.
        let metric = match info.kind {
            VectorKind::Single => DistanceType::L2,
            VectorKind::Multivector => DistanceType::Cosine,
        };
        let mut builder = IvfPqIndexBuilder::default().distance_type(metric);
        if let Some(n) = num_partitions {
            builder = builder.num_partitions(n);
        }
        if let Some(m) = num_sub_vectors {
            builder = builder.num_sub_vectors(m);
        }
        if let Some(b) = num_bits {
            builder = builder.num_bits(b);
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
        let info = self.resolve_schema_info(ns).await?.ok_or_else(|| {
            FirnflowError::InvalidRequest(format!(
                "cannot create FTS index on namespace {ns}: no data has been upserted yet"
            ))
        })?;

        let tbl = self.get_or_open_table(ns, info.kind, info.dim).await?;
        tbl.create_index(&["text"], Index::FTS(FtsIndexBuilder::default()))
            .execute()
            .await
            .map_err(|e| FirnflowError::Backend(format!("create_fts_index: {e}")))?;

        // Same manifest-bump rationale as `create_index`.
        self.evict_handle(ns);
        Ok(())
    }

    /// Build a BTree scalar index on a namespace column.
    ///
    /// v1 only accepts `_ingested_at` (see [`SCALAR_INDEX_COLUMNS`]).
    /// The index lets `/list` cursor pages do an index range scan
    /// instead of a full-fragment scan and lets the leading
    /// `ORDER BY _ingested_at` short-circuit the in-memory sort.
    ///
    /// Idempotent: `lancedb`'s `IndexBuilder` defaults to
    /// `replace=true`, so a repeat call rebuilds the index in place
    /// rather than failing.
    ///
    /// Returns [`FirnflowError::Unsupported`] for namespaces whose
    /// tables pre-date the `_ingested_at` column — same gate the
    /// `/list` endpoint applies.
    ///
    /// Like the other index builders, this is potentially expensive
    /// on large tables; the API handler runs it in a background task.
    pub async fn create_scalar_index(
        &self,
        ns: &NamespaceId,
        column: &str,
    ) -> Result<(), FirnflowError> {
        if !SCALAR_INDEX_COLUMNS.contains(&column) {
            return Err(FirnflowError::InvalidRequest(format!(
                "scalar index column {column:?} is not supported in v1; \
                 valid columns: {SCALAR_INDEX_COLUMNS:?}"
            )));
        }

        let info = self.resolve_schema_info(ns).await?.ok_or_else(|| {
            FirnflowError::InvalidRequest(format!(
                "cannot create scalar index on namespace {ns}: no data has been upserted yet"
            ))
        })?;

        if column == INGESTED_AT_COLUMN && !info.has_ingested_at {
            return Err(FirnflowError::Unsupported(format!(
                "namespace {ns} pre-dates the _ingested_at column; \
                 recreate the namespace before building a scalar index on it"
            )));
        }

        let tbl = self.get_or_open_table(ns, info.kind, info.dim).await?;
        tbl.create_index(&[column], Index::BTree(BTreeIndexBuilder::default()))
            .execute()
            .await
            .map_err(|e| FirnflowError::Backend(format!("create_scalar_index: {e}")))?;

        // Same manifest-bump rationale as `create_index` /
        // `create_fts_index`. Note: `OptimizeAction::All` (used by
        // `compact()`) runs `optimize_indices` which absorbs new rows
        // into the BTree without retraining, so callers do **not**
        // need to re-run `create_scalar_index` after a compaction.
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
        let info = self.resolve_schema_info(ns).await?.ok_or_else(|| {
            FirnflowError::InvalidRequest(format!(
                "cannot compact namespace {ns}: no data has been upserted yet"
            ))
        })?;

        let tbl = self.get_or_open_table(ns, info.kind, info.dim).await?;
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

        // Every successful list call makes S3 requests (manifest +
        // data reads via lance). Record one tick so the
        // `firnflow_s3_requests_total{operation="list"}` counter
        // preserves the cost-visibility story even though this path
        // bypasses `NamespaceService`, where the other operations
        // record theirs.
        self.metrics.record_s3_request(ns, "list");

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

        let tbl = self.get_or_open_table(ns, info.kind, info.dim).await?;
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

        let mut rows = batches_to_list_rows(&batches, info.kind)?;

        let next_cursor = if rows.len() > limit {
            rows.truncate(limit);
            rows.last()
                .map(|r| encode_list_cursor(r.ingested_at_micros, r.id))
        } else {
            None
        };

        Ok(ListPage { rows, next_cursor })
    }

    /// Gather operational metadata for a namespace without running a
    /// query: vector kind and dimension, live row count, fragment
    /// count, which index kinds are built, and the current table
    /// version.
    ///
    /// Returns `Ok(None)` when the namespace has no table yet (nothing
    /// has been written) so the API layer can answer 404. Never creates
    /// a table. `count_rows` and `list_indices` read table metadata on
    /// the backend, so this is a metadata round-trip and is
    /// deliberately not cached.
    pub async fn info(&self, ns: &NamespaceId) -> Result<Option<NamespaceInfo>, FirnflowError> {
        // This path opens the table and reads its metadata directly,
        // bypassing NamespaceService, so record the backend hit here —
        // same as `/list` — to keep `firnflow_s3_requests_total` an
        // honest count of Firn-initiated object-store operations.
        self.metrics.record_s3_request(ns, "info");

        let Some((tbl, schema)) = self.open_existing(ns).await? else {
            return Ok(None);
        };

        let row_count = tbl
            .count_rows(None)
            .await
            .map_err(|e| FirnflowError::Backend(format!("count_rows: {e}")))?;

        let indices = tbl
            .list_indices()
            .await
            .map_err(|e| FirnflowError::Backend(format!("list_indices: {e}")))?;
        let (has_vector_index, has_fts_index, has_scalar_index) =
            classify_index_types(indices.iter().map(|i| i.index_type.clone()));

        // Fragment count and table version come from the in-memory
        // manifest on the (now pooled) handle — no extra round-trip.
        let dataset = tbl
            .dataset()
            .ok_or_else(|| {
                FirnflowError::Backend("namespace info requires a native lance table".into())
            })?
            .get()
            .await
            .map_err(|e| FirnflowError::Backend(format!("resolve dataset: {e}")))?;
        let manifest = dataset.manifest();

        Ok(Some(NamespaceInfo {
            namespace: ns.to_string(),
            kind: schema.kind,
            vector_dim: schema.dim,
            row_count,
            fragment_count: manifest.fragments.len(),
            has_vector_index,
            has_fts_index,
            has_scalar_index,
            table_version: manifest.version,
        }))
    }
}

/// Classify a namespace's built indexes into `(vector, fts, scalar)`
/// presence flags. The IVF / HNSW family are vector indexes, `FTS` is
/// the BM25 full-text index, and BTree / bitmap / label-list are scalar
/// indexes. Exhaustive over [`IndexType`] on purpose: a new Lance index
/// kind will fail to compile here until it is classified deliberately.
fn classify_index_types(types: impl Iterator<Item = IndexType>) -> (bool, bool, bool) {
    let mut vector = false;
    let mut fts = false;
    let mut scalar = false;
    for ty in types {
        match ty {
            IndexType::IvfFlat
            | IndexType::IvfSq
            | IndexType::IvfPq
            | IndexType::IvfRq
            | IndexType::IvfHnswPq
            | IndexType::IvfHnswSq
            | IndexType::IvfHnswFlat => vector = true,
            IndexType::FTS => fts = true,
            IndexType::BTree | IndexType::Bitmap | IndexType::LabelList => scalar = true,
        }
    }
    (vector, fts, scalar)
}

/// Encode a `(timestamp_micros, id)` pair as a 32-character hex
/// cursor. The encoding is implementation-defined and may change —
/// clients must treat the returned string as an opaque token and
/// round-trip it verbatim via [`decode_list_cursor`]. Parsing the
/// bytes or constructing cursors by hand is not supported.
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

/// Read the schema facts (vector dimension, kind, `_ingested_at`
/// presence) from a Lance table in one pass.
///
/// The vector column type drives the kind:
/// - `FixedSizeList<Float32, dim>` → [`VectorKind::Single`]; `dim`
///   is the list size.
/// - `List<FixedSizeList<Float32, dim>>` → [`VectorKind::Multivector`];
///   `dim` is the inner sub-vector list size.
///
/// The `_ingested_at` flag is set when a column of that name with a
/// `Timestamp(Microsecond)` type is present.
async fn read_schema_info_from_table(
    tbl: &lancedb::Table,
) -> Result<NamespaceSchemaInfo, FirnflowError> {
    let schema = tbl
        .schema()
        .await
        .map_err(|e| FirnflowError::Backend(format!("read schema: {e}")))?;
    let mut dim_kind: Option<(usize, VectorKind)> = None;
    let mut has_ingested_at = false;
    for field in schema.fields() {
        match field.name().as_str() {
            "vector" => match field.data_type() {
                DataType::FixedSizeList(_, size) => {
                    dim_kind = Some((*size as usize, VectorKind::Single));
                }
                DataType::List(inner) => {
                    if let DataType::FixedSizeList(_, size) = inner.data_type() {
                        dim_kind = Some((*size as usize, VectorKind::Multivector));
                    }
                }
                _ => {}
            },
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
    let (dim, kind) = dim_kind.ok_or_else(|| {
        FirnflowError::Backend(
            "table schema 'vector' column is neither FixedSizeList nor List<FixedSizeList>".into(),
        )
    })?;
    Ok(NamespaceSchemaInfo {
        dim,
        kind,
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

/// Inspect a row's vector payload and report the kind + dimension
/// it implies. Used during fresh-namespace inference: the first row
/// of the first upsert determines the namespace's kind for the rest
/// of its life.
///
/// Returns [`FirnflowError::InvalidRequest`] if the row is missing
/// both vector fields, has both fields populated, has an empty
/// inner-vector list on a multivector payload, or has mixed
/// sub-vector dimensions within a multivector payload.
fn inspect_row_payload(row: &UpsertRow) -> Result<(VectorKind, usize), FirnflowError> {
    let single_set = !row.vector.is_empty();
    let multi_set = row.vectors.as_ref().map(|v| !v.is_empty()).unwrap_or(false);
    match (single_set, multi_set) {
        (true, true) => Err(FirnflowError::InvalidRequest(format!(
            "row id {}: set exactly one of `vector` or `vectors`, not both",
            row.id
        ))),
        (false, false) => Err(FirnflowError::InvalidRequest(format!(
            "row id {}: missing vector payload (set `vector` for single-vector \
             namespaces or `vectors` for multivector namespaces)",
            row.id
        ))),
        (true, false) => Ok((VectorKind::Single, row.vector.len())),
        (false, true) => {
            let multi = row.vectors.as_ref().expect("checked non-empty above");
            let dim = multi[0].len();
            if dim == 0 {
                return Err(FirnflowError::InvalidRequest(format!(
                    "row id {}: multivector sub-vector 0 is empty",
                    row.id
                )));
            }
            for (idx, sub) in multi.iter().enumerate() {
                if sub.len() != dim {
                    return Err(FirnflowError::InvalidRequest(format!(
                        "row id {}: multivector sub-vector {idx} length {}, \
                         expected {dim} (all sub-vectors in a row must share a dim)",
                        row.id,
                        sub.len(),
                    )));
                }
            }
            Ok((VectorKind::Multivector, dim))
        }
    }
}

/// Validate a row's payload against the namespace's resolved kind
/// and dimension. The error messages echo the expected payload
/// shape so a caller hitting the wrong namespace gets a clear
/// diagnostic.
fn validate_row_against(
    row: &UpsertRow,
    kind: VectorKind,
    dim: usize,
) -> Result<(), FirnflowError> {
    let (row_kind, row_dim) = inspect_row_payload(row)?;
    if row_kind != kind {
        let (expected, got) = match kind {
            VectorKind::Single => ("`vector: [...]`", "`vectors: [[...], ...]`"),
            VectorKind::Multivector => ("`vectors: [[...], ...]`", "`vector: [...]`"),
        };
        return Err(FirnflowError::InvalidRequest(format!(
            "row id {}: namespace kind is {}, expected {expected} but got {got}",
            row.id,
            kind.as_label(),
        )));
    }
    if row_dim != dim {
        return Err(FirnflowError::InvalidRequest(format!(
            "row id {}: {} dimension {}, expected {}",
            row.id,
            kind.as_label(),
            row_dim,
            dim,
        )));
    }
    Ok(())
}

fn rows_to_batch(
    schema: &Arc<Schema>,
    kind: VectorKind,
    dim: usize,
    rows: Vec<UpsertRow>,
    include_ingested_at: bool,
) -> Result<RecordBatch, FirnflowError> {
    let n = rows.len();
    let ids = UInt64Array::from_iter_values(rows.iter().map(|r| r.id));

    let vectors: ArrayRef = match kind {
        VectorKind::Single => {
            let values_builder = Float32Builder::with_capacity(n * dim);
            let mut list_builder = FixedSizeListBuilder::new(values_builder, dim as i32);
            for row in &rows {
                for &v in &row.vector {
                    list_builder.values().append_value(v);
                }
                list_builder.append(true);
            }
            Arc::new(list_builder.finish())
        }
        VectorKind::Multivector => {
            let values_builder = Float32Builder::new();
            let inner_builder = FixedSizeListBuilder::new(values_builder, dim as i32);
            let mut outer = ListBuilder::new(inner_builder);
            for row in &rows {
                let multi = row.vectors.as_ref().ok_or_else(|| {
                    FirnflowError::InvalidRequest(format!(
                        "row id {}: multivector namespace requires `vectors` payload",
                        row.id
                    ))
                })?;
                for sub in multi {
                    for &v in sub {
                        outer.values().values().append_value(v);
                    }
                    outer.values().append(true);
                }
                outer.append(true);
            }
            Arc::new(outer.finish())
        }
    };

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
        vectors,
        Arc::new(texts) as ArrayRef,
    ];

    if include_ingested_at {
        // Stamp every row in the batch with the same server-side
        // write timestamp. Merge-insert replaces all columns of a
        // matched row, so re-upserting an existing id advances this
        // value: it records the most recent write, not the first
        // insert.
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

/// Decode the per-row vector payload from a single batch row.
///
/// For [`VectorKind::Single`] this returns the row's full vector as
/// `Vec<f32>`. For [`VectorKind::Multivector`] it returns an empty
/// `Vec<f32>` — echoing the full bag of sub-vectors back through
/// every list/query response would balloon the payload by orders of
/// magnitude (a ColPali row holds ~1030 × 128 floats), so the v1
/// contract is "the bag is what you queried with, the server does
/// not echo it back". Callers that need the bag back can refetch by
/// id from a future endpoint.
fn extract_row_vector(
    batch: &RecordBatch,
    row: usize,
    kind: VectorKind,
    context: &str,
) -> Result<Vec<f32>, FirnflowError> {
    match kind {
        VectorKind::Single => {
            let vectors = batch
                .column_by_name("vector")
                .ok_or_else(|| FirnflowError::Backend(format!("{context}: missing vector column")))?
                .as_any()
                .downcast_ref::<FixedSizeListArray>()
                .ok_or_else(|| {
                    FirnflowError::Backend(format!("{context}: vector not FixedSizeList"))
                })?;
            let vector_arr = vectors.value(row);
            let vec_f32 = vector_arr
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| {
                    FirnflowError::Backend(format!("{context}: vector inner not Float32"))
                })?;
            Ok((0..vec_f32.len()).map(|i| vec_f32.value(i)).collect())
        }
        VectorKind::Multivector => {
            // Confirm the column shape but do not materialise the
            // bag — the response intentionally omits it.
            let _ = batch
                .column_by_name("vector")
                .ok_or_else(|| FirnflowError::Backend(format!("{context}: missing vector column")))?
                .as_any()
                .downcast_ref::<ListArray>()
                .ok_or_else(|| {
                    FirnflowError::Backend(format!(
                        "{context}: multivector column is not a List<FixedSizeList<Float32>>"
                    ))
                })?;
            Ok(Vec::new())
        }
    }
}

fn batches_to_list_rows(
    batches: &[RecordBatch],
    kind: VectorKind,
) -> Result<Vec<ListRow>, FirnflowError> {
    let mut out = Vec::new();
    for batch in batches {
        let ids = batch
            .column_by_name("id")
            .ok_or_else(|| FirnflowError::Backend("list: missing id column".into()))?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| FirnflowError::Backend("list: id not UInt64".into()))?;
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
            let vector = extract_row_vector(batch, row, kind, "list")?;
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

fn batches_to_results(
    batches: &[RecordBatch],
    kind: VectorKind,
) -> Result<Vec<QueryResult>, FirnflowError> {
    let mut out = Vec::new();
    for batch in batches {
        let ids = batch
            .column_by_name("id")
            .ok_or_else(|| FirnflowError::Backend("query result: missing id column".into()))?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| FirnflowError::Backend("query result: id not UInt64".into()))?;
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
            let vector = extract_row_vector(batch, row, kind, "query result")?;
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

    #[test]
    fn classify_index_types_buckets_by_family() {
        // No indexes: all false.
        assert_eq!(classify_index_types([].into_iter()), (false, false, false));

        // One of each family.
        assert_eq!(
            classify_index_types([IndexType::IvfPq, IndexType::FTS, IndexType::BTree].into_iter()),
            (true, true, true)
        );

        // HNSW variants count as vector; bitmap / label-list as scalar.
        assert_eq!(
            classify_index_types([IndexType::IvfHnswSq].into_iter()),
            (true, false, false)
        );
        assert_eq!(
            classify_index_types([IndexType::Bitmap, IndexType::LabelList].into_iter()),
            (false, false, true)
        );
    }
}
