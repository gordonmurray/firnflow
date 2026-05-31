//! Semantic-cache latency profile.
//!
//! Drives the `NamespaceService` directly so the numbers isolate the
//! cache-aside read path rather than HTTP extraction overhead.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use bincode::config;
use firnflow_core::cache::NamespaceCache;
use firnflow_core::{
    CoreMetrics, NamespaceId, NamespaceManager, NamespaceService, QueryRequest, QueryResultSet,
    SemanticCacheRequest, StorageRoot, UpsertRow,
};
use tempfile::TempDir;

struct BenchConfig {
    storage_root: StorageRoot,
    storage_options: HashMap<String, String>,
    dim: usize,
    rows: usize,
    reps: usize,
    k: usize,
    nprobes: usize,
    threshold: f32,
    cosine_sweep: Vec<f32>,
    fill_sizes: Vec<usize>,
    out_path: PathBuf,
    cache_ram_bytes: usize,
    cache_nvme_bytes: usize,
}

impl BenchConfig {
    fn from_env() -> anyhow::Result<Self> {
        let uri = std::env::var("FIRNFLOW_STORAGE_URI")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty());
        let bucket = std::env::var("FIRNFLOW_S3_BUCKET")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty());
        let storage_root = match (uri, bucket) {
            (Some(uri), _) => StorageRoot::parse(&uri)
                .map_err(|e| anyhow::anyhow!("FIRNFLOW_STORAGE_URI ({uri:?}): {e}"))?,
            (None, Some(bucket)) => StorageRoot::s3_bucket(&bucket)
                .map_err(|e| anyhow::anyhow!("FIRNFLOW_S3_BUCKET ({bucket:?}): {e}"))?,
            (None, None) => anyhow::bail!(
                "set FIRNFLOW_STORAGE_URI=s3://bucket or the legacy FIRNFLOW_S3_BUCKET=bucket"
            ),
        };

        let dim = parse_env("FIRNFLOW_BENCH_DIM", 512usize)?;
        let rows = parse_env("FIRNFLOW_BENCH_ROWS", 10_000usize)?;
        let reps = parse_env("FIRNFLOW_SEM_REPS", 200usize)?;
        let k = parse_env("FIRNFLOW_SEM_K", 10usize)?;
        let nprobes = parse_env("FIRNFLOW_BENCH_INDEX_NPROBES", 20usize)?;
        let threshold = parse_env("FIRNFLOW_SEM_THRESHOLD", 0.995f32)?;
        let cosine_sweep = parse_f32_list(
            "FIRNFLOW_SEM_COSINE_SWEEP",
            "0.999,0.997,0.995,0.99,0.97,0.95,0.90,0.85",
        )?;
        let fill_sizes = parse_usize_list("FIRNFLOW_SEM_FILL", "1,16,128,512,1024")?;
        let out_path = PathBuf::from(env_or(
            "FIRNFLOW_SEM_OUT",
            "bench/results/semantic_cache_profile.md",
        ));
        let cache_ram_mb = parse_env("FIRNFLOW_BENCH_CACHE_RAM_MB", 16usize)?;
        let cache_nvme_mb = parse_env("FIRNFLOW_BENCH_CACHE_NVME_MB", 256usize)?;

        if dim == 0 {
            anyhow::bail!("FIRNFLOW_BENCH_DIM must be > 0");
        }
        if rows == 0 {
            anyhow::bail!("FIRNFLOW_BENCH_ROWS must be > 0");
        }
        if reps == 0 {
            anyhow::bail!("FIRNFLOW_SEM_REPS must be > 0");
        }
        if k == 0 {
            anyhow::bail!("FIRNFLOW_SEM_K must be > 0");
        }
        if !threshold.is_finite() || threshold <= 0.0 || threshold > 1.0 {
            anyhow::bail!("FIRNFLOW_SEM_THRESHOLD must be in (0.0, 1.0]");
        }
        for c in &cosine_sweep {
            if !c.is_finite() || *c < 0.0 || *c > 1.0 {
                anyhow::bail!("FIRNFLOW_SEM_COSINE_SWEEP values must be in [0.0, 1.0]");
            }
        }
        if fill_sizes.is_empty() {
            anyhow::bail!("FIRNFLOW_SEM_FILL must contain at least one entry");
        }

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
            env_or("FIRNFLOW_S3_REGION", "us-east-1"),
        );

        Ok(Self {
            storage_root,
            storage_options: opts,
            dim,
            rows,
            reps,
            k,
            nprobes,
            threshold,
            cosine_sweep,
            fill_sizes,
            out_path,
            cache_ram_bytes: cache_ram_mb * 1024 * 1024,
            cache_nvme_bytes: cache_nvme_mb * 1024 * 1024,
        })
    }
}

