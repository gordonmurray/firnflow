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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::builder::{
    BooleanBuilder, FixedSizeListBuilder, Float32Builder, Float64Builder, Int64Builder,
    ListBuilder, StringBuilder,
};
use arrow_array::{
    new_null_array, Array, ArrayRef, BooleanArray, FixedSizeListArray, Float32Array, Float64Array,
    Int64Array, ListArray, RecordBatch, RecordBatchIterator, RecordBatchReader, StringArray,
    TimestampMicrosecondArray, UInt64Array,
};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef, TimeUnit};
use dashmap::DashMap;
use futures::{StreamExt, TryStreamExt};
use lance::dataset::scanner::ColumnOrdering;
use lance::dataset::{WriteMode, WriteParams};
use lancedb::index::scalar::{BTreeIndexBuilder, FtsIndexBuilder, FullTextSearchQuery};
use lancedb::index::vector::IvfPqIndexBuilder;
use lancedb::index::{Index, IndexType};
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::table::{NewColumnTransform, OptimizeAction, WriteOptions};
use lancedb::DistanceType;
use object_store::aws::AmazonS3Builder;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectStorePath;
use object_store::ObjectStore;
use xxhash_rust::xxh3::xxh3_64;

use crate::metrics::CoreMetrics;
use crate::query::DEFAULT_NPROBES;
use crate::result::{
    FacetBucket, FacetField, FacetResultSet, ListOrder, ListPage, ListRow, NamespaceInfo,
};
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

/// Columns the `/scalar-index` endpoint will build a BTree on.
///
/// - `id` accelerates merge-insert match-finding on the write path:
///   without it, every `/upsert` batch scans each fragment to decide
///   which incoming ids are updates and which are inserts, so write
///   latency grows with table size. New namespaces get this index
///   automatically on first write; the endpoint is the maintenance
///   path for namespaces created before auto-indexing existed.
/// - `_ingested_at` lets `/list` cursor pages do an index range scan
///   instead of a full-fragment scan, mirroring the constraint the
///   `/list` endpoint puts on `order_by`.
///
/// The BTree only earns its keep on columns the read or write path
/// actually uses; future user-column ordering work extends this slice.
const SCALAR_INDEX_COLUMNS: &[&str] = &["id", INGESTED_AT_COLUMN];

const RESERVED_ATTRIBUTE_COLUMNS: &[&str] = &[
    "id",
    "vector",
    "vectors",
    "text",
    INGESTED_AT_COLUMN,
    DISTANCE_COLUMN,
    SCORE_COLUMN,
    RELEVANCE_COLUMN,
];

/// Validate that `column` is one the scalar-index path will build a
/// BTree on, returning [`FirnflowError::InvalidRequest`] otherwise.
///
/// Exposed so the API layer can reject an unsupported column with a
/// synchronous `400` before it spawns the background build task —
/// the same reasoning as [`crate::validate_ivf_pq_options`], which
/// avoids a misleading `202` followed by a log-only failure.
pub fn validate_scalar_index_column(column: &str) -> Result<(), FirnflowError> {
    if SCALAR_INDEX_COLUMNS.contains(&column) {
        Ok(())
    } else {
        Err(FirnflowError::InvalidRequest(format!(
            "scalar index column {column:?} is not supported; \
             valid columns: {SCALAR_INDEX_COLUMNS:?}"
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Attribute column types accepted by JSON upsert, Arrow import,
/// filters, and facets. The first four are scalars; [`StringList`] is
/// a variable-length list of strings (e.g. tags/genres) — filterable
/// with `array_has(col, 'value')` and faceted per element.
///
/// [`StringList`]: AttributeType::StringList
pub enum AttributeType {
    /// Signed 64-bit integer attribute.
    Int64,
    /// 64-bit floating point attribute.
    Float64,
    /// Boolean attribute.
    Boolean,
    /// UTF-8 string attribute.
    Utf8,
    /// Variable-length list of UTF-8 strings (`List<Utf8>`). Multi-valued:
    /// `array_has(col, 'v')` filters it and a facet counts each element.
    StringList,
}

impl AttributeType {
    fn arrow(self) -> DataType {
        match self {
            Self::Int64 => DataType::Int64,
            Self::Float64 => DataType::Float64,
            Self::Boolean => DataType::Boolean,
            Self::Utf8 => DataType::Utf8,
            // Item field "item", nullable — matches arrow's ListBuilder
            // default so the built array's type equals the table schema.
            Self::StringList => DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
        }
    }

    fn from_arrow(dt: &DataType) -> Option<Self> {
        match dt {
            DataType::Int64 => Some(Self::Int64),
            DataType::Float64 => Some(Self::Float64),
            DataType::Boolean => Some(Self::Boolean),
            DataType::Utf8 => Some(Self::Utf8),
            DataType::List(item) if *item.data_type() == DataType::Utf8 => Some(Self::StringList),
            _ => None,
        }
    }

    fn sql_null_cast(self) -> &'static str {
        match self {
            Self::Int64 => "CAST(NULL AS BIGINT)",
            Self::Float64 => "CAST(NULL AS DOUBLE)",
            Self::Boolean => "CAST(NULL AS BOOLEAN)",
            Self::Utf8 => "CAST(NULL AS VARCHAR)",
            // Backfill an all-null list column on an existing table.
            Self::StringList => "arrow_cast(NULL, 'List(Utf8)')",
        }
    }
}

fn validate_attribute_name(name: &str) -> Result<(), FirnflowError> {
    if RESERVED_ATTRIBUTE_COLUMNS.contains(&name) || name.starts_with('_') {
        return Err(FirnflowError::InvalidRequest(format!(
            "attribute name {name:?} is reserved"
        )));
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => {
            return Err(FirnflowError::InvalidRequest(format!(
                "attribute name {name:?} must start with an ASCII letter"
            )));
        }
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(FirnflowError::InvalidRequest(format!(
            "attribute name {name:?} must contain only ASCII letters, digits, and underscores"
        )));
    }
    Ok(())
}

fn infer_attribute_type(value: &serde_json::Value) -> Result<Option<AttributeType>, FirnflowError> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Bool(_) => Ok(Some(AttributeType::Boolean)),
        serde_json::Value::String(_) => Ok(Some(AttributeType::Utf8)),
        serde_json::Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                Ok(Some(AttributeType::Int64))
            } else {
                Ok(Some(AttributeType::Float64))
            }
        }
        serde_json::Value::Array(items) => {
            // A list attribute. An empty list carries no type signal, so it
            // is treated like null (no column created from it alone); a later
            // non-empty list in the request/namespace fixes the type.
            if items.is_empty() {
                Ok(None)
            } else if items.iter().all(serde_json::Value::is_string) {
                Ok(Some(AttributeType::StringList))
            } else {
                Err(FirnflowError::InvalidRequest(
                    "list attributes must contain only strings".into(),
                ))
            }
        }
        _ => Err(FirnflowError::InvalidRequest(
            "attributes must be JSON scalars, a list of strings, or null".into(),
        )),
    }
}

