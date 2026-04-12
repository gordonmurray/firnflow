//! Namespace manager — the production shim over lancedb.
//!
//! Each namespace maps to its own Lance table rooted at
//! `s3://{bucket}/{namespace}/`. The manager is stateless with
//! respect to connections: every public call opens a fresh
//! `lancedb::Connection`. Connection caching is a later
//! optimisation.
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

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::builder::{FixedSizeListBuilder, Float32Builder};
use arrow_array::{
    Array, ArrayRef, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};
use dashmap::DashMap;
use futures::{StreamExt, TryStreamExt};
use lancedb::query::{ExecutableQuery, QueryBase};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectStorePath;
use object_store::ObjectStore;

use crate::{FirnflowError, NamespaceId, QueryResult, QueryResultSet};

const TABLE_NAME: &str = "data";
const DISTANCE_COLUMN: &str = "_distance";

/// Stateless namespace manager over an S3-backed set of Lance tables.
///
/// Each namespace independently determines its own vector dimension
/// — either inferred from the first upsert or read from the
/// existing Lance table schema. A single manager instance can serve
/// namespaces with different dimensions simultaneously.
pub struct NamespaceManager {
    bucket: String,
    storage_options: HashMap<String, String>,
    /// Per-namespace resolved dimensions. Populated on first
    /// interaction with each namespace (upsert or query).
    dims: DashMap<NamespaceId, usize>,
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
    pub fn new(bucket: impl Into<String>, storage_options: HashMap<String, String>) -> Self {
        Self {
            bucket: bucket.into(),
            storage_options,
            dims: DashMap::new(),
        }
    }

    /// Resolved vector dimension for a namespace, if known. Returns
    /// `None` for namespaces the manager has not yet interacted
    /// with.
    pub fn dim_for(&self, ns: &NamespaceId) -> Option<usize> {
        self.dims.get(ns).map(|r| *r)
    }

    fn uri(&self, ns: &NamespaceId) -> String {
        format!("s3://{}/{}", self.bucket, ns.as_str())
    }

    fn schema_for_dim(dim: usize) -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    dim as i32,
                ),
                false,
            ),
        ]))
    }

    async fn connect(&self, ns: &NamespaceId) -> Result<lancedb::Connection, FirnflowError> {
        lancedb::connect(&self.uri(ns))
            .storage_options(self.storage_options.clone())
            .execute()
            .await
            .map_err(|e| FirnflowError::Backend(format!("lancedb connect: {e}")))
    }

    /// Try to open the existing table and read its vector dimension
    /// from the schema. Returns `Ok(Some((table, dim)))` if the
    /// table exists, `Ok(None)` if it does not.
    async fn open_existing(
        &self,
        ns: &NamespaceId,
    ) -> Result<Option<(lancedb::Table, usize)>, FirnflowError> {
        let conn = self.connect(ns).await?;
        match conn.open_table(TABLE_NAME).execute().await {
            Ok(tbl) => {
                let dim = read_dim_from_table(&tbl).await?;
                Ok(Some((tbl, dim)))
            }
            Err(_) => Ok(None),
        }
    }

    /// Open the namespace's data table, creating it with an empty
    /// batch if it does not yet exist. The dimension must be known
    /// at creation time (from the first upsert).
    async fn open_or_create_table(
        &self,
        ns: &NamespaceId,
        dim: usize,
    ) -> Result<lancedb::Table, FirnflowError> {
        let conn = self.connect(ns).await?;
        match conn.open_table(TABLE_NAME).execute().await {
            Ok(tbl) => Ok(tbl),
            Err(_) => {
                let schema = Self::schema_for_dim(dim);
                let empty = rows_to_batch(&schema, dim, Vec::new())?;
                let reader: Box<dyn RecordBatchReader + Send> =
                    Box::new(RecordBatchIterator::new(vec![Ok(empty)], schema));
                conn.create_table(TABLE_NAME, reader)
                    .execute()
                    .await
                    .map_err(|e| FirnflowError::Backend(format!("create_table: {e}")))
            }
        }
    }

    /// Resolve the dimension for a namespace:
    /// 1. Check the in-memory cache.
    /// 2. Try reading from the existing Lance table schema.
    /// 3. Return `None` if the namespace doesn't exist yet.
    async fn resolve_dim(&self, ns: &NamespaceId) -> Result<Option<usize>, FirnflowError> {
        if let Some(dim) = self.dims.get(ns) {
            return Ok(Some(*dim));
        }
        // Not cached — try opening the table.
        if let Some((_tbl, dim)) = self.open_existing(ns).await? {
            self.dims.insert(ns.clone(), dim);
            return Ok(Some(dim));
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
        rows: Vec<(u64, Vec<f32>)>,
    ) -> Result<(), FirnflowError> {
        if rows.is_empty() {
            return Ok(());
        }

        // Determine the dimension: cached → schema-read → infer from row[0].
        let dim = match self.resolve_dim(ns).await? {
            Some(d) => d,
            None => {
                let d = rows[0].1.len();
                if d == 0 {
                    return Err(FirnflowError::InvalidRequest(
                        "row id 0: vector is empty".into(),
                    ));
                }
                d
            }
        };

        // Validate every row.
        for (id, v) in &rows {
            if v.len() != dim {
                return Err(FirnflowError::InvalidRequest(format!(
                    "row id {id}: vector length {}, expected {dim}",
                    v.len(),
                )));
            }
        }

        let tbl = self.open_or_create_table(ns, dim).await?;
        let schema = Self::schema_for_dim(dim);
        let batch = rows_to_batch(&schema, dim, rows)?;
        let reader: Box<dyn RecordBatchReader + Send> =
            Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema));
        tbl.add(reader)
            .execute()
            .await
            .map_err(|e| FirnflowError::Backend(format!("table.add: {e}")))?;

        // Cache the dimension now that the write succeeded.
        self.dims.insert(ns.clone(), dim);
        Ok(())
    }

    /// Remove every object under a namespace prefix from the
    /// underlying object store.
    ///
    /// Also evicts the cached dimension for this namespace so that
    /// a subsequent upsert can establish a new dimension.
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

        // Evict the dimension cache so a fresh upsert can pick a new dim.
        self.dims.remove(ns);

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

    /// Run a nearest-neighbor vector query. Returns up to `k`
    /// results ranked by the underlying engine's distance metric.
    ///
    /// The query vector's length must match the namespace's
    /// established dimension. If the namespace does not exist yet
    /// (no prior upsert), the query returns an empty result set
    /// rather than an error — same behaviour as querying an empty
    /// table.
    pub async fn query(
        &self,
        ns: &NamespaceId,
        vector: Vec<f32>,
        k: usize,
    ) -> Result<QueryResultSet, FirnflowError> {
        let dim = match self.resolve_dim(ns).await? {
            Some(d) => d,
            None => {
                // Namespace doesn't exist yet — return empty.
                return Ok(QueryResultSet {
                    query_id: String::new(),
                    results: Vec::new(),
                });
            }
        };

        if vector.len() != dim {
            return Err(FirnflowError::InvalidRequest(format!(
                "query vector length {}, expected {dim}",
                vector.len(),
            )));
        }

        let tbl = self.open_or_create_table(ns, dim).await?;
        let stream = tbl
            .query()
            .nearest_to(vector)
            .map_err(|e| FirnflowError::Backend(format!("query.nearest_to: {e}")))?
            .limit(k)
            .execute()
            .await
            .map_err(|e| FirnflowError::Backend(format!("query.execute: {e}")))?;

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
}

