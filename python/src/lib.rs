//! PyO3 bindings exposing the firnflow engine as the `firn` Python
//! package (the native `firn._native` module).
//!
//! `connect()` builds a [`NamespaceService`] over a local-filesystem or
//! object-storage root with its own foyer result cache, and the
//! `Client` / `Collection` data-plane methods map onto it. Every
//! blocking engine call releases the GIL so other Python threads make
//! progress.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use firnflow_core::cache::NamespaceCache;
use firnflow_core::{
    CoreMetrics, FirnflowError, NamespaceId, NamespaceManager, NamespaceService, QueryRequest,
    QueryResult, StorageRoot, UpsertRow,
};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use tokio::runtime::Runtime;

/// Embedded result-cache sizes (RAM tier, NVMe tier). Modest defaults
/// for a single-process user; the server runs larger.
const CACHE_MEMORY_BYTES: usize = 16 * 1024 * 1024;
const CACHE_NVME_BYTES: usize = 64 * 1024 * 1024;

/// Collection used when a caller does not name one explicitly.
const DEFAULT_COLLECTION: &str = "default";

/// Process-global multi-threaded tokio runtime shared by all clients.
///
/// One runtime per process (not per `Client`) keeps thread counts
/// bounded; Lance issues concurrent object-store requests, so a
/// multi-thread scheduler is required rather than a current-thread one.
fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build the firn tokio runtime")
    })
}

create_exception!(
    _native,
    FirnError,
    PyException,
    "Base class for all firn errors."
);
create_exception!(
    _native,
    StorageError,
    FirnError,
    "Object store, Lance, cache, or disk failure."
);
create_exception!(
    _native,
    TenantError,
    FirnError,
    "Invalid tenant or collection name."
);
create_exception!(
    _native,
    ValidationError,
    FirnError,
    "Invalid request payload or arguments."
);
create_exception!(
    _native,
    UnsupportedError,
    FirnError,
    "Feature not available in this build."
);

/// Map an engine error onto the Python exception hierarchy. The
/// original engine message is preserved so the Python traceback is
/// actionable.
fn to_py_err(err: FirnflowError) -> PyErr {
    let msg = err.to_string();
    match err {
        FirnflowError::Backend(_) | FirnflowError::Io(_) | FirnflowError::Cache(_) => {
            StorageError::new_err(msg)
        }
        FirnflowError::InvalidNamespace(_) => TenantError::new_err(msg),
        FirnflowError::InvalidRequest(_) => ValidationError::new_err(msg),
        FirnflowError::Unsupported(_) => UnsupportedError::new_err(msg),
        FirnflowError::Metrics(_) => FirnError::new_err(msg),
    }
}

/// A single search hit, built from the engine's `QueryResult`.
#[pyclass(frozen, get_all)]
#[derive(Clone)]
struct Hit {
    /// Stable row id.
    id: u64,
    /// Similarity score (cosine / L2 / BM25 / hybrid, per query type).
    score: f32,
    /// Stored text, if the row carried any.
    text: Option<String>,
    /// Stored vector — `None` unless `search(..., include_vectors=True)`.
    vector: Option<Vec<f32>>,
    /// Server-side write timestamp (microseconds since the Unix epoch).
    ingested_at_micros: Option<i64>,
}

impl From<QueryResult> for Hit {
    fn from(r: QueryResult) -> Self {
        Hit {
            id: r.id,
            score: r.score,
            text: r.text,
            vector: r.vector,
            ingested_at_micros: r.ingested_at_micros,
        }
    }
}

#[pymethods]
impl Hit {
    fn __repr__(&self) -> String {
        format!("Hit(id={}, score={:.4})", self.id, self.score)
    }
}

/// Shared per-namespace bookkeeping held by a `Client` and every
/// `Collection` it hands out, so they agree on which full-text indexes
/// are current. Keyed by the resolved `NamespaceId` string.
type FtsState = Arc<Mutex<HashSet<String>>>;

