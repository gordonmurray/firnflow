//! First-query-profile harness: measure where cold first-query
//! latency goes on a real object-storage backend.
//!
//! Profiles five measurement cases against a single namespace, runs
//! `FIRNFLOW_PROFILE_REPS` repetitions of each, and writes one
//! markdown report under `bench/results/`.
//!
//! **Measurement cases:**
//!
//! 1. `cold-process`: fresh `NamespaceManager` + `NamespaceCache` +
//!    `NamespaceService` per repetition; one query each. Lower bound
//!    on the true fresh-process number (process-wide AWS SDK / TLS
//!    state persists across reps even though the in-process objects
//!    do not, see the "lower bound" caveat below).
//! 2. `warm-identical`: single warm service, repeated identical
//!    query against the manager directly. Bypasses the foyer result
//!    cache so we measure the LanceDB / index / handle warm-state
//!    cost rather than a foyer hit.
//! 3. `warm-novel`: single warm service, novel query each rep.
//! 4. `dropped-handle`: single service, evict the pooled
//!    `(Connection, Table)` between reps via
//!    `NamespaceManager::evict_handle`. Measures the cost of
//!    re-opening the Lance table while everything else (AWS SDK,
//!    cache, schema info) stays warm.
//! 5. `fresh-process`: same shape as `cold-process` but runs after
//!    every other case, so the AWS SDK / process state is now warm
//!    while in-process objects are freshly constructed. Difference
//!    vs `cold-process` is the signal for SDK/connection warmup.
//!
//! **Seeding.** If `FIRNFLOW_PROFILE_SEED=true` (default `true`), the
//! harness upserts `FIRNFLOW_PROFILE_ROWS` rows of dimension
//! `FIRNFLOW_PROFILE_DIM`, builds an IVF_PQ index, and compacts.
//! Subsequent runs against the same namespace can set
//! `FIRNFLOW_PROFILE_SEED=false` to reuse the existing data.
//!
//! **The lower-bound caveat.** Running every case inside one
//! `tokio::main` keeps process-wide state (the AWS SDK HTTP client
//! pool, TLS sessions, the Tokio runtime) alive across "cold"
//! repetitions. The reported cold-process numbers are therefore a
//! *lower bound* on what a brand-new OS process would see; a real
//! cold-process measurement needs an outer driver that spawns a
//! fresh binary per repetition. The in-process variant is acceptable
//! for a first pass as long as the report carries this caveat.
//!
//! Run with:
//!
//! ```text
//! docker compose up -d minio minio-init
//! FIRNFLOW_STORAGE_URI=s3://firnflow-test \
//! FIRNFLOW_S3_ENDPOINT=http://127.0.0.1:9000 \
//! FIRNFLOW_S3_ACCESS_KEY=minioadmin \
//! FIRNFLOW_S3_SECRET_KEY=minioadmin \
//! FIRNFLOW_PROFILE_NAMESPACE=first-query-profile-shakeout \
//! FIRNFLOW_PROFILE_ROWS=1000 \
//! FIRNFLOW_PROFILE_DIM=32 \
//! FIRNFLOW_PROFILE_REPS=5 \
//!   ./scripts/cargo run --release -p firnflow-bench \
//!     --bin first_query_profile
//! ```
//!
//! | env var                            | default                                |
//! | ---------------------------------- | -------------------------------------- |
//! | `FIRNFLOW_STORAGE_URI`             | *(required, or `FIRNFLOW_S3_BUCKET`)*  |
//! | `FIRNFLOW_S3_BUCKET`               | *(legacy URI fallback)*                |
//! | `FIRNFLOW_S3_ENDPOINT`             | (real AWS)                             |
//! | `FIRNFLOW_S3_ACCESS_KEY`           | (default credential chain)             |
//! | `FIRNFLOW_S3_SECRET_KEY`           | (default credential chain)             |
//! | `FIRNFLOW_S3_REGION`               | `AWS_REGION` / `AWS_DEFAULT_REGION`, else `us-east-1` |
//! | `FIRNFLOW_PROFILE_NAMESPACE`       | `first-query-profile`                  |
//! | `FIRNFLOW_PROFILE_SEED`            | `true`                                 |
//! | `FIRNFLOW_PROFILE_ROWS`            | `100000`                               |
//! | `FIRNFLOW_PROFILE_DIM`             | `1536`                                 |
//! | `FIRNFLOW_PROFILE_REPS`            | `10`                                   |
//! | `FIRNFLOW_PROFILE_K`               | `10`                                   |
//! | `FIRNFLOW_PROFILE_NPROBES`         | `20`                                   |
//! | `FIRNFLOW_PROFILE_OUT`             | `bench/results/first_query_profile.md` |
//! | `FIRNFLOW_PROFILE_CACHE_RAM_MB`    | `16`                                   |
//! | `FIRNFLOW_PROFILE_CACHE_NVME_MB`   | `256`                                  |
//! | `FIRNFLOW_PROFILE_BACKEND_LABEL`   | `(derived from storage URI scheme)`    |
//! | `FIRNFLOW_PROFILE_OBJECT_CACHE`    | `false`                                |
//! | `FIRNFLOW_PROFILE_OBJECT_CACHE_DIR`| `/tmp/firn-obj-cache`                  |
//! | `FIRNFLOW_PROFILE_OBJECT_CACHE_BYTES` | `10737418240` (10 GiB)              |

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use firnflow_core::cache::NamespaceCache;
use firnflow_core::object_cache::{build_cached_session, ObjectCacheConfig, ObjectCacheMetrics};
use firnflow_core::{
    CoreMetrics, NamespaceId, NamespaceManager, NamespaceService, Scheme, StorageRoot, UpsertRow,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

struct Config {
    storage_root: StorageRoot,
    storage_options: HashMap<String, String>,
    namespace: NamespaceId,
    seed: bool,
    rows: usize,
    dim: usize,
    reps: usize,
    k: usize,
    nprobes: usize,
    out_path: PathBuf,
    cache_ram_bytes: usize,
    cache_nvme_bytes: usize,
    backend_label: String,
    /// When set, wrap the LanceDB object store in the local NVMe
    /// byte-range cache so cold/novel reads can be served from disk
    /// instead of object storage on repeat. Off by default.
    object_cache: bool,
    object_cache_dir: PathBuf,
    object_cache_bytes: u64,
    /// One shared counter set across every service build, so the
    /// report can show a cumulative hit/miss tally for the run.
    object_cache_metrics: Option<Arc<ObjectCacheMetrics>>,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let uri = env_nonempty("FIRNFLOW_STORAGE_URI");
        let bucket = env_nonempty("FIRNFLOW_S3_BUCKET");
        let storage_root = match (uri, bucket) {
            (Some(uri), _) => StorageRoot::parse(&uri)
                .map_err(|e| anyhow::anyhow!("FIRNFLOW_STORAGE_URI ({uri:?}): {e}"))?,
            (None, Some(bucket)) => StorageRoot::s3_bucket(&bucket)
                .map_err(|e| anyhow::anyhow!("FIRNFLOW_S3_BUCKET ({bucket:?}): {e}"))?,
            (None, None) => anyhow::bail!(
                "set FIRNFLOW_STORAGE_URI=s3://bucket or the legacy FIRNFLOW_S3_BUCKET=bucket"
            ),
        };

        let namespace_str = env_or("FIRNFLOW_PROFILE_NAMESPACE", "first-query-profile");
        let namespace = NamespaceId::new(namespace_str.clone())
            .map_err(|e| anyhow::anyhow!("FIRNFLOW_PROFILE_NAMESPACE ({namespace_str:?}): {e}"))?;

        let seed = parse_bool(&env_or("FIRNFLOW_PROFILE_SEED", "true"))
            .context("FIRNFLOW_PROFILE_SEED (expected true/false)")?;
        let rows: usize = env_or("FIRNFLOW_PROFILE_ROWS", "100000")
            .parse()
            .context("FIRNFLOW_PROFILE_ROWS")?;
        let dim: usize = env_or("FIRNFLOW_PROFILE_DIM", "1536")
            .parse()
            .context("FIRNFLOW_PROFILE_DIM")?;
        let reps: usize = env_or("FIRNFLOW_PROFILE_REPS", "10")
            .parse()
            .context("FIRNFLOW_PROFILE_REPS")?;
        if reps < 1 {
            anyhow::bail!("FIRNFLOW_PROFILE_REPS must be >= 1");
        }
        let k: usize = env_or("FIRNFLOW_PROFILE_K", "10")
            .parse()
            .context("FIRNFLOW_PROFILE_K")?;
        let nprobes: usize = env_or("FIRNFLOW_PROFILE_NPROBES", "20")
            .parse()
            .context("FIRNFLOW_PROFILE_NPROBES")?;
        let out_path = PathBuf::from(env_or(
            "FIRNFLOW_PROFILE_OUT",
            "bench/results/first_query_profile.md",
        ));
        let cache_ram_mb: usize = env_or("FIRNFLOW_PROFILE_CACHE_RAM_MB", "16")
            .parse()
            .context("FIRNFLOW_PROFILE_CACHE_RAM_MB")?;
        let cache_nvme_mb: usize = env_or("FIRNFLOW_PROFILE_CACHE_NVME_MB", "256")
            .parse()
            .context("FIRNFLOW_PROFILE_CACHE_NVME_MB")?;

        let mut opts = HashMap::new();
        if let Ok(v) = std::env::var("FIRNFLOW_S3_ENDPOINT") {
            opts.insert("aws_endpoint".into(), v);
            opts.insert("allow_http".into(), "true".into());
            opts.insert("aws_virtual_hosted_style_request".into(), "false".into());
        }
        if let Ok(v) = std::env::var("FIRNFLOW_S3_ACCESS_KEY") {
            opts.insert("aws_access_key_id".into(), v);
        }
        if let Ok(v) = std::env::var("FIRNFLOW_S3_SECRET_KEY") {
            opts.insert("aws_secret_access_key".into(), v);
        }
        opts.insert(
            "aws_region".into(),
            firnflow_core::resolve_s3_region(|k| std::env::var(k).ok()),
        );

        let backend_label = std::env::var("FIRNFLOW_PROFILE_BACKEND_LABEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| derive_backend_label(&storage_root, &opts));

        let object_cache = parse_bool(&env_or("FIRNFLOW_PROFILE_OBJECT_CACHE", "false"))
            .context("FIRNFLOW_PROFILE_OBJECT_CACHE (expected true/false)")?;
        let object_cache_dir = PathBuf::from(env_or(
            "FIRNFLOW_PROFILE_OBJECT_CACHE_DIR",
            "/tmp/firn-obj-cache",
        ));
        // 10 GiB default budget.
        let object_cache_bytes: u64 = env_or("FIRNFLOW_PROFILE_OBJECT_CACHE_BYTES", "10737418240")
            .parse()
            .context("FIRNFLOW_PROFILE_OBJECT_CACHE_BYTES")?;
        let object_cache_metrics =
            object_cache.then(|| Arc::new(ObjectCacheMetrics::unregistered()));

        Ok(Self {
            storage_root,
            storage_options: opts,
            namespace,
            seed,
            rows,
            dim,
            reps,
            k,
            nprobes,
            out_path,
            cache_ram_bytes: cache_ram_mb * 1024 * 1024,
            cache_nvme_bytes: cache_nvme_mb * 1024 * 1024,
            backend_label,
            object_cache,
            object_cache_dir,
            object_cache_bytes,
            object_cache_metrics,
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_bool(s: &str) -> anyhow::Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "y" => Ok(true),
        "false" | "0" | "no" | "n" => Ok(false),
        other => anyhow::bail!("expected true/false, got {other:?}"),
    }
}

fn derive_backend_label(root: &StorageRoot, opts: &HashMap<String, String>) -> String {
    // Identify the backend by scheme and region without echoing the
    // bucket name, so a committed report never carries it. Set
    // FIRNFLOW_PROFILE_BACKEND_LABEL to override.
    let scheme = match root.scheme() {
        Scheme::S3 => "S3",
        Scheme::Gcs => "GCS",
        Scheme::Local => "local filesystem",
    };
    let region = opts.get("aws_region").map(String::as_str).unwrap_or("");
    let suffix = if region.is_empty() {
        String::new()
    } else {
        format!(" ({region})")
    };
    if opts.contains_key("aws_endpoint") {
        format!("{scheme}-compatible endpoint{suffix}")
    } else {
        format!("{scheme}{suffix}")
    }
}

// ---------------------------------------------------------------------------
// stats + formatting
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Percentiles {
    n: usize,
    p50: Duration,
    p95: Duration,
    p99: Duration,
    min: Duration,
    max: Duration,
}

fn percentiles(mut samples: Vec<Duration>) -> Percentiles {
    assert!(!samples.is_empty(), "no samples");
    samples.sort_unstable();
    let n = samples.len();
    Percentiles {
        n,
        // For small N (e.g. reps=5) p95/p99 collapse onto max; that's
        // expected: the report calls reps out explicitly so the reader
        // can interpret.
        p50: samples[n / 2],
        p95: samples[((n * 95) / 100).min(n - 1)],
        p99: samples[((n * 99) / 100).min(n - 1)],
        min: samples[0],
        max: *samples.last().unwrap(),
    }
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_secs_f64() * 1_000_000.0;
    if us < 1000.0 {
        format!("{:.2} us", us)
    } else if us < 1_000_000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}

// ---------------------------------------------------------------------------
// data generation
// ---------------------------------------------------------------------------

fn make_vector(seed: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| (((seed * 7919 + j * 31) as f32) * 0.001).sin())
        .collect()
}

fn make_query_vector(seed: usize, dim: usize) -> Vec<f32> {
    // Offset into a region of seed-space that does not collide with
    // upserted row vectors so cold queries don't get an artificial
    // exact-match speedup.
    make_vector(seed + 1_000_000, dim)
}

fn ts_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn utc_now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = ymd_from_unix_secs(secs);
    let rem = secs.rem_euclid(86_400);
    let hh = (rem / 3600) % 24;
    let mm = (rem / 60) % 60;
    let ss = rem % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn today_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = ymd_from_unix_secs(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

// Howard Hinnant's days-from-civil inverse. Same approach as the
// existing `chrono_today` helper in the cold-vs-warm harness, broken
// out so it returns the (y, m, d) triple for both the date-only and
// the full-timestamp formatters.
fn ymd_from_unix_secs(secs: i64) -> (i64, u64, u64) {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y_base = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y_base + 1 } else { y_base };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// service construction
// ---------------------------------------------------------------------------

struct ServiceBundle {
    manager: Arc<NamespaceManager>,
    service: NamespaceService,
    metrics: Arc<CoreMetrics>,
    _cache_dir: TempDir,
}

async fn build_service(cfg: &Config) -> anyhow::Result<ServiceBundle> {
    let metrics = Arc::new(CoreMetrics::new().context("build CoreMetrics")?);
    let mut manager = NamespaceManager::new(
        cfg.storage_root.clone(),
        cfg.storage_options.clone(),
        Arc::clone(&metrics),
    );
    if cfg.object_cache {
        // Persistent (non-temp) cache dir shared across every service
        // build, so cached byte ranges survive between the fresh
        // managers a "cold" rep constructs. The shared metrics counter
        // accumulates hits/misses across the whole run.
        let oc_cfg = ObjectCacheConfig::new(cfg.object_cache_dir.clone(), cfg.object_cache_bytes);
        let oc_metrics = cfg
            .object_cache_metrics
            .clone()
            .unwrap_or_else(|| Arc::new(ObjectCacheMetrics::unregistered()));
        let session = build_cached_session(&oc_cfg, oc_metrics);
        manager = manager.with_object_cache_session(session);
    }
    let manager = Arc::new(manager);
    let cache_dir = tempfile::tempdir().context("tempdir for cache")?;
    let cache = Arc::new(
        NamespaceCache::new(
            cfg.cache_ram_bytes,
            cache_dir.path(),
            cfg.cache_nvme_bytes,
            Arc::clone(&metrics),
        )
        .await
        .map_err(|e| anyhow::anyhow!("build cache: {e}"))?,
    );
    let service = NamespaceService::new(Arc::clone(&manager), cache, Arc::clone(&metrics));
    Ok(ServiceBundle {
        manager,
        service,
        metrics,
        _cache_dir: cache_dir,
    })
}

async fn upsert_rows(
    service: &NamespaceService,
    ns: &NamespaceId,
    total_rows: usize,
    dim: usize,
) -> anyhow::Result<Duration> {
    let batch_size = 10_000;
    let start = Instant::now();
    let mut offset = 0;
    while offset < total_rows {
        let end = (offset + batch_size).min(total_rows);
        let rows: Vec<UpsertRow> = (offset..end)
            .map(|i| (i as u64, make_vector(i, dim)).into())
            .collect();
        service.upsert(ns, rows).await?;
        offset = end;
    }
    Ok(start.elapsed())
}

// ---------------------------------------------------------------------------
// measurement primitives
// ---------------------------------------------------------------------------

async fn query_via_manager(
    manager: &NamespaceManager,
    ns: &NamespaceId,
    vector: Vec<f32>,
    k: usize,
    nprobes: usize,
) -> anyhow::Result<Duration> {
    let start = Instant::now();
    // Going through the manager skips the foyer result cache, so
    // warm-second-query measurements reflect LanceDB + connection
    // pool + OS-level warm state, not a foyer hit.
    //
    // include_vector=true keeps the measurement shape that predates the
    // projection knob (the stored vectors were always read back), so
    // these numbers stay comparable to the published cold/warm framing.
    manager
        .query(ns, vector, None, k, Some(nprobes), None, None, true)
        .await
        .map_err(|e| anyhow::anyhow!("query: {e}"))?;
    Ok(start.elapsed())
}

#[derive(Clone)]
struct CaseResult {
    name: &'static str,
    description: &'static str,
    percentiles: Percentiles,
    samples: Vec<Duration>,
    s3_requests_before: u64,
    s3_requests_after: u64,
}

impl CaseResult {
    fn s3_delta(&self) -> u64 {
        self.s3_requests_after
            .saturating_sub(self.s3_requests_before)
    }
}

// ---------------------------------------------------------------------------
// per-case runners
// ---------------------------------------------------------------------------

async fn run_cold_process(
    cfg: &Config,
    label: &'static str,
    description: &'static str,
) -> anyhow::Result<CaseResult> {
    let mut samples = Vec::with_capacity(cfg.reps);
    // The first cold-process repetition includes one-time AWS SDK
    // setup the first time the binary touches S3; later cold-process
    // reps share that warmed-up SDK state. We record both numbers
    // (the full distribution) and call out the asymmetry in the
    // report. This is exactly the SDK-warmup signal the `fresh-
    // process` case is meant to surface against `cold-process`.
    let mut tot_before = 0u64;
    let mut tot_after = 0u64;

    for i in 0..cfg.reps {
        let bundle = build_service(cfg).await?;
        let before = read_s3_requests(&bundle.metrics, &cfg.namespace);
        if i == 0 {
            tot_before = before;
        }
        let vec = make_query_vector(i, cfg.dim);
        let elapsed =
            query_via_manager(&bundle.manager, &cfg.namespace, vec, cfg.k, cfg.nprobes).await?;
        samples.push(elapsed);
        tot_after = read_s3_requests(&bundle.metrics, &cfg.namespace);
        drop(bundle);
        println!(
            "  {label} rep {:>2}/{}: {}",
            i + 1,
            cfg.reps,
            fmt_dur(elapsed)
        );
    }

    Ok(CaseResult {
        name: label,
        description,
        percentiles: percentiles(samples.clone()),
        samples,
        s3_requests_before: tot_before,
        s3_requests_after: tot_after,
    })
}

async fn run_warm_identical(cfg: &Config) -> anyhow::Result<CaseResult> {
    let bundle = build_service(cfg).await?;
    let warmup_vec = make_query_vector(0, cfg.dim);

    // One warm-up query to populate the manager handle pool, Lance
    // dataset cache, and the AWS SDK HTTP pool. Not recorded.
    let _ = query_via_manager(
        &bundle.manager,
        &cfg.namespace,
        warmup_vec.clone(),
        cfg.k,
        cfg.nprobes,
    )
    .await?;

    let before = read_s3_requests(&bundle.metrics, &cfg.namespace);
    let mut samples = Vec::with_capacity(cfg.reps);
    for i in 0..cfg.reps {
        let elapsed = query_via_manager(
            &bundle.manager,
            &cfg.namespace,
            warmup_vec.clone(),
            cfg.k,
            cfg.nprobes,
        )
        .await?;
        samples.push(elapsed);
        println!(
            "  warm-identical rep {:>2}/{}: {}",
            i + 1,
            cfg.reps,
            fmt_dur(elapsed)
        );
    }
    let after = read_s3_requests(&bundle.metrics, &cfg.namespace);

    Ok(CaseResult {
        name: "warm-identical",
        description: "single warm service, repeated identical query (bypasses foyer)",
        percentiles: percentiles(samples.clone()),
        samples,
        s3_requests_before: before,
        s3_requests_after: after,
    })
}

async fn run_warm_novel(cfg: &Config) -> anyhow::Result<CaseResult> {
    let bundle = build_service(cfg).await?;
    let warmup_vec = make_query_vector(0, cfg.dim);
    let _ = query_via_manager(
        &bundle.manager,
        &cfg.namespace,
        warmup_vec,
        cfg.k,
        cfg.nprobes,
    )
    .await?;

    let before = read_s3_requests(&bundle.metrics, &cfg.namespace);
    let mut samples = Vec::with_capacity(cfg.reps);
    for i in 0..cfg.reps {
        let vec = make_query_vector(1 + i, cfg.dim);
        let elapsed =
            query_via_manager(&bundle.manager, &cfg.namespace, vec, cfg.k, cfg.nprobes).await?;
        samples.push(elapsed);
        println!(
            "  warm-novel     rep {:>2}/{}: {}",
            i + 1,
            cfg.reps,
            fmt_dur(elapsed)
        );
    }
    let after = read_s3_requests(&bundle.metrics, &cfg.namespace);

    Ok(CaseResult {
        name: "warm-novel",
        description: "single warm service, novel query per rep",
        percentiles: percentiles(samples.clone()),
        samples,
        s3_requests_before: before,
        s3_requests_after: after,
    })
}

async fn run_dropped_handle(cfg: &Config) -> anyhow::Result<CaseResult> {
    let bundle = build_service(cfg).await?;
    let warmup_vec = make_query_vector(0, cfg.dim);
    let _ = query_via_manager(
        &bundle.manager,
        &cfg.namespace,
        warmup_vec.clone(),
        cfg.k,
        cfg.nprobes,
    )
    .await?;

    let before = read_s3_requests(&bundle.metrics, &cfg.namespace);
    let mut samples = Vec::with_capacity(cfg.reps);
    for i in 0..cfg.reps {
        // Force a re-open of the (Connection, Table) pair without
        // tearing down the rest of the process. Everything else
        // (AWS SDK client pool, schema-info cache, Tokio runtime)
        // stays warm.
        bundle.manager.evict_handle(&cfg.namespace);
        let vec = make_query_vector(100 + i, cfg.dim);
        let elapsed =
            query_via_manager(&bundle.manager, &cfg.namespace, vec, cfg.k, cfg.nprobes).await?;
        samples.push(elapsed);
        println!(
            "  dropped-handle rep {:>2}/{}: {}",
            i + 1,
            cfg.reps,
            fmt_dur(elapsed)
        );
    }
    let after = read_s3_requests(&bundle.metrics, &cfg.namespace);

    Ok(CaseResult {
        name: "dropped-handle",
        description: "evict pooled (Connection, Table) between reps; process stays warm",
        percentiles: percentiles(samples.clone()),
        samples,
        s3_requests_before: before,
        s3_requests_after: after,
    })
}

fn read_s3_requests(metrics: &CoreMetrics, ns: &NamespaceId) -> u64 {
    let body = match metrics.encode() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let needle_ns = format!(r#"namespace="{}""#, ns.as_str());
    let mut total = 0u64;
    for line in body.lines() {
        if !line.starts_with("firnflow_s3_requests_total") {
            continue;
        }
        if !line.contains(&needle_ns) {
            continue;
        }
        if let Some(value_str) = line.rsplit_once(char::is_whitespace).map(|(_, v)| v) {
            if let Ok(v) = value_str.parse::<f64>() {
                total = total.saturating_add(v as u64);
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// seeding
// ---------------------------------------------------------------------------

async fn ensure_seeded(cfg: &Config) -> anyhow::Result<SeedSummary> {
    let bundle = build_service(cfg).await?;
    if !cfg.seed {
        // Probe by running a tiny query; if the namespace is empty
        // we get an empty result set, which we accept (the per-case
        // numbers will reveal that the namespace was unseeded).
        let probe_start = Instant::now();
        let _ = query_via_manager(
            &bundle.manager,
            &cfg.namespace,
            make_query_vector(0, cfg.dim),
            1,
            cfg.nprobes,
        )
        .await?;
        return Ok(SeedSummary {
            seeded_now: false,
            upsert_elapsed: None,
            index_elapsed: None,
            compact_elapsed: None,
            probe_elapsed: Some(probe_start.elapsed()),
        });
    }

    println!("seeding {} rows of dim {}", cfg.rows, cfg.dim);
    let upsert_elapsed = upsert_rows(&bundle.service, &cfg.namespace, cfg.rows, cfg.dim).await?;
    println!("  upsert: {}", fmt_dur(upsert_elapsed));

    // IVF_PQ: num_partitions ~= sqrt(rows), num_sub_vectors = dim/16
    // (must divide dim evenly).
    let num_partitions = (cfg.rows as f64).sqrt() as u32;
    let num_sub_vectors = (cfg.dim / 16).max(1) as u32;
    println!("  building IVF_PQ index: partitions={num_partitions}, sub_vectors={num_sub_vectors}");
    let idx_start = Instant::now();
    bundle
        .service
        // num_bits=None ⇒ default 8-bit PQ (the 4-bit path is opt-in).
        .create_index(
            &cfg.namespace,
            Some(num_partitions),
            Some(num_sub_vectors),
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("create_index: {e}"))?;
    let index_elapsed = idx_start.elapsed();
    println!("  index: {}", fmt_dur(index_elapsed));

    let compact_start = Instant::now();
    bundle
        .service
        .compact(&cfg.namespace)
        .await
        .map_err(|e| anyhow::anyhow!("compact: {e}"))?;
    let compact_elapsed = compact_start.elapsed();
    println!("  compact: {}", fmt_dur(compact_elapsed));

    Ok(SeedSummary {
        seeded_now: true,
        upsert_elapsed: Some(upsert_elapsed),
        index_elapsed: Some(index_elapsed),
        compact_elapsed: Some(compact_elapsed),
        probe_elapsed: None,
    })
}

struct SeedSummary {
    seeded_now: bool,
    upsert_elapsed: Option<Duration>,
    index_elapsed: Option<Duration>,
    compact_elapsed: Option<Duration>,
    probe_elapsed: Option<Duration>,
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn write_report(
    cfg: &Config,
    seed: &SeedSummary,
    start_utc: &str,
    stop_utc: &str,
    cases: &[CaseResult],
    out_path: &Path,
) -> anyhow::Result<()> {
    use std::fmt::Write;

    let date = today_iso();
    let mut out = String::new();
    writeln!(out, "# First-query latency profile")?;
    writeln!(out)?;
    writeln!(out, "- **Date**: {date}")?;
    writeln!(out, "- **start_utc**: {start_utc}")?;
    writeln!(out, "- **stop_utc**: {stop_utc}")?;
    writeln!(out, "- **Backend**: {}", cfg.backend_label)?;
    writeln!(
        out,
        "- **Storage prefix**: `{}/` under the configured bucket, a stable S3 access-log filter",
        cfg.namespace.as_str()
    )?;
    writeln!(out, "- **Namespace**: `{}`", cfg.namespace.as_str())?;
    writeln!(
        out,
        "- **Config**: rows={}, dim={}, k={}, nprobes={}, reps={}",
        cfg.rows, cfg.dim, cfg.k, cfg.nprobes, cfg.reps
    )?;
    writeln!(
        out,
        "- **Foyer cache**: RAM={} MB, NVMe={} MB (fresh tempdir per service build)",
        cfg.cache_ram_bytes / (1024 * 1024),
        cfg.cache_nvme_bytes / (1024 * 1024)
    )?;
    if cfg.object_cache {
        writeln!(
            out,
            "- **Object cache**: ENABLED, dir `{}`, budget {} GiB (persists across reps)",
            cfg.object_cache_dir.display(),
            cfg.object_cache_bytes / (1024 * 1024 * 1024)
        )?;
    } else {
        writeln!(out, "- **Object cache**: disabled")?;
    }
    writeln!(
        out,
        "- **Harness**: `./scripts/cargo run --release -p firnflow-bench --bin first_query_profile`"
    )?;
    writeln!(out)?;

    writeln!(out, "## Seed")?;
    writeln!(out)?;
    if seed.seeded_now {
        writeln!(
            out,
            "- Seeded this run: upsert {}, index {}, compact {}",
            seed.upsert_elapsed
                .map(fmt_dur)
                .unwrap_or_else(|| "-".into()),
            seed.index_elapsed
                .map(fmt_dur)
                .unwrap_or_else(|| "-".into()),
            seed.compact_elapsed
                .map(fmt_dur)
                .unwrap_or_else(|| "-".into())
        )?;
    } else {
        writeln!(
            out,
            "- Reused existing namespace (no seed step). Probe query: {}",
            seed.probe_elapsed
                .map(fmt_dur)
                .unwrap_or_else(|| "-".into())
        )?;
    }
    writeln!(out)?;

    writeln!(out, "## Per-case latency")?;
    writeln!(out)?;
    writeln!(
        out,
        "| case | reps | p50 | p95 | p99 | min | max | s3_requests delta |"
    )?;
    writeln!(out, "| ---- | ---:| ---:| ---:| ---:| ---:| ---:| ---: |")?;
    for c in cases {
        writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            c.name,
            c.percentiles.n,
            fmt_dur(c.percentiles.p50),
            fmt_dur(c.percentiles.p95),
            fmt_dur(c.percentiles.p99),
            fmt_dur(c.percentiles.min),
            fmt_dur(c.percentiles.max),
            c.s3_delta()
        )?;
    }
    writeln!(out)?;

    writeln!(out, "## Case definitions")?;
    writeln!(out)?;
    for c in cases {
        writeln!(out, "- **{}**: {}.", c.name, c.description)?;
    }
    writeln!(out)?;

    writeln!(out, "## Raw samples")?;
    writeln!(out)?;
    for c in cases {
        let formatted: Vec<String> = c.samples.iter().copied().map(fmt_dur).collect();
        writeln!(
            out,
            "- `{}` ({} reps): {}",
            c.name,
            c.samples.len(),
            formatted.join(", ")
        )?;
    }
    writeln!(out)?;

    if let Some(m) = &cfg.object_cache_metrics {
        let (inner_gets, hits, misses, s3_bytes, evictions) = m.snapshot();
        writeln!(out, "## Object cache counters")?;
        writeln!(out)?;
        writeln!(
            out,
            "Cumulative over the whole run (seed + all cases), shared across every service build:"
        )?;
        writeln!(out)?;
        writeln!(out, "- hits: {hits}")?;
        writeln!(out, "- misses: {misses}")?;
        writeln!(
            out,
            "- inner_gets (reads that fell through to object storage): {inner_gets}"
        )?;
        writeln!(
            out,
            "- s3_bytes (bytes fetched from object storage): {s3_bytes}"
        )?;
        writeln!(out, "- evictions: {evictions}")?;
        writeln!(out)?;
    }

    writeln!(out, "## Caveats")?;
    writeln!(out)?;
    writeln!(
        out,
        "- **`cold-process` is a lower bound on a true fresh-process number.** \
         Every repetition runs inside one binary invocation, so the AWS SDK \
         HTTP client pool, TLS sessions, and the Tokio runtime persist across \
         repetitions even though the in-process `NamespaceManager` + cache + \
         service objects are rebuilt each rep. A true fresh-process measurement \
         needs an outer driver that spawns a fresh binary per repetition. The \
         `fresh-process` case runs after every other case so the difference \
         vs `cold-process` is a coarse signal for SDK / connection warmup."
    )?;
    writeln!(
        out,
        "- **`s3_requests delta` only counts firnflow's service-boundary calls**, \
         not raw `object_store` GETs / range GETs. For real S3 access-log attribution \
         use the namespace prefix above as the path filter (primary) or the \
         start/stop UTC window as a backstop."
    )?;
    writeln!(
        out,
        "- **`warm-identical` and `warm-novel` bypass the foyer result cache** \
         by calling `NamespaceManager::query` directly. This isolates the \
         LanceDB / index / handle-pool warm-state cost. A foyer hit would \
         otherwise dominate the number and tell us nothing about the underlying \
         object-store path."
    )?;

    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    fs::write(out_path, &out).with_context(|| format!("writing {}", out_path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    println!(
        "first-query-profile config: namespace={}, seed={}, rows={}, dim={}, reps={}, \
         k={}, nprobes={}, backend={}",
        cfg.namespace.as_str(),
        cfg.seed,
        cfg.rows,
        cfg.dim,
        cfg.reps,
        cfg.k,
        cfg.nprobes,
        cfg.backend_label
    );
    println!("storage_root={}", cfg.storage_root);
    println!("run_id={}", ts_nanos());

    if cfg.object_cache {
        // Start from an empty cache so the first cold rep is a true
        // miss and the warming is visible in the per-rep samples.
        let _ = fs::remove_dir_all(&cfg.object_cache_dir);
        fs::create_dir_all(&cfg.object_cache_dir).with_context(|| {
            format!(
                "creating object cache dir {}",
                cfg.object_cache_dir.display()
            )
        })?;
        println!(
            "object cache: ENABLED, dir={}, budget={} GiB",
            cfg.object_cache_dir.display(),
            cfg.object_cache_bytes / (1024 * 1024 * 1024)
        );
    }

    let start_utc = utc_now_iso();

    let seed_summary = ensure_seeded(&cfg).await?;

    let mut cases = Vec::with_capacity(5);

    println!("\n[1/5] cold-process");
    cases.push(
        run_cold_process(
            &cfg,
            "cold-process",
            "fresh manager + cache + service per rep, one query each",
        )
        .await?,
    );

    println!("\n[2/5] warm-identical");
    cases.push(run_warm_identical(&cfg).await?);

    println!("\n[3/5] warm-novel");
    cases.push(run_warm_novel(&cfg).await?);

    println!("\n[4/5] dropped-handle");
    cases.push(run_dropped_handle(&cfg).await?);

    println!("\n[5/5] fresh-process");
    cases.push(
        run_cold_process(
            &cfg,
            "fresh-process",
            "fresh manager + cache + service per rep, run AFTER all other cases so \
             AWS SDK / process state is now warm (in-process variant only)",
        )
        .await?,
    );

    let stop_utc = utc_now_iso();

    write_report(
        &cfg,
        &seed_summary,
        &start_utc,
        &stop_utc,
        &cases,
        &cfg.out_path,
    )?;
    println!("\nwrote {}", cfg.out_path.display());

    Ok(())
}