/// Read the vector dimension from a Lance table's schema by
/// inspecting the `FixedSizeList.list_size` of the `vector` column.
async fn read_dim_from_table(tbl: &lancedb::Table) -> Result<usize, FirnflowError> {
    let schema = tbl
        .schema()
        .await
        .map_err(|e| FirnflowError::Backend(format!("read schema: {e}")))?;
    for field in schema.fields() {
        if field.name() == "vector" {
            if let DataType::FixedSizeList(_, size) = field.data_type() {
                return Ok(*size as usize);
            }
        }
    }
    Err(FirnflowError::Backend(
        "table schema has no FixedSizeList 'vector' column".into(),
    ))
}

fn rows_to_batch(
    schema: &Arc<Schema>,
    dim: usize,
    rows: Vec<(u64, Vec<f32>)>,
) -> Result<RecordBatch, FirnflowError> {
    let n = rows.len();
    let ids = UInt64Array::from_iter_values(rows.iter().map(|(id, _)| *id));

    let values_builder = Float32Builder::with_capacity(n * dim);
    let mut list_builder = FixedSizeListBuilder::new(values_builder, dim as i32);
    for (_, vector) in &rows {
        for &v in vector {
            list_builder.values().append_value(v);
        }
        list_builder.append(true);
    }
    let vectors = list_builder.finish();

    RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(ids) as ArrayRef, Arc::new(vectors) as ArrayRef],
    )
    .map_err(|e| FirnflowError::Backend(format!("batch build: {e}")))
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
        let distances = batch
            .column_by_name(DISTANCE_COLUMN)
            .ok_or_else(|| {
                FirnflowError::Backend(format!("query result: missing {DISTANCE_COLUMN} column"))
            })?
            .as_any()
            .downcast_ref::<Float32Array>()
            .ok_or_else(|| {
                FirnflowError::Backend(format!("query result: {DISTANCE_COLUMN} not Float32"))
            })?;

        for row in 0..batch.num_rows() {
            let vector_arr = vectors.value(row);
            let vec_f32 = vector_arr
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| {
                    FirnflowError::Backend("query result: vector inner not Float32".into())
                })?;
            let vector: Vec<f32> = (0..vec_f32.len()).map(|i| vec_f32.value(i)).collect();
            out.push(QueryResult {
                id: ids.value(row),
                score: distances.value(row),
                vector,
            });
        }
    }
    Ok(out)
}