/// Lifecycle gate shared by a `Client` and the `Collection`s it hands
/// out. A data-plane call holds a read guard for its whole duration;
/// `close()` takes the write guard, so it waits for in-flight operations
/// to finish before shutting the cache down, and marks the client closed
/// only once the cache actually closed (a failed close stays retryable).
struct Lifecycle {
    closed: AtomicBool,
    gate: RwLock<()>,
}

impl Lifecycle {
    fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            gate: RwLock::new(()),
        }
    }

    /// Cheap, non-blocking check so a closed client rejects use before
    /// any other validation. The authoritative, race-safe check is in
    /// [`run`](Self::run).
    fn check_open(&self) -> PyResult<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(closed_py_err())
        } else {
            Ok(())
        }
    }

    /// Run a blocking operation under a read guard. **The caller must
    /// have released the GIL** — the gate is acquired here, so a thread
    /// blocked waiting for it never holds the GIL (which would deadlock
    /// against another thread needing the GIL to drop its guard).
    /// Returns `None` if the client is closed; the read guard guarantees
    /// `close()` is not running concurrently.
    fn run<T>(
        &self,
        f: impl FnOnce() -> Result<T, FirnflowError>,
    ) -> Result<Option<T>, FirnflowError> {
        let _r = self.gate.read().unwrap_or_else(|p| p.into_inner());
        if self.closed.load(Ordering::Acquire) {
            return Ok(None);
        }
        f().map(Some)
    }
}

/// The base `FirnError` raised on use-after-close.
fn closed_py_err() -> PyErr {
    FirnError::new_err("client is closed; open a new one with firn.connect()")
}

/// Validate one part of a namespace name (a collection or tenant) under
/// the rules that keep the `--` tenant separator unambiguous: non-empty,
/// `[a-z0-9-]`, no leading/trailing hyphen, no consecutive hyphens.
fn validate_component(value: &str, what: &str) -> PyResult<()> {
    if value.is_empty() {
        return Err(TenantError::new_err(format!("{what} must not be empty")));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(TenantError::new_err(format!(
            "{what} {value:?} must contain only lowercase letters, digits, and hyphens"
        )));
    }
    if value.starts_with('-') || value.ends_with('-') {
        return Err(TenantError::new_err(format!(
            "{what} {value:?} must not start or end with a hyphen"
        )));
    }
    if value.contains("--") {
        return Err(TenantError::new_err(format!(
            "{what} {value:?} must not contain consecutive hyphens (\"--\" is reserved as the tenant separator)"
        )));
    }
    Ok(())
}

/// Resolve a `(collection, tenant)` pair to a physical namespace.
///
/// With no tenant the namespace is the collection name; with a tenant it
/// is `"{collection}--{tenant}"`. Because no component may contain `--`,
/// the join is unambiguous and two distinct pairs can never collide on
/// one namespace.
fn compose_namespace(collection: &str, tenant: Option<&str>) -> PyResult<NamespaceId> {
    validate_component(collection, "collection")?;
    let name = match tenant {
        None => collection.to_string(),
        Some(t) => {
            validate_component(t, "tenant")?;
            format!("{collection}--{t}")
        }
    };
    NamespaceId::new(name).map_err(to_py_err)
}