struct BenchService {
    service: NamespaceService,
    cache: Arc<NamespaceCache>,
    metrics: Arc<CoreMetrics>,
    _tmp: TempDir,
}

#[derive(Clone, Copy)]
struct Percentiles {
    p50: Duration,
    p95: Duration,
    p99: Duration,
    max: Duration,
}

#[derive(Default, Clone, Copy)]
struct MetricSnapshot {
    cache_hits: u64,
    cache_misses: u64,
    semantic_hits: u64,
    semantic_misses: u64,
    semantic_empty: u64,
    backend_queries: u64,
}

#[derive(Clone, Copy)]
struct MetricDelta {
    cache_hits: u64,
    cache_misses: u64,
    semantic_hits: u64,
    semantic_misses: u64,
    semantic_empty: u64,
    backend_queries: u64,
}

struct CaseResult {
    samples: usize,
    latency: Percentiles,
    metrics: MetricDelta,
}

struct LatencyResults {
    cold_novel: CaseResult,
    foyer_hit: CaseResult,
    semantic_hit: CaseResult,
    semantic_miss_below: CaseResult,
    hit_cosine: f32,
    below_cosine: f32,
    hit_overlap: usize,
}

struct ThresholdRow {
    target_cosine: f32,
    observed_cosine: f32,
    outcome: String,
    overlap: usize,
}

struct ScanRow {
    entries: usize,
    result: CaseResult,
}

struct ReportInput<'a> {
    cfg: &'a BenchConfig,
    ns: &'a NamespaceId,
    start_utc: &'a str,
    stop_utc: &'a str,
    upsert_elapsed: Duration,
    index_elapsed: Duration,
    num_partitions: u32,
    num_sub_vectors: u32,
    latency: &'a LatencyResults,
    threshold_rows: &'a [ThresholdRow],
    scan_rows: &'a [ScanRow],
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn parse_env<T>(key: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(value) => value
            .parse()
            .map_err(|e| anyhow::anyhow!("{key}={value:?}: {e}")),
        Err(_) => Ok(default),
    }
}

fn parse_f32_list(key: &str, default: &str) -> anyhow::Result<Vec<f32>> {
    env_or(key, default)
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<f32>()
                .map_err(|e| anyhow::anyhow!("{key} entry {s:?}: {e}"))
        })
        .collect()
}

fn parse_usize_list(key: &str, default: &str) -> anyhow::Result<Vec<usize>> {
    env_or(key, default)
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<usize>()
                .map_err(|e| anyhow::anyhow!("{key} entry {s:?}: {e}"))
        })
        .collect()
}

async fn build_bench_service(
    manager: Arc<NamespaceManager>,
    cfg: &BenchConfig,
) -> anyhow::Result<BenchService> {
    let tmp = tempfile::tempdir()?;
    let metrics = Arc::new(CoreMetrics::new()?);
    let cache = Arc::new(
        NamespaceCache::new(
            cfg.cache_ram_bytes,
            tmp.path(),
            cfg.cache_nvme_bytes,
            Arc::clone(&metrics),
        )
        .await
        .map_err(|e| anyhow::anyhow!("build cache: {e}"))?,
    );
    let service = NamespaceService::new(manager, Arc::clone(&cache), Arc::clone(&metrics));
    Ok(BenchService {
        service,
        cache,
        metrics,
        _tmp: tmp,
    })
}