fn infer_request_attributes(
    rows: &[UpsertRow],
) -> Result<BTreeMap<String, AttributeType>, FirnflowError> {
    let mut out = BTreeMap::new();
    for row in rows {
        for (name, value) in &row.attributes {
            validate_attribute_name(name)?;
            let Some(ty) = infer_attribute_type(value)? else {
                continue;
            };
            match out.get(name) {
                Some(existing) if *existing != ty => {
                    return Err(FirnflowError::InvalidRequest(format!(
                        "attribute {name:?} has conflicting types in request: \
                         {:?} and {:?}",
                        existing, ty
                    )));
                }
                Some(_) => {}
                None => {
                    out.insert(name.clone(), ty);
                }
            }
        }
    }
    Ok(out)
}

fn reconcile_attribute_schema(
    info: &mut NamespaceSchemaInfo,
    request: &BTreeMap<String, AttributeType>,
) -> Result<BTreeMap<String, AttributeType>, FirnflowError> {
    let mut added = BTreeMap::new();
    for (name, ty) in request {
        match info.attributes.get(name) {
            Some(existing) if existing != ty => {
                return Err(FirnflowError::InvalidRequest(format!(
                    "attribute {name:?} is {:?} in this namespace, got {:?}",
                    existing, ty
                )));
            }
            Some(_) => {}
            None => {
                info.attributes.insert(name.clone(), *ty);
                added.insert(name.clone(), *ty);
            }
        }
    }
    Ok(added)
}

async fn add_null_attribute_columns(
    tbl: &lancedb::Table,
    attributes: &BTreeMap<String, AttributeType>,
) -> Result<(), FirnflowError> {
    if attributes.is_empty() {
        return Ok(());
    }
    let transforms: Vec<(String, String)> = attributes
        .iter()
        .map(|(name, ty)| (name.clone(), ty.sql_null_cast().to_string()))
        .collect();
    tbl.add_columns(NewColumnTransform::SqlExpressions(transforms), None)
        .await
        .map_err(|e| FirnflowError::Backend(format!("add attribute columns: {e}")))?;
    Ok(())
}

/// Per-namespace schema facts cached after the first table open.
/// The resolved vector dimension, the kind of vector representation
/// (single vs multivector), and whether the table carries the
/// `_ingested_at` system column all come from the same schema read
/// and are stored together.
#[derive(Debug, Clone)]
struct NamespaceSchemaInfo {
    /// For [`VectorKind::Single`] this is the vector dimension. For
    /// [`VectorKind::Multivector`] this is the inner sub-vector
    /// dimension (each sub-vector is fixed at this width).
    dim: usize,
    kind: VectorKind,
    has_ingested_at: bool,
    attributes: BTreeMap<String, AttributeType>,
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
    /// User-defined scalar attributes.
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

impl From<(u64, Vec<f32>)> for UpsertRow {
    fn from((id, vector): (u64, Vec<f32>)) -> Self {
        Self {
            id,
            vector,
            vectors: None,
            text: None,
            attributes: serde_json::Map::new(),
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
    /// Optional lance [`Session`] whose object-store registry wraps cloud
    /// stores with the local-NVMe byte-range cache (issue #51). When set, it is
    /// passed to every `lancedb::connect()` so Lance reads are served from the
    /// cache. `None` disables the object cache (default).
    object_cache_session: Option<Arc<lance::session::Session>>,
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
            object_cache_session: None,
        }
    }