/// Convert a Python `list[dict]` into engine upsert rows under the GIL.
fn parse_documents(documents: &Bound<'_, PyList>) -> PyResult<Vec<UpsertRow>> {
    let mut rows = Vec::with_capacity(documents.len());
    for item in documents.iter() {
        let dict = item
            .downcast::<PyDict>()
            .map_err(|_| ValidationError::new_err("each document must be a dict"))?;
        let id: u64 = dict
            .get_item("id")?
            .ok_or_else(|| {
                ValidationError::new_err("document is missing the required integer 'id'")
            })?
            .extract()
            .map_err(|_| {
                ValidationError::new_err("document 'id' must be a non-negative integer")
            })?;
        let text: Option<String> = match dict.get_item("text")? {
            Some(v) => Some(
                v.extract()
                    .map_err(|_| ValidationError::new_err("document 'text' must be a string"))?,
            ),
            None => None,
        };
        let vector: Vec<f32> = match dict.get_item("vector")? {
            Some(v) => v.extract().map_err(|_| {
                ValidationError::new_err("document 'vector' must be a list of numbers")
            })?,
            None => Vec::new(),
        };
        let vectors: Option<Vec<Vec<f32>>> = match dict.get_item("vectors")? {
            Some(v) => {
                let parsed: Vec<Vec<f32>> = v.extract().map_err(|_| {
                    ValidationError::new_err(
                        "document 'vectors' must be a list of lists of numbers",
                    )
                })?;
                (!parsed.is_empty()).then_some(parsed)
            }
            None => None,
        };
        // Each row needs exactly one vector payload: a single `vector`
        // or a multivector `vectors` bag, never both and never neither.
        match (vector.is_empty(), vectors.is_some()) {
            (false, true) => {
                return Err(ValidationError::new_err(
                    "document must set exactly one of 'vector' or 'vectors', not both",
                ))
            }
            (true, false) => {
                return Err(ValidationError::new_err(
                    "document must include a 'vector' (list[float]) or 'vectors' \
                     (list[list[float]]); firn stores vectors with optional 'text'",
                ))
            }
            _ => {}
        }
        rows.push(UpsertRow {
            id,
            vector,
            vectors,
            text,
        });
    }
    Ok(rows)
}