fn make_vector(seed: usize, dim: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(dim);
    for j in 0..dim {
        let a = seed.wrapping_mul(7_919).wrapping_add(j.wrapping_mul(31));
        let b = seed.wrapping_mul(104_729).wrapping_add(j.wrapping_mul(17));
        v.push(((a as f32) * 0.001).sin() + 0.5 * ((b as f32) * 0.0007).cos());
    }
    normalize(v)
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = l2_norm(&v);
    if norm == 0.0 || !norm.is_finite() {
        if let Some(first) = v.first_mut() {
            *first = 1.0;
        }
        return v;
    }
    for x in &mut v {
        *x /= norm;
    }
    v
}

fn l2_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let denom = l2_norm(a) * l2_norm(b);
    if denom == 0.0 {
        0.0
    } else {
        dot(a, b) / denom
    }
}

fn orthogonal_unit(base: &[f32], seed: usize) -> Vec<f32> {
    for offset in 0..16 {
        let mut v = make_vector(seed + offset * 104_729, base.len());
        let projection = dot(&v, base);
        for (x, b) in v.iter_mut().zip(base.iter()) {
            *x -= projection * b;
        }
        let norm = l2_norm(&v);
        if norm > 1e-6 && norm.is_finite() {
            for x in &mut v {
                *x /= norm;
            }
            return v;
        }
    }

    let mut v = vec![0.0; base.len()];
    let idx = base
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    v[idx] = 1.0;
    let projection = dot(&v, base);
    for (x, b) in v.iter_mut().zip(base.iter()) {
        *x -= projection * b;
    }
    normalize(v)
}

fn vector_at_cosine(base: &[f32], target_cosine: f32, seed: usize) -> Vec<f32> {
    let base = normalize(base.to_vec());
    let c = target_cosine.clamp(0.0, 1.0);
    let orthogonal = orthogonal_unit(&base, seed);
    let orthogonal_scale = (1.0 - c * c).max(0.0).sqrt();
    let v = base
        .iter()
        .zip(orthogonal.iter())
        .map(|(b, o)| c * b + orthogonal_scale * o)
        .collect();
    normalize(v)
}

fn above_threshold(threshold: f32) -> f32 {
    if threshold >= 0.9999 {
        1.0
    } else {
        (threshold + 0.002).min(0.9999)
    }
}

fn below_threshold(threshold: f32) -> f32 {
    if threshold > 0.001 {
        threshold - 0.001
    } else {
        threshold * 0.5
    }
}