    /// Enable the local-NVMe object cache (issue #51) by supplying a lance
    /// [`Session`] whose object-store registry wraps cloud reads with the
    /// byte-range cache (build one via
    /// [`crate::object_cache::build_cached_session`]). Every subsequent
    /// `lancedb::connect()` routes its object-store reads through the cache.
    pub fn with_object_cache_session(mut self, session: Arc<lance::session::Session>) -> Self {
        self.object_cache_session = Some(session);
        self
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
    fn schema_for_kind(
        kind: VectorKind,
        dim: usize,
        with_ingested_at: bool,
        attributes: &BTreeMap<String, AttributeType>,
    ) -> Arc<Schema> {
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
        for (name, ty) in attributes {
            fields.push(Field::new(name, ty.arrow(), true));
        }
        Arc::new(Schema::new(fields))
    }

    async fn connect(&self, ns: &NamespaceId) -> Result<lancedb::Connection, FirnflowError> {
        let mut builder =
            lancedb::connect(&self.uri(ns)).storage_options(self.storage_options.clone());
        if let Some(session) = &self.object_cache_session {
            // Route Lance object-store reads through the local-NVMe byte-range
            // cache (issue #51). Sharing one session across connections reuses
            // the wrapped stores and their cache.
            builder = builder.session(session.clone());
        }
        builder
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
    ///
    /// Also exposed publicly so the benchmark harness can simulate a
    /// "dropped handle" measurement case without triggering a
    /// destructive op. A no-op when no handle is currently pooled.
    pub fn evict_handle(&self, ns: &NamespaceId) {
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
        attributes: &BTreeMap<String, AttributeType>,
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
                let schema = Self::schema_for_kind(kind, dim, true, attributes);
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
            return Ok(Some(info.clone()));
        }
        if let Some((_tbl, info)) = self.open_existing(ns).await? {
            self.schema_info.insert(ns.clone(), info.clone());
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
    /// finds matches on `id`; a BTree index on `id` lets that lookup
    /// use the index instead of scanning every fragment, so on the
    /// first write to a fresh namespace this builds that index once
    /// (the table is still small). Namespaces created before this
    /// behaviour existed can build it through `create_scalar_index`
    /// with the `id` column. The build is best-effort: the write has
    /// already committed by the time it runs, so a failure is logged
    /// and the upsert still succeeds.
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
        // legacy shape so writes continue to match. A `None` here
        // means the table does not exist yet, so this call creates it
        // — the one moment we build the `id` index, while it is empty
        // or near-empty.
        let request_attributes = infer_request_attributes(&rows)?;
        let (mut info, is_fresh) = match self.resolve_schema_info(ns).await? {
            Some(info) => (info, false),
            None => {
                let (kind, dim) = inspect_row_payload(&rows[0])?;
                (
                    NamespaceSchemaInfo {
                        dim,
                        kind,
                        has_ingested_at: true,
                        attributes: request_attributes.clone(),
                    },
                    true,
                )
            }
        };
        let new_attributes = reconcile_attribute_schema(&mut info, &request_attributes)?;

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
        let mut tbl = self
            .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
            .await?;
        if !new_attributes.is_empty() && !is_fresh {
            add_null_attribute_columns(&tbl, &new_attributes).await?;
            self.evict_handle(ns);
            tbl = self
                .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
                .await?;
        }
        let schema =
            Self::schema_for_kind(info.kind, info.dim, info.has_ingested_at, &info.attributes);
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

        self.schema_info.insert(ns.clone(), info.clone());

        // On the namespace's first write, build a BTree on `id` so
        // every subsequent merge-insert finds its matches through the
        // index rather than scanning each fragment — the write-path
        // cost that otherwise grows with table size. Best-effort: the
        // rows above are already durable, so an index-build failure is
        // logged and the upsert still returns success (the index can
        // be rebuilt later through `create_scalar_index`).
        if is_fresh {
            match self.build_id_index(&tbl).await {
                Ok(()) => {
                    // The index build is a Lance commit, so the handle
                    // pooled while creating the table now references the
                    // pre-index view. Drop it and open a fresh one in its
                    // place: later merge-insert batches then see and use
                    // the index, and the pool stays warm — an upsert must
                    // not leave it cold (the explicit index builders evict
                    // and let the next call re-open lazily; here the next
                    // call is the warm upsert, so re-pool eagerly).
                    self.evict_handle(ns);
                    if let Err(e) = self
                        .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
                        .await
                    {
                        tracing::warn!(
                            namespace = %ns,
                            error = %e,
                            "could not re-pool handle after id index build; \
                             it will reopen on the next operation"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        namespace = %ns,
                        error = %e,
                        "auto-build of id index failed on first write; \
                         rebuild it with POST /ns/{ns}/scalar-index column=id"
                    );
                }
            }
        }
        Ok(())
    }

    /// Build a BTree scalar index on the `id` column of an already
    /// open table. Used by [`upsert`](Self::upsert) on a namespace's
    /// first write and by [`create_scalar_index`](Self::create_scalar_index)
    /// for the `id` maintenance path. Idempotent: `lancedb`'s
    /// `IndexBuilder` defaults to `replace=true`.
    async fn build_id_index(&self, tbl: &lancedb::Table) -> Result<(), FirnflowError> {
        tbl.create_index(&["id"], Index::BTree(BTreeIndexBuilder::default()))
            .execute()
            .await
            .map_err(|e| FirnflowError::Backend(format!("build id index: {e}")))?;
        Ok(())
    }

    /// Bulk-append the rows from an Arrow IPC stream, insert-only, in a
    /// single Lance commit.
    ///
    /// This is the binary bulk-ingest path behind `POST /ns/{ns}/import`.
    /// Unlike [`upsert`](Self::upsert) it does **not** merge by `id`: the
    /// caller asserts the ids are new, so there is no per-batch match
    /// scan, and the whole stream is written as one append commit
    /// (`WriteMode::Append`, `skip_auto_cleanup`) regardless of how many
    /// Arrow batches it carries — `Table::add` pulls the wrapped reader
    /// one batch at a time, so peak memory stays at a single batch.
    ///
    /// The incoming schema is validated up front
    /// ([`validate_arrow_import_schema`]); each batch is then validated
    /// row-by-row (non-null `id`, non-null vector payload, non-empty
    /// multivector lists) and projected into the canonical table schema
    /// with a server-set `_ingested_at`. On a fresh namespace the
    /// stream's kind/dim fix the namespace; on an existing one they must
    /// match. No `id` index is built (this path never does a merge
    /// lookup — build indexes after the load, per the large-ingest
    /// recipe). Returns the number of rows appended.
    pub async fn import_arrow(
        &self,
        ns: &NamespaceId,
        reader: Box<dyn RecordBatchReader + Send>,
    ) -> Result<usize, FirnflowError> {
        let in_schema = reader.schema();
        let (kind, dim, has_text, import_attributes) = validate_arrow_import_schema(&in_schema)?;

        // Fresh namespace: the stream fixes kind/dim. Existing: it must
        // match the established shape.
        let (info, new_import_attributes) = match self.resolve_schema_info(ns).await? {
            Some(info) => {
                if info.kind != kind || info.dim != dim {
                    return Err(FirnflowError::InvalidRequest(format!(
                        "import: stream is {} dim {dim}, but namespace {ns} is {} dim {}",
                        kind.as_label(),
                        info.kind.as_label(),
                        info.dim,
                    )));
                }
                let mut info = info;
                let added = reconcile_attribute_schema(&mut info, &import_attributes)?;
                (info, added)
            }
            None => (
                NamespaceSchemaInfo {
                    dim,
                    kind,
                    has_ingested_at: true,
                    attributes: import_attributes,
                },
                BTreeMap::new(),
            ),
        };

        let mut tbl = self
            .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
            .await?;
        if !new_import_attributes.is_empty() {
            add_null_attribute_columns(&tbl, &new_import_attributes).await?;
            self.evict_handle(ns);
            tbl = self
                .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
                .await?;
        }
        let canonical = Self::schema_for_kind(info.kind, info.dim, true, &info.attributes);

        // Column positions in the incoming stream (any order).
        let id_idx = in_schema
            .index_of("id")
            .map_err(|e| FirnflowError::InvalidRequest(format!("import: {e}")))?;
        let vec_name = match info.kind {
            VectorKind::Single => "vector",
            VectorKind::Multivector => "vectors",
        };
        let vec_idx = in_schema
            .index_of(vec_name)
            .map_err(|e| FirnflowError::InvalidRequest(format!("import: {e}")))?;
        let text_idx = if has_text {
            in_schema.index_of("text").ok()
        } else {
            None
        };
        let attr_indices: Vec<(String, usize)> = info
            .attributes
            .keys()
            .filter_map(|name| in_schema.index_of(name).ok().map(|idx| (name.clone(), idx)))
            .collect();

        let rows = Arc::new(AtomicU64::new(0));
        let ingest = IngestReader {
            inner: reader,
            canonical: canonical.clone(),
            kind: info.kind,
            id_idx,
            vec_idx,
            text_idx,
            attr_indices,
            ts_micros: current_micros(),
            rows: Arc::clone(&rows),
        };

        // One append commit for the whole stream. `WriteParams::default()`
        // is `Create`, so `Append` must be set explicitly;
        // `skip_auto_cleanup` drops the per-commit old-version cleanup that
        // adds up under high-frequency ingest (run compaction as explicit
        // maintenance instead).
        let write_params = WriteParams {
            mode: WriteMode::Append,
            skip_auto_cleanup: true,
            ..Default::default()
        };
        let ingest: Box<dyn RecordBatchReader + Send> = Box::new(ingest);
        tbl.add(ingest)
            .write_options(WriteOptions {
                lance_write_params: Some(write_params),
            })
            .execute()
            .await
            .map_err(map_import_lance_error)?;

        self.schema_info.insert(ns.clone(), info);
        Ok(rows.load(Ordering::Relaxed) as usize)
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
            Scheme::Local => self.build_local_object_store(),
        }
    }

    /// Build a local-filesystem `object_store` client rooted at the
    /// configured base directory (embedded mode). Used by the namespace
    /// delete path, which lists and removes objects under the namespace
    /// prefix; the read/write path opens local Lance tables directly
    /// through `lancedb::connect` with the `file://` URI.
    ///
    /// The base directory is created if absent so that
    /// `LocalFileSystem::new_with_prefix` — which canonicalises the
    /// prefix and requires it to exist — succeeds even before any
    /// namespace has been written.
    fn build_local_object_store(&self) -> Result<Arc<dyn ObjectStore>, FirnflowError> {
        let dir = self.storage_root.bucket();
        std::fs::create_dir_all(dir)
            .map_err(|e| FirnflowError::Backend(format!("create local storage dir {dir}: {e}")))?;
        let store = LocalFileSystem::new_with_prefix(dir)
            .map_err(|e| FirnflowError::Backend(format!("build local object store: {e}")))?;
        Ok(Arc::new(store))
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
    ///
    /// `include_vector: false` projects the stored vector column out
    /// of the result batches — hits carry `id`, `score`, `text`, and
    /// `ingested_at_micros` but `vector: None`. The query vector is
    /// still used for search; only the response materialisation
    /// changes.
    ///
    /// `filter` is a DataFusion SQL predicate applied through
    /// LanceDB's prefilter (`only_if`) before vector ranking or FTS
    /// scoring. Malformed predicates are reported as
    /// [`FirnflowError::InvalidRequest`].
    #[allow(clippy::too_many_arguments)]
    pub async fn query(
        &self,
        ns: &NamespaceId,
        vector: Vec<f32>,
        vectors: Option<Vec<Vec<f32>>>,
        k: usize,
        nprobes: Option<usize>,
        text: Option<String>,
        filter: Option<String>,
        include_vector: bool,
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
        let tbl = self
            .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
            .await?;

        // Opting out of the stored vector becomes a column projection:
        // Lance then never materialises the vector column into the
        // result batches (for multivector namespaces that is the whole
        // bag of sub-vectors). The score column (`_distance`, `_score`,
        // or `_relevance_score`) is auto-projected by Lance on top of
        // the selection. `id` and `text` are always in the schema;
        // `_ingested_at` only exists on tables created since the
        // column was introduced.
        let projection: Option<Vec<&str>> = if include_vector {
            None
        } else {
            let mut cols = vec!["id", "text"];
            if info.has_ingested_at {
                cols.push(INGESTED_AT_COLUMN);
            }
            for name in info.attributes.keys() {
                cols.push(name.as_str());
            }
            Some(cols)
        };

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
            if let Some(ref f) = filter {
                vq = vq.only_if(f.clone());
            }
            if let Some(ref cols) = projection {
                vq = vq.select(Select::columns(cols));
            }
            vq.execute().await.map_err(|e| {
                if filter.is_some() {
                    FirnflowError::InvalidRequest(format!("query filter: {e}"))
                } else {
                    FirnflowError::Backend(format!("query.execute: {e}"))
                }
            })?
        } else {
            // FTS-only
            let t = text.unwrap();
            let mut q = tbl
                .query()
                .full_text_search(FullTextSearchQuery::new(t))
                .limit(k);
            if let Some(ref f) = filter {
                q = q.only_if(f.clone());
            }
            if let Some(ref cols) = projection {
                q = q.select(Select::columns(cols));
            }
            q.execute().await.map_err(|e| {
                if filter.is_some() {
                    FirnflowError::InvalidRequest(format!("query filter: {e}"))
                } else {
                    FirnflowError::Backend(format!("fts.execute: {e}"))
                }
            })?
        };

        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| FirnflowError::Backend(format!("query.collect: {e}")))?;

        let results = batches_to_results(&batches, info.kind, include_vector)?;
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

        let tbl = self
            .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
            .await?;

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

        let tbl = self
            .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
            .await?;
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
    /// Accepts the columns in [`SCALAR_INDEX_COLUMNS`]: `id` to speed
    /// up merge-insert match-finding on the write path, and
    /// `_ingested_at` to let `/list` cursor pages do an index range
    /// scan instead of a full-fragment scan and let the leading
    /// `ORDER BY _ingested_at` short-circuit the in-memory sort.
    ///
    /// New namespaces get the `id` index automatically on first
    /// write; this endpoint is the maintenance path for building it
    /// on namespaces created before auto-indexing existed.
    ///
    /// Idempotent: `lancedb`'s `IndexBuilder` defaults to
    /// `replace=true`, so a repeat call rebuilds the index in place
    /// rather than failing.
    ///
    /// Returns [`FirnflowError::Unsupported`] for namespaces whose
    /// tables pre-date the `_ingested_at` column when that column is
    /// requested — same gate the `/list` endpoint applies.
    ///
    /// Like the other index builders, this is potentially expensive
    /// on large tables; the API handler runs it in a background task.
    pub async fn create_scalar_index(
        &self,
        ns: &NamespaceId,
        column: &str,
    ) -> Result<(), FirnflowError> {
        let info = self.resolve_schema_info(ns).await?.ok_or_else(|| {
            FirnflowError::InvalidRequest(format!(
                "cannot create scalar index on namespace {ns}: no data has been upserted yet"
            ))
        })?;
        self.validate_scalar_index_column_for_info(ns, column, &info)?;

        if column == INGESTED_AT_COLUMN && !info.has_ingested_at {
            return Err(FirnflowError::Unsupported(format!(
                "namespace {ns} pre-dates the _ingested_at column; \
                 recreate the namespace before building a scalar index on it"
            )));
        }

        let tbl = self
            .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
            .await?;
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

    /// Validate that a scalar index can be built on `column` for the
    /// namespace's live schema.
    pub async fn validate_scalar_index_column_for_namespace(
        &self,
        ns: &NamespaceId,
        column: &str,
    ) -> Result<(), FirnflowError> {
        let info = self.resolve_schema_info(ns).await?.ok_or_else(|| {
            FirnflowError::InvalidRequest(format!(
                "cannot create scalar index on namespace {ns}: no data has been upserted yet"
            ))
        })?;
        self.validate_scalar_index_column_for_info(ns, column, &info)
    }

    fn validate_scalar_index_column_for_info(
        &self,
        ns: &NamespaceId,
        column: &str,
        info: &NamespaceSchemaInfo,
    ) -> Result<(), FirnflowError> {
        if SCALAR_INDEX_COLUMNS.contains(&column) || info.attributes.contains_key(column) {
            if column == INGESTED_AT_COLUMN && !info.has_ingested_at {
                return Err(FirnflowError::Unsupported(format!(
                    "namespace {ns} pre-dates the _ingested_at column; \
                     recreate the namespace before building a scalar index on it"
                )));
            }
            Ok(())
        } else {
            Err(FirnflowError::InvalidRequest(format!(
                "scalar index column {column:?} is not supported; valid columns are `id`, \
                 `{INGESTED_AT_COLUMN}`, or a scalar attribute column"
            )))
        }
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

        let tbl = self
            .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
            .await?;
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

        let tbl = self
            .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
            .await?;
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

    /// Compute value-count facets over the full filtered row set.
    ///
    /// This is independent of vector ranking and top-k: the scan
    /// projects only the requested scalar columns, applies `filter`
    /// when supplied, and counts every matching row.
    pub async fn facet(
        &self,
        ns: &NamespaceId,
        filter: Option<String>,
        fields: &[String],
        top: usize,
    ) -> Result<FacetResultSet, FirnflowError> {
        let info = match self.resolve_schema_info(ns).await? {
            Some(i) => i,
            None => return Ok(FacetResultSet { facets: Vec::new() }),
        };
        for field in fields {
            validate_facet_field(field, &info)?;
        }

        let tbl = self
            .get_or_open_table(ns, info.kind, info.dim, &info.attributes)
            .await?;
        let dataset_wrapper = tbl
            .dataset()
            .ok_or_else(|| FirnflowError::Backend("facet requires a native lance table".into()))?;
        let dataset = dataset_wrapper
            .get()
            .await
            .map_err(|e| FirnflowError::Backend(format!("resolve dataset: {e}")))?;

        let mut scan = dataset.scan();
        scan.project(fields)
            .map_err(|e| FirnflowError::Backend(format!("facet project: {e}")))?;
        if let Some(ref predicate) = filter {
            scan.filter(predicate)
                .map_err(|e| FirnflowError::InvalidRequest(format!("facet filter: {e}")))?;
        }
        let stream = scan.try_into_stream().await.map_err(|e| {
            if filter.is_some() {
                FirnflowError::InvalidRequest(format!("facet filter: {e}"))
            } else {
                FirnflowError::Backend(format!("facet scan: {e}"))
            }
        })?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| FirnflowError::Backend(format!("facet collect: {e}")))?;

        let mut counts: Vec<HashMap<String, (serde_json::Value, u64)>> =
            fields.iter().map(|_| HashMap::new()).collect();
        for batch in &batches {
            for (field_idx, field) in fields.iter().enumerate() {
                let col = batch.column_by_name(field).ok_or_else(|| {
                    FirnflowError::Backend(format!("facet: missing projected column {field}"))
                })?;
                for row in 0..batch.num_rows() {
                    // A list column contributes one count per element (a book
                    // counts once per genre); a scalar contributes one value.
                    for value in facet_values_at(col, row)? {
                        let key = facet_value_sort_key(&value);
                        let entry = counts[field_idx].entry(key).or_insert((value, 0));
                        entry.1 += 1;
                    }
                }
            }
        }

        let facets = fields
            .iter()
            .zip(counts)
            .map(|(field, field_counts)| {
                let mut buckets: Vec<FacetBucket> = field_counts
                    .into_values()
                    .map(|(value, count)| FacetBucket { value, count })
                    .collect();
                buckets.sort_by(|a, b| {
                    b.count.cmp(&a.count).then_with(|| {
                        facet_value_sort_key(&a.value).cmp(&facet_value_sort_key(&b.value))
                    })
                });
                let truncated = buckets.len() > top;
                if truncated {
                    buckets.truncate(top);
                }
                FacetField {
                    field: field.clone(),
                    buckets,
                    truncated,
                }
            })
            .collect();
        Ok(FacetResultSet { facets })
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
    let mut attributes = BTreeMap::new();
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
            "id" | "text" => {}
            name => {
                if let Some(ty) = AttributeType::from_arrow(field.data_type()) {
                    attributes.insert(name.to_string(), ty);
                }
            }
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
        attributes,
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
/// Extract the dimension of a `FixedSizeList<Float32, dim>` data type,
/// ignoring child-field naming. Returns `None` for any other type.
fn fixed_size_float_dim(dt: &DataType) -> Option<usize> {
    match dt {
        DataType::FixedSizeList(field, size) if field.data_type() == &DataType::Float32 => {
            Some(*size as usize)
        }
        _ => None,
    }
}

/// Extract the inner dimension of a `List<FixedSizeList<Float32, dim>>`
/// (the multivector payload type). Returns `None` for any other type.
fn list_of_fixed_size_float_dim(dt: &DataType) -> Option<usize> {
    match dt {
        DataType::List(field) => fixed_size_float_dim(field.data_type()),
        _ => None,
    }
}

/// Validate the Arrow schema of a `/import` stream and report the
/// namespace kind, vector dimension, and whether a `text` column is
/// present.
///
/// The stream must carry `id` (`UInt64`) plus exactly one vector
/// payload column, mirroring the JSON field names: `vector`
/// (`FixedSizeList<Float32, dim>`, single-vector) or `vectors`
/// (`List<FixedSizeList<Float32, dim>>`, multivector). An optional
/// `text` column must be `Utf8`. The server owns `_ingested_at`, so it
/// must **not** be supplied. The vector column's element type must
/// match Firn's canonical shape exactly (child field `item`, `Float32`)
/// so Lance appends it without a cast.
///
/// Returns [`FirnflowError::InvalidRequest`] for any missing, extra, or
/// mistyped column, letting the API layer answer `400` before it starts
/// the background import.
pub fn validate_arrow_import_schema(
    schema: &Schema,
) -> Result<(VectorKind, usize, bool, BTreeMap<String, AttributeType>), FirnflowError> {
    match schema.column_with_name("id") {
        Some((_, f)) if f.data_type() == &DataType::UInt64 => {}
        Some((_, f)) => {
            return Err(FirnflowError::InvalidRequest(format!(
                "import: column `id` must be UInt64, got {:?}",
                f.data_type()
            )));
        }
        None => {
            return Err(FirnflowError::InvalidRequest(
                "import: required column `id` (UInt64) is missing".into(),
            ));
        }
    }

    if schema.column_with_name(INGESTED_AT_COLUMN).is_some() {
        return Err(FirnflowError::InvalidRequest(format!(
            "import: column `{INGESTED_AT_COLUMN}` is server-owned and must not be included"
        )));
    }

    let has_vector = schema.column_with_name("vector").is_some();
    let has_vectors = schema.column_with_name("vectors").is_some();
    let (kind, dim) = match (has_vector, has_vectors) {
        (true, true) => {
            return Err(FirnflowError::InvalidRequest(
                "import: set exactly one of `vector` or `vectors`, not both".into(),
            ));
        }
        (false, false) => {
            return Err(FirnflowError::InvalidRequest(
                "import: missing vector payload (column `vector` for single-vector \
                 or `vectors` for multivector namespaces)"
                    .into(),
            ));
        }
        (true, false) => {
            let (_, f) = schema.column_with_name("vector").expect("checked present");
            let dim = fixed_size_float_dim(f.data_type()).ok_or_else(|| {
                FirnflowError::InvalidRequest(format!(
                    "import: column `vector` must be FixedSizeList<Float32, dim>, got {:?}",
                    f.data_type()
                ))
            })?;
            (VectorKind::Single, dim)
        }
        (false, true) => {
            let (_, f) = schema.column_with_name("vectors").expect("checked present");
            let dim = list_of_fixed_size_float_dim(f.data_type()).ok_or_else(|| {
                FirnflowError::InvalidRequest(format!(
                    "import: column `vectors` must be List<FixedSizeList<Float32, dim>>, got {:?}",
                    f.data_type()
                ))
            })?;
            (VectorKind::Multivector, dim)
        }
    };

    if dim == 0 {
        return Err(FirnflowError::InvalidRequest(
            "import: vector dimension must be non-zero".into(),
        ));
    }

    // Require the exact canonical element type (child field `item`,
    // Float32, nullable) so `Table::add` accepts the column without a
    // cast. The dim probes above are lenient on child naming; this is
    // the strict gate.
    let canonical = NamespaceManager::schema_for_kind(kind, dim, false, &BTreeMap::new());
    let canonical_vec = canonical.field(1).data_type();
    let vec_name = match kind {
        VectorKind::Single => "vector",
        VectorKind::Multivector => "vectors",
    };
    let (_, f) = schema.column_with_name(vec_name).expect("checked present");
    if f.data_type() != canonical_vec {
        return Err(FirnflowError::InvalidRequest(format!(
            "import: column `{vec_name}` must be {canonical_vec:?} \
             (Float32 elements, child field `item`), got {:?}",
            f.data_type()
        )));
    }

    let has_text = match schema.column_with_name("text") {
        Some((_, f)) if f.data_type() == &DataType::Utf8 => true,
        Some((_, f)) => {
            return Err(FirnflowError::InvalidRequest(format!(
                "import: column `text` must be Utf8, got {:?}",
                f.data_type()
            )));
        }
        None => false,
    };

    // Extra columns are accepted as scalar attributes. Arrow permits
    // duplicate field names and column lookup returns the first match,
    // so a repeated column would silently shadow data — reject those too.
    const BUILTIN: &[&str] = &["id", "vector", "vectors", "text"];
    let mut seen = HashSet::new();
    let mut attributes = BTreeMap::new();
    for field in schema.fields() {
        let name = field.name().as_str();
        if !seen.insert(name) {
            return Err(FirnflowError::InvalidRequest(format!(
                "import: duplicate column `{name}`; each column may appear at most once"
            )));
        }
        if BUILTIN.contains(&name) {
            continue;
        }
        validate_attribute_name(name).map_err(|e| match e {
            FirnflowError::InvalidRequest(msg) => {
                FirnflowError::InvalidRequest(format!("import: {msg}"))
            }
            other => other,
        })?;
        let ty = AttributeType::from_arrow(field.data_type()).ok_or_else(|| {
            FirnflowError::InvalidRequest(format!(
                "import: attribute column `{name}` must be Int64, Float64, Boolean, Utf8, \
                 or List<Utf8>, got {:?}",
                field.data_type()
            ))
        })?;
        attributes.insert(name.to_string(), ty);
    }

    Ok((kind, dim, has_text, attributes))
}

/// Classify a `Table::add` failure: an Arrow error means the client's
/// stream was malformed or a row failed validation (the
/// [`IngestReader`] surfaces row-level rejections as `ArrowError`), so
/// it maps to a caller-facing `InvalidRequest`; anything else is a
/// backend (S3 / commit) failure.
fn map_import_lance_error(e: lancedb::Error) -> FirnflowError {
    match e {
        lancedb::Error::Arrow { .. } => {
            FirnflowError::InvalidRequest(format!("import: malformed Arrow data: {e}"))
        }
        other => FirnflowError::Backend(format!("table.add: {other}")),
    }
}

/// Adapts a client `/import` Arrow stream into Firn's canonical table
/// schema. For each batch it validates the rows, appends a server-set
/// `_ingested_at`, and emits a batch matching `schema_for_kind`. Handed
/// to `Table::add`, which pulls one batch at a time, so the whole
/// stream lands in a single commit with peak memory of one batch.
struct IngestReader {
    inner: Box<dyn RecordBatchReader + Send>,
    canonical: SchemaRef,
    kind: VectorKind,
    id_idx: usize,
    vec_idx: usize,
    text_idx: Option<usize>,
    attr_indices: Vec<(String, usize)>,
    ts_micros: i64,
    rows: Arc<AtomicU64>,
}

impl IngestReader {
    fn project(&self, batch: &RecordBatch) -> Result<RecordBatch, ArrowError> {
        let n = batch.num_rows();

        let id = batch.column(self.id_idx).clone();
        if id.null_count() != 0 {
            return Err(ArrowError::InvalidArgumentError(
                "import: column `id` contains null values".into(),
            ));
        }

        let vector = batch.column(self.vec_idx).clone();
        if vector.null_count() != 0 {
            return Err(ArrowError::InvalidArgumentError(
                "import: vector payload contains null rows".into(),
            ));
        }

        // Reject nulls anywhere inside the vector payload, not just at the
        // top level. A valid Arrow `FixedSizeList<Float32>` can carry a
        // non-null row with null float children, and a multivector input
        // can carry null inner sub-vectors or null floats — none of which
        // the JSON `/upsert` path can produce, so `/import` rejects them
        // too. (`FixedSizeList` makes the per-vector width structural, so
        // there is no separate dim check.)
        match self.kind {
            VectorKind::Single => {
                let fsl = vector
                    .as_any()
                    .downcast_ref::<FixedSizeListArray>()
                    .ok_or_else(|| {
                        ArrowError::InvalidArgumentError(
                            "import: `vector` is not a FixedSizeList array".into(),
                        )
                    })?;
                if fsl.values().null_count() != 0 {
                    return Err(ArrowError::InvalidArgumentError(
                        "import: `vector` contains null float values".into(),
                    ));
                }
            }
            VectorKind::Multivector => {
                let list = vector.as_any().downcast_ref::<ListArray>().ok_or_else(|| {
                    ArrowError::InvalidArgumentError("import: `vectors` is not a List array".into())
                })?;
                // Each row needs at least one sub-vector (an empty list has
                // no MaxSim contribution; the JSON path never makes one).
                let offsets = list.value_offsets();
                if offsets.windows(2).any(|w| w[1] - w[0] == 0) {
                    return Err(ArrowError::InvalidArgumentError(
                        "import: a row has an empty `vectors` list (multivector rows need at \
                         least one sub-vector)"
                            .into(),
                    ));
                }
                let inner = list.values();
                if inner.null_count() != 0 {
                    return Err(ArrowError::InvalidArgumentError(
                        "import: `vectors` contains null sub-vectors".into(),
                    ));
                }
                let inner_fsl = inner
                    .as_any()
                    .downcast_ref::<FixedSizeListArray>()
                    .ok_or_else(|| {
                        ArrowError::InvalidArgumentError(
                            "import: `vectors` inner is not a FixedSizeList array".into(),
                        )
                    })?;
                if inner_fsl.values().null_count() != 0 {
                    return Err(ArrowError::InvalidArgumentError(
                        "import: `vectors` contains null float values".into(),
                    ));
                }
            }
        }

        let text: ArrayRef = match self.text_idx {
            Some(i) => batch.column(i).clone(),
            None => new_null_array(&DataType::Utf8, n),
        };

        let ts: ArrayRef = Arc::new(TimestampMicrosecondArray::from_iter_values(
            std::iter::repeat_n(self.ts_micros, n),
        ));

        let mut columns = vec![id, vector, text, ts];
        for field in self.canonical.fields().iter().skip(4) {
            let attr = self
                .attr_indices
                .iter()
                .find(|(name, _)| name == field.name())
                .map(|(_, idx)| batch.column(*idx).clone())
                .unwrap_or_else(|| new_null_array(field.data_type(), n));
            columns.push(attr);
        }

        let out = RecordBatch::try_new(self.canonical.clone(), columns)?;
        self.rows.fetch_add(n as u64, Ordering::Relaxed);
        Ok(out)
    }
}

impl Iterator for IngestReader {
    type Item = Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next() {
            Some(Ok(batch)) => Some(self.project(&batch)),
            Some(Err(e)) => Some(Err(e)),
            None => None,
        }
    }
}

impl RecordBatchReader for IngestReader {
    fn schema(&self) -> SchemaRef {
        self.canonical.clone()
    }
}

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

    for (name, ty) in schema.fields().iter().skip(columns.len()).map(|f| {
        let ty = AttributeType::from_arrow(f.data_type()).expect("attribute scalar type");
        (f.name().clone(), ty)
    }) {
        columns.push(build_attribute_array(&rows, &name, ty)?);
    }

    RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| FirnflowError::Backend(format!("batch build: {e}")))
}

fn build_attribute_array(
    rows: &[UpsertRow],
    name: &str,
    ty: AttributeType,
) -> Result<ArrayRef, FirnflowError> {
    match ty {
        AttributeType::Int64 => {
            let mut builder = Int64Builder::with_capacity(rows.len());
            for row in rows {
                match row.attributes.get(name) {
                    None | Some(serde_json::Value::Null) => builder.append_null(),
                    Some(serde_json::Value::Number(n)) => {
                        let value = n
                            .as_i64()
                            .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()))
                            .ok_or_else(|| {
                                FirnflowError::InvalidRequest(format!(
                                    "attribute {name:?} must fit in Int64"
                                ))
                            })?;
                        builder.append_value(value);
                    }
                    Some(_) => {
                        return Err(FirnflowError::InvalidRequest(format!(
                            "attribute {name:?} must be Int64 or null"
                        )));
                    }
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        AttributeType::Float64 => {
            let mut builder = Float64Builder::with_capacity(rows.len());
            for row in rows {
                match row.attributes.get(name) {
                    None | Some(serde_json::Value::Null) => builder.append_null(),
                    Some(serde_json::Value::Number(n)) => {
                        let value = n.as_f64().ok_or_else(|| {
                            FirnflowError::InvalidRequest(format!(
                                "attribute {name:?} must be Float64"
                            ))
                        })?;
                        builder.append_value(value);
                    }
                    Some(_) => {
                        return Err(FirnflowError::InvalidRequest(format!(
                            "attribute {name:?} must be Float64 or null"
                        )));
                    }
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        AttributeType::Boolean => {
            let mut builder = BooleanBuilder::with_capacity(rows.len());
            for row in rows {
                match row.attributes.get(name) {
                    None | Some(serde_json::Value::Null) => builder.append_null(),
                    Some(serde_json::Value::Bool(v)) => builder.append_value(*v),
                    Some(_) => {
                        return Err(FirnflowError::InvalidRequest(format!(
                            "attribute {name:?} must be Boolean or null"
                        )));
                    }
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        AttributeType::Utf8 => {
            let mut builder = StringBuilder::with_capacity(rows.len(), rows.len() * 16);
            for row in rows {
                match row.attributes.get(name) {
                    None | Some(serde_json::Value::Null) => builder.append_null(),
                    Some(serde_json::Value::String(v)) => builder.append_value(v),
                    Some(_) => {
                        return Err(FirnflowError::InvalidRequest(format!(
                            "attribute {name:?} must be Utf8 or null"
                        )));
                    }
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        AttributeType::StringList => {
            // ListBuilder<StringBuilder> yields List(item: Utf8, nullable),
            // matching AttributeType::StringList::arrow(). A missing key or
            // JSON null is a null list; `[]` is an empty (non-null) list.
            let mut builder = ListBuilder::new(StringBuilder::new());
            for row in rows {
                match row.attributes.get(name) {
                    None | Some(serde_json::Value::Null) => builder.append_null(),
                    Some(serde_json::Value::Array(items)) => {
                        for item in items {
                            match item {
                                serde_json::Value::String(v) => builder.values().append_value(v),
                                serde_json::Value::Null => builder.values().append_null(),
                                _ => {
                                    return Err(FirnflowError::InvalidRequest(format!(
                                        "attribute {name:?} must be a list of strings"
                                    )));
                                }
                            }
                        }
                        builder.append(true);
                    }
                    Some(_) => {
                        return Err(FirnflowError::InvalidRequest(format!(
                            "attribute {name:?} must be a string list or null"
                        )));
                    }
                }
            }
            Ok(Arc::new(builder.finish()))
        }
    }
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
                attributes: extract_scalar_attributes(batch, row),
            });
        }
    }
    Ok(out)
}

fn batches_to_results(
    batches: &[RecordBatch],
    kind: VectorKind,
    include_vector: bool,
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

        // `_ingested_at` is absent on tables created before the
        // column existed; those rows report `None`.
        let ingested_ats = batch
            .column_by_name(INGESTED_AT_COLUMN)
            .and_then(|c| c.as_any().downcast_ref::<TimestampMicrosecondArray>());

        for row in 0..batch.num_rows() {
            // Multivector hits never carry the bag (it is hundreds of
            // KB per row); single-vector hits carry the stored vector
            // unless the caller opted out and the column was projected
            // away.
            let vector = if include_vector && kind == VectorKind::Single {
                Some(extract_row_vector(batch, row, kind, "query result")?)
            } else {
                None
            };
            let text = texts.and_then(|t| {
                if t.is_null(row) {
                    None
                } else {
                    Some(t.value(row).to_owned())
                }
            });
            let ingested_at_micros = ingested_ats.and_then(|a| {
                if a.is_null(row) {
                    None
                } else {
                    Some(a.value(row))
                }
            });
            out.push(QueryResult {
                id: ids.value(row),
                score: scores.value(row),
                vector,
                text,
                ingested_at_micros,
                attributes: extract_scalar_attributes(batch, row),
            });
        }
    }
    Ok(out)
}

fn extract_scalar_attributes(
    batch: &RecordBatch,
    row: usize,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for (idx, field) in batch.schema().fields().iter().enumerate() {
        let name = field.name();
        if RESERVED_ATTRIBUTE_COLUMNS.contains(&name.as_str()) {
            continue;
        }
        let value = match field.data_type() {
            DataType::Int64 => batch
                .column(idx)
                .as_any()
                .downcast_ref::<Int64Array>()
                .map(|a| {
                    if a.is_null(row) {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::from(a.value(row))
                    }
                }),
            DataType::Float64 => batch
                .column(idx)
                .as_any()
                .downcast_ref::<Float64Array>()
                .map(|a| {
                    if a.is_null(row) {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::from(a.value(row))
                    }
                }),
            DataType::Boolean => batch
                .column(idx)
                .as_any()
                .downcast_ref::<BooleanArray>()
                .map(|a| {
                    if a.is_null(row) {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::from(a.value(row))
                    }
                }),
            DataType::Utf8 => batch
                .column(idx)
                .as_any()
                .downcast_ref::<StringArray>()
                .map(|a| {
                    if a.is_null(row) {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::from(a.value(row))
                    }
                }),
            DataType::List(item) if *item.data_type() == DataType::Utf8 => batch
                .column(idx)
                .as_any()
                .downcast_ref::<ListArray>()
                .map(|a| {
                    if a.is_null(row) {
                        serde_json::Value::Null
                    } else {
                        let elems = a.value(row);
                        match elems.as_any().downcast_ref::<StringArray>() {
                            Some(sa) => serde_json::Value::Array(
                                (0..sa.len())
                                    .map(|i| {
                                        if sa.is_null(i) {
                                            serde_json::Value::Null
                                        } else {
                                            serde_json::Value::from(sa.value(i))
                                        }
                                    })
                                    .collect(),
                            ),
                            None => serde_json::Value::Null,
                        }
                    }
                }),
            _ => None,
        };
        if let Some(value) = value {
            out.insert(name.clone(), value);
        }
    }
    out
}

fn validate_facet_field(field: &str, info: &NamespaceSchemaInfo) -> Result<(), FirnflowError> {
    match field {
        "id" => Ok(()),
        INGESTED_AT_COLUMN if info.has_ingested_at => Ok(()),
        "vector" | "vectors" | "text" => Err(FirnflowError::InvalidRequest(format!(
            "field {field:?} is not facetable"
        ))),
        name if info.attributes.contains_key(name) => Ok(()),
        _ => Err(FirnflowError::InvalidRequest(format!(
            "unknown facet field {field:?}"
        ))),
    }
}

/// Facet contribution(s) for one row of a facet column. A `List<Utf8>`
/// column yields one value per (non-null) element, so a multi-valued row
/// (e.g. a book's genres) is counted in every bucket it belongs to; a
/// null or empty list contributes nothing. A scalar column yields exactly
/// one value (its scalar, or JSON null for a null cell).
fn facet_values_at(array: &ArrayRef, row: usize) -> Result<Vec<serde_json::Value>, FirnflowError> {
    if let Some(a) = array.as_any().downcast_ref::<ListArray>() {
        if a.is_null(row) {
            return Ok(Vec::new());
        }
        let elems = a.value(row);
        let strs = elems.as_any().downcast_ref::<StringArray>().ok_or_else(|| {
            FirnflowError::InvalidRequest("facet list field must be a list of strings".into())
        })?;
        let mut out = Vec::with_capacity(strs.len());
        for i in 0..strs.len() {
            if !strs.is_null(i) {
                out.push(serde_json::Value::from(strs.value(i)));
            }
        }
        return Ok(out);
    }
    Ok(vec![scalar_value_at(array, row)?])
}

fn scalar_value_at(array: &ArrayRef, row: usize) -> Result<serde_json::Value, FirnflowError> {
    if array.is_null(row) {
        return Ok(serde_json::Value::Null);
    }
    if let Some(a) = array.as_any().downcast_ref::<UInt64Array>() {
        return Ok(serde_json::Value::from(a.value(row)));
    }
    if let Some(a) = array.as_any().downcast_ref::<Int64Array>() {
        return Ok(serde_json::Value::from(a.value(row)));
    }
    if let Some(a) = array.as_any().downcast_ref::<Float64Array>() {
        return Ok(serde_json::Value::from(a.value(row)));
    }
    if let Some(a) = array.as_any().downcast_ref::<BooleanArray>() {
        return Ok(serde_json::Value::from(a.value(row)));
    }
    if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
        return Ok(serde_json::Value::from(a.value(row)));
    }
    if let Some(a) = array.as_any().downcast_ref::<TimestampMicrosecondArray>() {
        return Ok(serde_json::Value::from(a.value(row)));
    }
    Err(FirnflowError::InvalidRequest(
        "facet fields must be scalar columns".into(),
    ))
}

fn facet_value_sort_key(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "0:".to_string(),
        serde_json::Value::Bool(v) => format!("1:{v}"),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                format!("2:{i:020}")
            } else if let Some(u) = n.as_u64() {
                format!("2:{u:020}")
            } else {
                format!("2:{:020.12}", n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => format!("3:{s}"),
        _ => "4:".to_string(),
    }
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
    fn scalar_index_column_validation() {
        // Both supported columns pass.
        assert!(validate_scalar_index_column("id").is_ok());
        assert!(validate_scalar_index_column(INGESTED_AT_COLUMN).is_ok());
        // Anything else is a 400-mapped InvalidRequest.
        assert!(matches!(
            validate_scalar_index_column("vector"),
            Err(FirnflowError::InvalidRequest(_))
        ));
        assert!(matches!(
            validate_scalar_index_column("text"),
            Err(FirnflowError::InvalidRequest(_))
        ));
    }

    #[test]
    fn import_schema_validation() {
        let single_vec =
            NamespaceManager::schema_for_kind(VectorKind::Single, 4, false, &BTreeMap::new())
                .field(1)
                .data_type()
                .clone();
        let multi_vec =
            NamespaceManager::schema_for_kind(VectorKind::Multivector, 4, false, &BTreeMap::new())
                .field(1)
                .data_type()
                .clone();

        // Happy: single-vector, no text.
        let s = Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("vector", single_vec.clone(), false),
        ]);
        assert_eq!(
            validate_arrow_import_schema(&s).unwrap(),
            (VectorKind::Single, 4, false, BTreeMap::new())
        );

        // Happy: multivector with text.
        let s = Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("vectors", multi_vec.clone(), false),
            Field::new("text", DataType::Utf8, true),
        ]);
        assert_eq!(
            validate_arrow_import_schema(&s).unwrap(),
            (VectorKind::Multivector, 4, true, BTreeMap::new())
        );

        let s = Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("vector", single_vec.clone(), false),
            Field::new("section", DataType::Utf8, true),
            Field::new("priority", DataType::Int64, true),
        ]);
        let (_, _, _, attrs) = validate_arrow_import_schema(&s).unwrap();
        assert_eq!(attrs.get("section"), Some(&AttributeType::Utf8));
        assert_eq!(attrs.get("priority"), Some(&AttributeType::Int64));

        // Each of these must be rejected as InvalidRequest.
        let bad_float =
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 4);
        let rejected = [
            // missing id
            Schema::new(vec![Field::new("vector", single_vec.clone(), false)]),
            // wrong id type
            Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("vector", single_vec.clone(), false),
            ]),
            // server-owned _ingested_at supplied
            Schema::new(vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("vector", single_vec.clone(), false),
                Field::new(
                    INGESTED_AT_COLUMN,
                    DataType::Timestamp(TimeUnit::Microsecond, None),
                    false,
                ),
            ]),
            // neither vector nor vectors
            Schema::new(vec![Field::new("id", DataType::UInt64, false)]),
            // both vector and vectors
            Schema::new(vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("vector", single_vec.clone(), false),
                Field::new("vectors", multi_vec.clone(), false),
            ]),
            // wrong element type (Float64)
            Schema::new(vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("vector", bad_float, false),
            ]),
            // text wrong type
            Schema::new(vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("vector", single_vec.clone(), false),
                Field::new("text", DataType::Int32, true),
            ]),
            // unsupported attribute type
            Schema::new(vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("vector", single_vec.clone(), false),
                Field::new("bad", single_vec.clone(), false),
            ]),
            // reserved attribute name
            Schema::new(vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("vector", single_vec.clone(), false),
                Field::new("_bad", DataType::Utf8, true),
            ]),
            // duplicate column name (Arrow allows it; we reject it so the
            // second does not silently shadow the first)
            Schema::new(vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("vector", single_vec.clone(), false),
                Field::new("vector", single_vec.clone(), false),
            ]),
        ];
        for (i, s) in rejected.iter().enumerate() {
            assert!(
                matches!(
                    validate_arrow_import_schema(s),
                    Err(FirnflowError::InvalidRequest(_))
                ),
                "schema #{i} should have been rejected"
            );
        }
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