/// Ensure a BM25 full-text index exists for a namespace before a text
/// or hybrid query. Built lazily and rebuilt after writes (tracked in
/// `fts`); a namespace with no rows yet is a no-op, so the query simply
/// returns no hits.
fn ensure_fts_built(
    service: &Arc<NamespaceService>,
    fts: &FtsState,
    ns: &NamespaceId,
) -> Result<(), FirnflowError> {
    if fts.lock().unwrap().contains(ns.as_str()) {
        return Ok(());
    }
    match runtime().block_on(service.create_fts_index(ns)) {
        Ok(()) => {
            fts.lock().unwrap().insert(ns.as_str().to_string());
            Ok(())
        }
        // No rows upserted yet: nothing to index. The query returns no
        // hits. Any other failure is a real error.
        Err(FirnflowError::InvalidRequest(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Insert or update documents in a namespace (merge-insert by `id`).
fn op_upsert(
    service: &Arc<NamespaceService>,
    fts: &FtsState,
    lifecycle: &Arc<Lifecycle>,
    py: Python<'_>,
    ns: &NamespaceId,
    documents: &Bound<'_, PyList>,
) -> PyResult<usize> {
    let rows = parse_documents(documents)?;
    let n = rows.len();
    let service = service.clone();
    let lifecycle = lifecycle.clone();
    let ns_owned = ns.clone();
    let outcome = py
        .allow_threads(move || {
            lifecycle.run(|| runtime().block_on(service.upsert(&ns_owned, rows)))
        })
        .map_err(to_py_err)?;
    if outcome.is_none() {
        return Err(closed_py_err());
    }
    // New rows make any existing full-text index stale.
    fts.lock().unwrap().remove(ns.as_str());
    Ok(n)
}

/// Delete a whole namespace (all rows). Row-level delete by id is not
/// supported by the engine yet.
fn op_delete(
    service: &Arc<NamespaceService>,
    fts: &FtsState,
    lifecycle: &Arc<Lifecycle>,
    py: Python<'_>,
    ns: &NamespaceId,
) -> PyResult<usize> {
    let service = service.clone();
    let lifecycle = lifecycle.clone();
    let ns_owned = ns.clone();
    let count = py
        .allow_threads(move || lifecycle.run(|| runtime().block_on(service.delete(&ns_owned))))
        .map_err(to_py_err)?;
    match count {
        None => Err(closed_py_err()),
        Some(c) => {
            fts.lock().unwrap().remove(ns.as_str());
            Ok(c)
        }
    }
}

/// Run a vector, full-text, or hybrid search against a namespace.
#[allow(clippy::too_many_arguments)]
fn op_search(
    service: &Arc<NamespaceService>,
    fts: &FtsState,
    lifecycle: &Arc<Lifecycle>,
    py: Python<'_>,
    ns: &NamespaceId,
    query: Option<String>,
    vector: Option<Vec<f32>>,
    vectors: Option<Vec<Vec<f32>>>,
    hybrid: bool,
    limit: usize,
    include_vectors: bool,
) -> PyResult<Vec<Hit>> {
    // An empty list is "no payload", not an empty vector — otherwise a
    // `vector=[]` would slip past the hybrid guard and silently degrade
    // to FTS-only.
    let vector = vector.filter(|v| !v.is_empty());
    let vectors = vectors.filter(|v| !v.is_empty());
    let has_vector = vector.is_some() || vectors.is_some();
    if query.is_none() && !has_vector {
        return Err(ValidationError::new_err(
            "search needs a text query, a vector, or vectors",
        ));
    }
    if vector.is_some() && vectors.is_some() {
        return Err(ValidationError::new_err(
            "set exactly one of 'vector' or 'vectors', not both",
        ));
    }
    if hybrid && !has_vector {
        return Err(ValidationError::new_err(
            "hybrid=True needs a vector to fuse with the text query",
        ));
    }
    let needs_fts = query.is_some();
    let req = QueryRequest {
        vector: vector.unwrap_or_default(),
        vectors,
        k: limit,
        nprobes: None,
        text: query,
        include_vector: include_vectors,
        semantic_cache: None,
    };
    let service = service.clone();
    let fts = fts.clone();
    let lifecycle = lifecycle.clone();
    let ns_owned = ns.clone();
    let result = py
        .allow_threads(move || {
            lifecycle.run(|| {
                // Text and hybrid queries need a BM25 index; build it
                // lazily. Vector-only search needs no index.
                if needs_fts {
                    ensure_fts_built(&service, &fts, &ns_owned)?;
                }
                runtime().block_on(service.query(&ns_owned, &req))
            })
        })
        .map_err(to_py_err)?;
    match result {
        None => Err(closed_py_err()),
        Some(rs) => Ok(rs.results.into_iter().map(Hit::from).collect()),
    }
}

/// A handle to one named collection. Search and write methods take an
/// optional `tenant=` that selects a physically separate namespace.
#[pyclass]
struct Collection {
    service: Arc<NamespaceService>,
    collection: String,
    fts: FtsState,
    lifecycle: Arc<Lifecycle>,
}

#[pymethods]
impl Collection {
    /// Insert or update documents. See `Client.add`.
    #[pyo3(signature = (documents, *, tenant=None))]
    fn add(
        &self,
        py: Python<'_>,
        documents: &Bound<'_, PyList>,
        tenant: Option<String>,
    ) -> PyResult<usize> {
        self.lifecycle.check_open()?;
        let ns = compose_namespace(&self.collection, tenant.as_deref())?;
        op_upsert(
            &self.service,
            &self.fts,
            &self.lifecycle,
            py,
            &ns,
            documents,
        )
    }

    /// Alias for `add` (the engine write is an upsert by `id`).
    #[pyo3(signature = (documents, *, tenant=None))]
    fn upsert(
        &self,
        py: Python<'_>,
        documents: &Bound<'_, PyList>,
        tenant: Option<String>,
    ) -> PyResult<usize> {
        self.add(py, documents, tenant)
    }

    /// Delete the whole collection (optionally for one tenant). Passing
    /// `ids` raises `UnsupportedError` — row-level delete is not yet
    /// available.
    #[pyo3(signature = (ids=None, *, tenant=None))]
    fn delete(
        &self,
        py: Python<'_>,
        ids: Option<Vec<u64>>,
        tenant: Option<String>,
    ) -> PyResult<usize> {
        self.lifecycle.check_open()?;
        if ids.is_some() {
            return Err(UnsupportedError::new_err(
                "row-level delete by id is not supported yet; call delete() with no ids to drop the whole collection",
            ));
        }
        let ns = compose_namespace(&self.collection, tenant.as_deref())?;
        op_delete(&self.service, &self.fts, &self.lifecycle, py, &ns)
    }

    /// Search this collection. See `Client.search`.
    #[pyo3(signature = (query=None, *, vector=None, vectors=None, hybrid=false, limit=10, tenant=None, include_vectors=false))]
    #[allow(clippy::too_many_arguments)]
    fn search(
        &self,
        py: Python<'_>,
        query: Option<String>,
        vector: Option<Vec<f32>>,
        vectors: Option<Vec<Vec<f32>>>,
        hybrid: bool,
        limit: usize,
        tenant: Option<String>,
        include_vectors: bool,
    ) -> PyResult<Vec<Hit>> {
        self.lifecycle.check_open()?;
        let ns = compose_namespace(&self.collection, tenant.as_deref())?;
        op_search(
            &self.service,
            &self.fts,
            &self.lifecycle,
            py,
            &ns,
            query,
            vector,
            vectors,
            hybrid,
            limit,
            include_vectors,
        )
    }
}

/// An embedded firn client over a local or object-storage root. Proxies
/// the data plane to a default collection and hands out named ones.
#[pyclass]
struct Client {
    service: Arc<NamespaceService>,
    cache: Arc<NamespaceCache>,
    fts: FtsState,
    lifecycle: Arc<Lifecycle>,
}

#[pymethods]
impl Client {
    /// Return a handle to a named collection.
    fn collection(&self, name: String) -> PyResult<Collection> {
        self.lifecycle.check_open()?;
        validate_component(&name, "collection")?;
        Ok(Collection {
            service: self.service.clone(),
            collection: name,
            fts: self.fts.clone(),
            lifecycle: self.lifecycle.clone(),
        })
    }

    /// Insert or update documents in the default collection.
    ///
    /// Each document is a dict with an integer `id`, a `vector`
    /// (list[float]), and optional `text` (str). `tenant=` writes to a
    /// physically separate namespace. Returns the number of rows written.
    #[pyo3(signature = (documents, *, tenant=None))]
    fn add(
        &self,
        py: Python<'_>,
        documents: &Bound<'_, PyList>,
        tenant: Option<String>,
    ) -> PyResult<usize> {
        self.lifecycle.check_open()?;
        let ns = compose_namespace(DEFAULT_COLLECTION, tenant.as_deref())?;
        op_upsert(
            &self.service,
            &self.fts,
            &self.lifecycle,
            py,
            &ns,
            documents,
        )
    }

    /// Alias for `add` (the engine write is an upsert by `id`).
    #[pyo3(signature = (documents, *, tenant=None))]
    fn upsert(
        &self,
        py: Python<'_>,
        documents: &Bound<'_, PyList>,
        tenant: Option<String>,
    ) -> PyResult<usize> {
        self.add(py, documents, tenant)
    }

    /// Delete the default collection (optionally for one tenant).
    /// Passing `ids` raises `UnsupportedError`.
    #[pyo3(signature = (ids=None, *, tenant=None))]
    fn delete(
        &self,
        py: Python<'_>,
        ids: Option<Vec<u64>>,
        tenant: Option<String>,
    ) -> PyResult<usize> {
        self.lifecycle.check_open()?;
        if ids.is_some() {
            return Err(UnsupportedError::new_err(
                "row-level delete by id is not supported yet; call delete() with no ids to drop the whole collection",
            ));
        }
        let ns = compose_namespace(DEFAULT_COLLECTION, tenant.as_deref())?;
        op_delete(&self.service, &self.fts, &self.lifecycle, py, &ns)
    }

    /// Search the default collection.
    ///
    /// `query` runs BM25 full-text search; `vector` runs
    /// nearest-neighbour search; supplying both (or `hybrid=True`) runs
    /// hybrid (RRF) search. `tenant=` scopes to one tenant's namespace.
    /// Vectors are not echoed back on hits unless `include_vectors=True`.
    #[pyo3(signature = (query=None, *, vector=None, vectors=None, hybrid=false, limit=10, tenant=None, include_vectors=false))]
    #[allow(clippy::too_many_arguments)]
    fn search(
        &self,
        py: Python<'_>,
        query: Option<String>,
        vector: Option<Vec<f32>>,
        vectors: Option<Vec<Vec<f32>>>,
        hybrid: bool,
        limit: usize,
        tenant: Option<String>,
        include_vectors: bool,
    ) -> PyResult<Vec<Hit>> {
        self.lifecycle.check_open()?;
        let ns = compose_namespace(DEFAULT_COLLECTION, tenant.as_deref())?;
        op_search(
            &self.service,
            &self.fts,
            &self.lifecycle,
            py,
            &ns,
            query,
            vector,
            vectors,
            hybrid,
            limit,
            include_vectors,
        )
    }

    /// Flush the result cache's NVMe write buffer and release it. After
    /// this the client and any collections from it reject use. Safe to
    /// call more than once.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let lifecycle = self.lifecycle.clone();
        let cache = self.cache.clone();
        py.allow_threads(move || -> Result<(), FirnflowError> {
            // Wait for in-flight operations (the write guard), then close
            // the cache, and mark closed only on success so a failed
            // close stays retryable. A second close is a no-op.
            let _w = lifecycle.gate.write().unwrap_or_else(|p| p.into_inner());
            if lifecycle.closed.load(Ordering::Acquire) {
                return Ok(());
            }
            runtime().block_on(cache.close())?;
            lifecycle.closed.store(true, Ordering::Release);
            Ok(())
        })
        .map_err(to_py_err)
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc_value=None, _traceback=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<Bound<'_, PyAny>>,
        _exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }
}

/// Platform cache directory for the foyer NVMe tier (Linux:
/// `$XDG_CACHE_HOME` or `~/.cache`; macOS: `~/Library/Caches`). Kept out
/// of the data directory so the cache is never committed alongside data.
fn platform_cache_base() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x));
        }
    }
    let home = std::env::var("HOME").ok()?;
    if cfg!(target_os = "macos") {
        Some(PathBuf::from(home).join("Library").join("Caches"))
    } else {
        Some(PathBuf::from(home).join(".cache"))
    }
}