fn vector_query(
    vector: Vec<f32>,
    k: usize,
    nprobes: usize,
    semantic_threshold: Option<f32>,
) -> QueryRequest {
    QueryRequest {
        vector,
        vectors: None,
        k,
        nprobes: Some(nprobes),
        text: None,
        semantic_cache: semantic_threshold.map(|threshold| SemanticCacheRequest {
            enabled: true,
            min_similarity: Some(threshold),
        }),
    }
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

async fn true_query(
    manager: &NamespaceManager,
    ns: &NamespaceId,
    vector: Vec<f32>,
    cfg: &BenchConfig,
) -> anyhow::Result<QueryResultSet> {
    manager
        .query(ns, vector, None, cfg.k, Some(cfg.nprobes), None)
        .await
        .map_err(|e| anyhow::anyhow!("manager query: {e}"))
}

fn encode_result(result: &QueryResultSet) -> anyhow::Result<Vec<u8>> {
    bincode::serde::encode_to_vec(result, config::standard())
        .map_err(|e| anyhow::anyhow!("encode result payload: {e}"))
}

fn seed_semantic_sidecar(
    bench: &BenchService,
    ns: &NamespaceId,
    vectors: Vec<Vec<f32>>,
    result_bytes: &[u8],
    cfg: &BenchConfig,
) {
    let generation = bench.cache.generation(ns);
    for vector in vectors {
        bench.service.semantic_cache().insert(
            ns,
            generation,
            vector,
            cfg.k,
            cfg.nprobes,
            result_bytes.to_vec(),
        );
    }
}

async fn measure_case<F>(
    bench: &BenchService,
    ns: &NamespaceId,
    reps: usize,
    mut make_req: F,
) -> anyhow::Result<CaseResult>
where
    F: FnMut(usize) -> QueryRequest,
{
    let before = snapshot(&bench.metrics, ns);
    let mut samples = Vec::with_capacity(reps);
    for i in 0..reps {
        let req = make_req(i);
        let start = Instant::now();
        let _ = bench.service.query(ns, &req).await?;
        samples.push(start.elapsed());
    }
    let after = snapshot(&bench.metrics, ns);
    Ok(CaseResult {
        samples: reps,
        latency: percentiles(samples),
        metrics: after.diff(before),
    })
}

async fn measure_semantic_miss_case(
    bench: &BenchService,
    ns: &NamespaceId,
    cfg: &BenchConfig,
    base_vector: &[f32],
    base_bytes: &[u8],
    miss_cosine: f32,
    seed_offset: usize,
) -> anyhow::Result<CaseResult> {
    let before = snapshot(&bench.metrics, ns);
    let mut samples = Vec::with_capacity(cfg.reps);
    for i in 0..cfg.reps {
        let req = vector_query(
            vector_at_cosine(base_vector, miss_cosine, seed_offset + i),
            cfg.k,
            cfg.nprobes,
            Some(cfg.threshold),
        );
        let start = Instant::now();
        let _ = bench.service.query(ns, &req).await?;
        samples.push(start.elapsed());

        bench.service.semantic_cache().invalidate(ns);
        seed_semantic_sidecar(bench, ns, vec![base_vector.to_vec()], base_bytes, cfg);
    }
    let after = snapshot(&bench.metrics, ns);
    Ok(CaseResult {
        samples: cfg.reps,
        latency: percentiles(samples),
        metrics: after.diff(before),
    })
}

fn percentiles(mut samples: Vec<Duration>) -> Percentiles {
    assert!(!samples.is_empty(), "no samples");
    samples.sort_unstable();
    let n = samples.len();
    Percentiles {
        p50: samples[(n * 50 / 100).min(n - 1)],
        p95: samples[(n * 95 / 100).min(n - 1)],
        p99: samples[(n * 99 / 100).min(n - 1)],
        max: *samples.last().unwrap(),
    }
}

fn snapshot(metrics: &CoreMetrics, ns: &NamespaceId) -> MetricSnapshot {
    let body = metrics.encode().unwrap_or_default();
    let namespace_label = format!(r#"namespace="{}""#, ns.as_str());
    let query_label = r#"operation="query""#;
    MetricSnapshot {
        cache_hits: metric_value(&body, "firnflow_cache_hits_total", &[&namespace_label]),
        cache_misses: metric_value(&body, "firnflow_cache_misses_total", &[&namespace_label]),
        semantic_hits: metrics.semantic_cache_hits_value(ns),
        semantic_misses: metrics.semantic_cache_misses_value(ns),
        semantic_empty: metrics.semantic_cache_rejections_value(ns, "empty_index"),
        backend_queries: metric_value(
            &body,
            "firnflow_s3_requests_total",
            &[&namespace_label, query_label],
        ),
    }
}

impl MetricSnapshot {
    fn diff(self, before: Self) -> MetricDelta {
        MetricDelta {
            cache_hits: self.cache_hits.saturating_sub(before.cache_hits),
            cache_misses: self.cache_misses.saturating_sub(before.cache_misses),
            semantic_hits: self.semantic_hits.saturating_sub(before.semantic_hits),
            semantic_misses: self.semantic_misses.saturating_sub(before.semantic_misses),
            semantic_empty: self.semantic_empty.saturating_sub(before.semantic_empty),
            backend_queries: self.backend_queries.saturating_sub(before.backend_queries),
        }
    }
}

fn metric_value(body: &str, metric: &str, label_needles: &[&str]) -> u64 {
    for line in body.lines() {
        if line.starts_with('#') || !line.starts_with(metric) {
            continue;
        }
        if label_needles.iter().any(|needle| !line.contains(needle)) {
            continue;
        }
        if let Some((_, value)) = line.rsplit_once(char::is_whitespace) {
            return value.parse::<f64>().map(|v| v as u64).unwrap_or(0);
        }
    }
    0
}

fn overlap_count(a: &QueryResultSet, b: &QueryResultSet) -> usize {
    let a_ids: HashSet<u64> = a.results.iter().map(|r| r.id).collect();
    b.results.iter().filter(|r| a_ids.contains(&r.id)).count()
}

async fn run_latency_tiers(
    manager: Arc<NamespaceManager>,
    ns: &NamespaceId,
    cfg: &BenchConfig,
    base_vector: &[f32],
    base_result: &QueryResultSet,
    base_bytes: &[u8],
) -> anyhow::Result<LatencyResults> {
    let hit_cosine = above_threshold(cfg.threshold);
    let below_cosine = below_threshold(cfg.threshold);
    let hit_probe = vector_at_cosine(base_vector, hit_cosine, 20_000);
    let true_hit_probe = true_query(&manager, ns, hit_probe.clone(), cfg).await?;
    let hit_overlap = overlap_count(base_result, &true_hit_probe);

    let cold_bench = build_bench_service(Arc::clone(&manager), cfg).await?;
    seed_semantic_sidecar(&cold_bench, ns, vec![base_vector.to_vec()], base_bytes, cfg);
    let cold_novel =
        measure_semantic_miss_case(&cold_bench, ns, cfg, base_vector, base_bytes, 0.0, 30_000)
            .await?;

    let foyer_bench = build_bench_service(Arc::clone(&manager), cfg).await?;
    let base_req = vector_query(base_vector.to_vec(), cfg.k, cfg.nprobes, None);
    let _ = foyer_bench.service.query(ns, &base_req).await?;
    let foyer_hit = measure_case(&foyer_bench, ns, cfg.reps, |_| {
        vector_query(base_vector.to_vec(), cfg.k, cfg.nprobes, None)
    })
    .await?;

    let semantic_hit_bench = build_bench_service(Arc::clone(&manager), cfg).await?;
    seed_semantic_sidecar(
        &semantic_hit_bench,
        ns,
        vec![base_vector.to_vec()],
        base_bytes,
        cfg,
    );
    let semantic_hit = measure_case(&semantic_hit_bench, ns, cfg.reps, |_| {
        vector_query(hit_probe.clone(), cfg.k, cfg.nprobes, Some(cfg.threshold))
    })
    .await?;

    let semantic_miss_bench = build_bench_service(manager, cfg).await?;
    seed_semantic_sidecar(
        &semantic_miss_bench,
        ns,
        vec![base_vector.to_vec()],
        base_bytes,
        cfg,
    );
    let semantic_miss_below = measure_semantic_miss_case(
        &semantic_miss_bench,
        ns,
        cfg,
        base_vector,
        base_bytes,
        below_cosine,
        40_000,
    )
    .await?;

    Ok(LatencyResults {
        cold_novel,
        foyer_hit,
        semantic_hit,
        semantic_miss_below,
        hit_cosine,
        below_cosine,
        hit_overlap,
    })
}

async fn run_threshold_cliff(
    manager: Arc<NamespaceManager>,
    ns: &NamespaceId,
    cfg: &BenchConfig,
    base_vector: &[f32],
    base_bytes: &[u8],
) -> anyhow::Result<Vec<ThresholdRow>> {
    let bench = build_bench_service(Arc::clone(&manager), cfg).await?;
    seed_semantic_sidecar(&bench, ns, vec![base_vector.to_vec()], base_bytes, cfg);

    let mut rows = Vec::with_capacity(cfg.cosine_sweep.len());
    for (idx, target) in cfg.cosine_sweep.iter().copied().enumerate() {
        let probe = vector_at_cosine(base_vector, target, 50_000 + idx);
        let observed = cosine(base_vector, &probe);
        let true_result = true_query(&manager, ns, probe.clone(), cfg).await?;
        let before = snapshot(&bench.metrics, ns);
        let result = bench
            .service
            .query(
                ns,
                &vector_query(probe, cfg.k, cfg.nprobes, Some(cfg.threshold)),
            )
            .await?;
        let after = snapshot(&bench.metrics, ns);
        let delta = after.diff(before);
        let outcome = if delta.semantic_hits > 0 {
            "semantic-hit"
        } else if delta.semantic_misses > 0 {
            "semantic-miss"
        } else if delta.semantic_empty > 0 {
            "empty-index"
        } else if delta.cache_hits > 0 {
            "exact-hit"
        } else {
            "backend"
        }
        .to_string();
        let overlap = overlap_count(&result, &true_result);
        rows.push(ThresholdRow {
            target_cosine: target,
            observed_cosine: observed,
            outcome,
            overlap,
        });
    }
    Ok(rows)
}

async fn run_scan_cost(
    manager: Arc<NamespaceManager>,
    ns: &NamespaceId,
    cfg: &BenchConfig,
    base_vector: &[f32],
    base_bytes: &[u8],
) -> anyhow::Result<Vec<ScanRow>> {
    let hit_cosine = above_threshold(cfg.threshold);
    let probe = vector_at_cosine(base_vector, hit_cosine, 60_000);
    let mut rows = Vec::with_capacity(cfg.fill_sizes.len());
    for fill in &cfg.fill_sizes {
        let bench = build_bench_service(Arc::clone(&manager), cfg).await?;
        let mut vectors = Vec::with_capacity(*fill);
        if *fill > 0 {
            vectors.push(base_vector.to_vec());
        }
        for i in 1..*fill {
            vectors.push(vector_at_cosine(base_vector, 0.0, 70_000 + i));
        }
        seed_semantic_sidecar(&bench, ns, vectors, base_bytes, cfg);
        let result = measure_case(&bench, ns, cfg.reps, |_| {
            vector_query(probe.clone(), cfg.k, cfg.nprobes, Some(cfg.threshold))
        })
        .await?;
        rows.push(ScanRow {
            entries: *fill,
            result,
        });
    }
    Ok(rows)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = BenchConfig::from_env()?;
    let start_utc = utc_timestamp();
    println!(
        "semantic cache profile: dim={}, rows={}, reps={}, k={}, nprobes={}, threshold={}, storage_root={}",
        cfg.dim, cfg.rows, cfg.reps, cfg.k, cfg.nprobes, cfg.threshold, cfg.storage_root
    );

    let setup_metrics = Arc::new(CoreMetrics::new()?);
    let manager = Arc::new(NamespaceManager::new(
        cfg.storage_root.clone(),
        cfg.storage_options.clone(),
        Arc::clone(&setup_metrics),
    ));
    let setup = build_bench_service(Arc::clone(&manager), &cfg).await?;
    let ns = NamespaceId::new(format!("bench-semantic-{}", ts_nanos()))?;

    println!("namespace: {ns}");
    println!("upsert: {} rows...", cfg.rows);
    let upsert_elapsed = upsert_rows(&setup.service, &ns, cfg.rows, cfg.dim).await?;
    println!("  completed in {:.1}s", upsert_elapsed.as_secs_f64());

    let num_partitions = default_num_partitions(cfg.rows);
    let num_sub_vectors = default_num_sub_vectors(cfg.dim);
    println!(
        "index build: IVF_PQ, partitions={}, sub_vectors={}...",
        num_partitions, num_sub_vectors
    );
    let index_start = Instant::now();
    setup
        .service
        .create_index(&ns, Some(num_partitions), Some(num_sub_vectors), None)
        .await?;
    let index_elapsed = index_start.elapsed();
    println!("  completed in {:.1}s", index_elapsed.as_secs_f64());

    let base_vector = make_vector(42, cfg.dim);
    let base_result = true_query(&manager, &ns, base_vector.clone(), &cfg).await?;
    let base_bytes = encode_result(&base_result)?;

    println!("latency tiers...");
    let latency = run_latency_tiers(
        Arc::clone(&manager),
        &ns,
        &cfg,
        &base_vector,
        &base_result,
        &base_bytes,
    )
    .await?;
    println!(
        "  cold p50={}, semantic-hit p50={}, foyer-hit p50={}",
        fmt_dur(latency.cold_novel.latency.p50),
        fmt_dur(latency.semantic_hit.latency.p50),
        fmt_dur(latency.foyer_hit.latency.p50)
    );

    println!("threshold cliff...");
    let threshold_rows =
        run_threshold_cliff(Arc::clone(&manager), &ns, &cfg, &base_vector, &base_bytes).await?;

    println!("scan cost...");
    let scan_rows =
        run_scan_cost(Arc::clone(&manager), &ns, &cfg, &base_vector, &base_bytes).await?;

    let stop_utc = utc_timestamp();
    let report = render_report(ReportInput {
        cfg: &cfg,
        ns: &ns,
        start_utc: &start_utc,
        stop_utc: &stop_utc,
        upsert_elapsed,
        index_elapsed,
        num_partitions,
        num_sub_vectors,
        latency: &latency,
        threshold_rows: &threshold_rows,
        scan_rows: &scan_rows,
    });

    if let Some(parent) = cfg.out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    fs::write(&cfg.out_path, report)
        .with_context(|| format!("writing {}", cfg.out_path.display()))?;
    println!("wrote {}", cfg.out_path.display());
    Ok(())
}

fn default_num_partitions(rows: usize) -> u32 {
    ((rows as f64).sqrt().round() as u32).max(1)
}

fn default_num_sub_vectors(dim: usize) -> u32 {
    let target = (dim / 16).max(1);
    for candidate in (1..=target).rev() {
        if dim % candidate == 0 {
            return candidate as u32;
        }
    }
    1
}

fn render_report(input: ReportInput<'_>) -> String {
    let cfg = input.cfg;
    let latency = input.latency;
    let storage_mb = (cfg.rows as f64 * cfg.dim as f64 * 4.0) / (1024.0 * 1024.0);
    let semantic_speedup = latency.cold_novel.latency.p50.as_secs_f64()
        / latency.semantic_hit.latency.p50.as_secs_f64();
    let foyer_speedup =
        latency.cold_novel.latency.p50.as_secs_f64() / latency.foyer_hit.latency.p50.as_secs_f64();

    let mut out = format!(
        "# Semantic cache profile\n\
\n\
- **Start UTC**: {start_utc}\n\
- **Stop UTC**: {stop_utc}\n\
- **Harness**: `./scripts/cargo run --release -p firnflow-bench --bin semantic_cache_profile -j 1`\n\
- **Backend**: `{storage_root}`\n\
- **Namespace**: `{ns}`\n\
- **Config**: dim={dim}, rows={rows}, reps={reps}, k={k}, nprobes={nprobes}, threshold={threshold:.6}\n\
- **Storage**: ~{storage_mb:.1} MB raw vector data\n\
- **Cache**: RAM={cache_ram}MB, NVMe={cache_nvme}MB\n\
- **Upsert**: {upsert_secs:.1}s\n\
- **Index build**: IVF_PQ (partitions={num_partitions}, sub_vectors={num_sub_vectors}) in {index_secs:.1}s\n\
\n\
The benchmark uses deterministic normalized synthetic vectors and drives `NamespaceService` in-process. Semantic-hit cases seed the sidecar with a real encoded top-k result, then issue a near-duplicate query whose exact-cache key is different.\n\
\n\
## Latency by tier\n\
\n\
| case | samples | p50 | p95 | p99 | max | backend queries | semantic hits | semantic misses | exact hits | exact misses |\n\
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
        start_utc = input.start_utc,
        stop_utc = input.stop_utc,
        storage_root = cfg.storage_root,
        ns = input.ns,
        dim = cfg.dim,
        rows = cfg.rows,
        reps = cfg.reps,
        k = cfg.k,
        nprobes = cfg.nprobes,
        threshold = cfg.threshold,
        storage_mb = storage_mb,
        cache_ram = cfg.cache_ram_bytes / (1024 * 1024),
        cache_nvme = cfg.cache_nvme_bytes / (1024 * 1024),
        upsert_secs = input.upsert_elapsed.as_secs_f64(),
        num_partitions = input.num_partitions,
        num_sub_vectors = input.num_sub_vectors,
        index_secs = input.index_elapsed.as_secs_f64(),
    );
    push_case_row(&mut out, "cold novel", &latency.cold_novel);
    push_case_row(&mut out, "foyer hit", &latency.foyer_hit);
    push_case_row(&mut out, "semantic hit", &latency.semantic_hit);
    push_case_row(
        &mut out,
        "semantic miss below threshold",
        &latency.semantic_miss_below,
    );

    out.push_str(&format!(
        "\n\
## Primary comparison\n\
\n\
| comparison | p50 ratio |\n\
| --- | ---: |\n\
| cold novel / semantic hit | {semantic_speedup:.1}x |\n\
| cold novel / foyer hit | {foyer_speedup:.1}x |\n\
\n\
The semantic-hit probe was generated at cosine {hit_cosine:.6} against the cached query. Its true backend top-k overlapped the reused cached top-k by {hit_overlap}/{k} ids.\n\
\n\
The below-threshold miss probe was generated at cosine {below_cosine:.6}. It should fall through to LanceDB at threshold {threshold:.6}; backend query counts in the latency table confirm that behavior.\n\
\n\
## Threshold cliff\n\
\n\
| target cosine | observed cosine | outcome | overlap vs true top-k |\n\
| ---: | ---: | --- | ---: |\n",
        semantic_speedup = semantic_speedup,
        foyer_speedup = foyer_speedup,
        hit_cosine = latency.hit_cosine,
        hit_overlap = latency.hit_overlap,
        k = cfg.k,
        below_cosine = latency.below_cosine,
        threshold = cfg.threshold,
    ));
    for row in input.threshold_rows {
        out.push_str(&format!(
            "| {target:.6} | {observed:.9} | {outcome} | {overlap}/{k} |\n",
            target = row.target_cosine,
            observed = row.observed_cosine,
            outcome = row.outcome,
            overlap = row.overlap,
            k = cfg.k,
        ));
    }

    out.push_str(
        "\n\
## Scan cost\n\
\n\
| sidecar entries | samples | p50 | p95 | p99 | max | semantic hits |\n\
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    for row in input.scan_rows {
        out.push_str(&format!(
            "| {entries} | {samples} | {p50} | {p95} | {p99} | {max} | {hits} |\n",
            entries = row.entries,
            samples = row.result.samples,
            p50 = fmt_dur(row.result.latency.p50),
            p95 = fmt_dur(row.result.latency.p95),
            p99 = fmt_dur(row.result.latency.p99),
            max = fmt_dur(row.result.latency.max),
            hits = row.result.metrics.semantic_hits,
        ));
    }

    out.push_str(
        "\n\
## Notes\n\
\n\
- The semantic sidecar is a linear scan over at most 1024 entries per namespace generation.\n\
- `backend queries` is the service-level S3-bound query counter, not raw HTTP request count.\n\
- Semantic hits return the cached neighbour's serialized top-k bytes. The overlap number is the quality check against a fresh backend query for the probe vector.\n",
    );
    out
}

fn push_case_row(out: &mut String, label: &str, result: &CaseResult) {
    out.push_str(&format!(
        "| {label} | {samples} | {p50} | {p95} | {p99} | {max} | {backend} | {sem_hits} | {sem_misses} | {exact_hits} | {exact_misses} |\n",
        label = label,
        samples = result.samples,
        p50 = fmt_dur(result.latency.p50),
        p95 = fmt_dur(result.latency.p95),
        p99 = fmt_dur(result.latency.p99),
        max = fmt_dur(result.latency.max),
        backend = result.metrics.backend_queries,
        sem_hits = result.metrics.semantic_hits,
        sem_misses = result.metrics.semantic_misses,
        exact_hits = result.metrics.cache_hits,
        exact_misses = result.metrics.cache_misses,
    ));
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_secs_f64() * 1_000_000.0;
    if us < 1000.0 {
        format!("{:.2} us", us)
    } else {
        let ms = us / 1000.0;
        if ms < 1000.0 {
            format!("{:.2} ms", ms)
        } else {
            format!("{:.2} s", d.as_secs_f64())
        }
    }
}

fn ts_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn utc_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let second_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u64, u64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