/// Cache directory for a storage root, keyed by the root so two
/// different datasets do not share a cache file. Created if absent.
fn embedded_cache_dir(root: &StorageRoot) -> PyResult<PathBuf> {
    let base = platform_cache_base().ok_or_else(|| {
        StorageError::new_err(
            "could not resolve a platform cache directory (set XDG_CACHE_HOME or HOME)",
        )
    })?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&root.as_uri(), &mut hasher);
    let dir = base
        .join("firn")
        .join(format!("{:016x}", std::hash::Hasher::finish(&hasher)));
    std::fs::create_dir_all(&dir)
        .map_err(|e| StorageError::new_err(format!("create cache dir {}: {e}", dir.display())))?;
    Ok(dir)
}

/// Build the S3-family storage options for an object-storage root from
/// the explicit credential keyword arguments. Empty when none are
/// given, in which case the underlying client reads the standard
/// `AWS_*` environment.
fn object_storage_options(
    access_key: Option<String>,
    secret_key: Option<String>,
    endpoint: Option<String>,
    region: Option<String>,
) -> HashMap<String, String> {
    let mut opts = HashMap::new();
    if let Some(k) = access_key {
        opts.insert("aws_access_key_id".to_string(), k);
    }
    if let Some(s) = secret_key {
        opts.insert("aws_secret_access_key".to_string(), s);
    }
    if let Some(r) = region {
        opts.insert("aws_region".to_string(), r);
    }
    if let Some(e) = endpoint {
        // A custom endpoint (Tigris, MinIO, R2, Spaces) needs path-style
        // addressing; allow plain HTTP only for an `http://` endpoint.
        let allow_http = e.starts_with("http://");
        opts.insert("aws_endpoint".to_string(), e);
        opts.insert(
            "aws_virtual_hosted_style_request".to_string(),
            "false".to_string(),
        );
        if allow_http {
            opts.insert("allow_http".to_string(), "true".to_string());
        }
    }
    opts
}

/// Storage URL configured through the environment, in precedence order:
/// `FIRN_STORAGE_URL`, then the server's `FIRNFLOW_STORAGE_URI`, then a
/// bare `FIRNFLOW_S3_BUCKET` (mapped to `s3://bucket`).
fn env_storage_url() -> Option<String> {
    for var in ["FIRN_STORAGE_URL", "FIRNFLOW_STORAGE_URI"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    std::env::var("FIRNFLOW_S3_BUCKET")
        .ok()
        .filter(|b| !b.trim().is_empty())
        .map(|b| format!("s3://{b}"))
}

/// Open a firn client.
///
/// Storage is resolved in this order (the first that applies wins):
/// 1. `storage_url` keyword (e.g. `s3://bucket[/prefix]`, `gs://bucket`).
/// 2. `path` keyword — a local directory.
/// 3. environment — `FIRN_STORAGE_URL`, then `FIRNFLOW_STORAGE_URI`,
///    then `FIRNFLOW_S3_BUCKET`. S3 credentials come from the explicit
///    keyword arguments or the standard `AWS_*` environment.
/// 4. local default — `./firn_data` in the current working directory.
///
/// The directory or object-store prefix is created on first write.
#[pyfunction]
#[pyo3(signature = (
    path=None,
    *,
    storage_url=None,
    access_key=None,
    secret_key=None,
    endpoint=None,
    region=None,
))]
fn connect(
    py: Python<'_>,
    path: Option<String>,
    storage_url: Option<String>,
    access_key: Option<String>,
    secret_key: Option<String>,
    endpoint: Option<String>,
    region: Option<String>,
) -> PyResult<Client> {
    let (root, options) = if let Some(url) = storage_url {
        (
            StorageRoot::parse(&url).map_err(to_py_err)?,
            object_storage_options(access_key, secret_key, endpoint, region),
        )
    } else if let Some(p) = path {
        (StorageRoot::local(p).map_err(to_py_err)?, HashMap::new())
    } else if let Some(url) = env_storage_url() {
        (
            StorageRoot::parse(&url).map_err(to_py_err)?,
            object_storage_options(access_key, secret_key, endpoint, region),
        )
    } else {
        (
            StorageRoot::local("./firn_data").map_err(to_py_err)?,
            HashMap::new(),
        )
    };

    let metrics = Arc::new(CoreMetrics::new().map_err(to_py_err)?);
    let manager = Arc::new(NamespaceManager::new(
        root.clone(),
        options,
        Arc::clone(&metrics),
    ));
    let cache_dir = embedded_cache_dir(&root)?;
    let cache = Arc::new(
        py.allow_threads(|| {
            runtime().block_on(NamespaceCache::new(
                CACHE_MEMORY_BYTES,
                &cache_dir,
                CACHE_NVME_BYTES,
                Arc::clone(&metrics),
            ))
        })
        .map_err(to_py_err)?,
    );
    let service = Arc::new(NamespaceService::new(manager, Arc::clone(&cache), metrics));
    Ok(Client {
        service,
        cache,
        fts: Arc::new(Mutex::new(HashSet::new())),
        lifecycle: Arc::new(Lifecycle::new()),
    })
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(connect, m)?)?;
    m.add_class::<Client>()?;
    m.add_class::<Collection>()?;
    m.add_class::<Hit>()?;
    m.add("FirnError", m.py().get_type::<FirnError>())?;
    m.add("StorageError", m.py().get_type::<StorageError>())?;
    m.add("TenantError", m.py().get_type::<TenantError>())?;
    m.add("ValidationError", m.py().get_type::<ValidationError>())?;
    m.add("UnsupportedError", m.py().get_type::<UnsupportedError>())?;
    Ok(())
}
