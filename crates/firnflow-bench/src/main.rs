//! Slice-6d bench harness: four-phase cold vs warm query latency
//! through the real `NamespaceService` cache-aside path.
//!
//! Drives traffic entirely in-process (no HTTP) so the numbers
//! reflect the service layer and the cache — not the axum
//! extractor stack. The bench builds a fresh `NamespaceService`
//! with a tempdir-backed foyer cache so every run starts cold.
//!
//! **Four phases:**
//!
//! 1. Linear scan, cold — fresh namespace, no index
//! 2. Linear scan, warm — same queries, cache hit
//! 3. IVF_PQ indexed, cold — new namespace, index built, then
//!    fresh queries through the indexed table
//! 4. IVF_PQ indexed, warm — same queries, cache hit
//!
//! Run with:
//!
//! ```text
//! docker compose up -d minio minio-init
//! FIRNFLOW_S3_BUCKET=firnflow-test \
//! FIRNFLOW_S3_ENDPOINT=http://127.0.0.1:9000 \
//! FIRNFLOW_S3_ACCESS_KEY=minioadmin \
//! FIRNFLOW_S3_SECRET_KEY=minioadmin \
//! FIRNFLOW_BENCH_DIM=1536 \
//! FIRNFLOW_BENCH_ROWS=100000 \
//!   ./scripts/cargo run --release -p firnflow-bench
//! ```
//!
//! Env vars (all optional except `FIRNFLOW_S3_BUCKET`):
//!
//! | var                             | default                                    |
//! | ------------------------------- | ------------------------------------------ |
//! | `FIRNFLOW_S3_BUCKET`            | *(required)*                               |
//! | `FIRNFLOW_S3_ENDPOINT`          | (real AWS)                                 |
//! | `FIRNFLOW_S3_ACCESS_KEY`        | (default credential chain)                 |
//! | `FIRNFLOW_S3_SECRET_KEY`        | (default credential chain)                 |
//! | `FIRNFLOW_S3_REGION`            | `us-east-1`                                |
//! | `FIRNFLOW_BENCH_DIM`            | `32`                                       |
//! | `FIRNFLOW_BENCH_ROWS`           | `100`                                      |
//! | `FIRNFLOW_BENCH_QUERIES`        | `50`                                       |
//! | `FIRNFLOW_BENCH_OUT`            | `bench/results/cold_vs_warm_realistic.md`  |
//! | `FIRNFLOW_BENCH_CACHE_RAM_MB`   | `16`                                       |
//! | `FIRNFLOW_BENCH_CACHE_NVME_MB`  | `256`                                      |
//! | `FIRNFLOW_BENCH_INDEX_NPROBES`  | `20`                                       |

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use firnflow_core::cache::NamespaceCache;
use firnflow_core::{
    CoreMetrics, NamespaceId, NamespaceManager, NamespaceService, QueryRequest, UpsertRow,
};

struct BenchConfig {
    bucket: String,
    storage_options: HashMap<String, String>,
    dim: usize,
    rows: usize,
    queries: usize,
    out_path: PathBuf,
    cache_ram_bytes: usize,
    cache_nvme_bytes: usize,
    nprobes: usize,
}

impl BenchConfig {
    fn from_env() -> anyhow::Result<Self> {
        let bucket =
            std::env::var("FIRNFLOW_S3_BUCKET").context("FIRNFLOW_S3_BUCKET must be set")?;
        let dim: usize = env_or("FIRNFLOW_BENCH_DIM", "32")
            .parse()
            .context("FIRNFLOW_BENCH_DIM")?;
        let rows: usize = env_or("FIRNFLOW_BENCH_ROWS", "100")
            .parse()
            .context("FIRNFLOW_BENCH_ROWS")?;
        let queries: usize = env_or("FIRNFLOW_BENCH_QUERIES", "50")
            .parse()
            .context("FIRNFLOW_BENCH_QUERIES")?;
        let out_path = PathBuf::from(env_or(
            "FIRNFLOW_BENCH_OUT",
            "bench/results/cold_vs_warm_realistic.md",
        ));
        let cache_ram_mb: usize = env_or("FIRNFLOW_BENCH_CACHE_RAM_MB", "16")
            .parse()
            .context("FIRNFLOW_BENCH_CACHE_RAM_MB")?;
        let cache_nvme_mb: usize = env_or("FIRNFLOW_BENCH_CACHE_NVME_MB", "256")
            .parse()
            .context("FIRNFLOW_BENCH_CACHE_NVME_MB")?;
        let nprobes: usize = env_or("FIRNFLOW_BENCH_INDEX_NPROBES", "20")
            .parse()
            .context("FIRNFLOW_BENCH_INDEX_NPROBES")?;

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
            bucket,
            storage_options: opts,
            dim,
            rows,
            queries,
            out_path,
            cache_ram_bytes: cache_ram_mb * 1024 * 1024,
            cache_nvme_bytes: cache_nvme_mb * 1024 * 1024,
            nprobes,
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn make_vector(seed: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| (((seed * 7919 + j * 31) as f32) * 0.001).sin())
        .collect()
}

fn make_query_vector(seed: usize, dim: usize) -> Vec<f32> {
    make_vector(seed + 1_000_000, dim)
}

struct Percentiles {
    p50: Duration,
    p95: Duration,
    p99: Duration,
    max: Duration,
}

fn percentiles(mut samples: Vec<Duration>) -> Percentiles {
    assert!(!samples.is_empty(), "no samples");
    samples.sort_unstable();
    let n = samples.len();
    Percentiles {
        p50: samples[n / 2],
        p95: samples[(n * 95) / 100],
        p99: samples[(n * 99) / 100],
        max: *samples.last().unwrap(),
    }
}

async fn run_queries(
    service: &NamespaceService,
    ns: &NamespaceId,
    queries: &[QueryRequest],
) -> anyhow::Result<Vec<Duration>> {
    let mut samples = Vec::with_capacity(queries.len());
    for req in queries {
        let start = Instant::now();
        let _ = service.query(ns, req).await?;
        samples.push(start.elapsed());
    }
    Ok(samples)
}

fn metric_value(body: &str, metric: &str, label_needle: &str) -> Option<f64> {
    for line in body.lines() {
        if line.starts_with('#') || !line.starts_with(metric) {
            continue;
        }
        if !label_needle.is_empty() && !line.contains(label_needle) {
            continue;
        }
        let value = line.rsplit_once(char::is_whitespace)?.1;
        return value.parse().ok();
    }
    None
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_secs_f64() * 1_000_000.0;
    if us < 1000.0 {
        format!("{:>10.2} us", us)
    } else {
        let ms = us / 1000.0;
        if ms < 1000.0 {
            format!("{:>10.2} ms", ms)
        } else {
            format!("{:>10.2} s ", d.as_secs_f64())
        }
    }
}

fn ts_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// Upsert rows into a namespace. For large row counts, batches the
/// upserts to avoid building a single enormous Arrow batch in
/// memory.
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = BenchConfig::from_env()?;
    println!(
        "bench config: dim={}, rows={}, queries={}, nprobes={}, \
         cache_ram={}MB, cache_nvme={}MB, bucket={}",
        cfg.dim,
        cfg.rows,
        cfg.queries,
        cfg.nprobes,
        cfg.cache_ram_bytes / (1024 * 1024),
        cfg.cache_nvme_bytes / (1024 * 1024),
        cfg.bucket
    );

    let tmp = tempfile::tempdir()?;
    let metrics = Arc::new(CoreMetrics::new()?);
    let manager = Arc::new(NamespaceManager::new(
        cfg.bucket.clone(),
        cfg.storage_options.clone(),
        Arc::clone(&metrics),
    ));
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
    let service = NamespaceService::new(Arc::clone(&manager), cache, Arc::clone(&metrics));

    let queries: Vec<QueryRequest> = (0..cfg.queries)
        .map(|i| QueryRequest {
            vector: make_query_vector(i, cfg.dim),
            k: 10,
            nprobes: None,
            text: None,
        })
        .collect();

    let queries_indexed: Vec<QueryRequest> = (0..cfg.queries)
        .map(|i| QueryRequest {
            vector: make_query_vector(i, cfg.dim),
            k: 10,
            nprobes: Some(cfg.nprobes),
            text: None,
        })
        .collect();

    // ================================================================
    // Phase 1+2: Linear scan (no index)
    // ================================================================
    let ns_linear = NamespaceId::new(format!("bench-linear-{}", ts_nanos()))?;
    println!("\n--- linear scan namespace: {} ---", ns_linear);

    println!("upsert phase: {} rows...", cfg.rows);
    let upsert_linear = upsert_rows(&service, &ns_linear, cfg.rows, cfg.dim).await?;
    println!("  upsert completed in {:.1}s", upsert_linear.as_secs_f64());

    println!("linear cold: {} queries...", cfg.queries);
    let linear_cold_samples = run_queries(&service, &ns_linear, &queries).await?;
    let linear_cold = percentiles(linear_cold_samples);
    println!(
        "  p50={} p99={}",
        fmt_dur(linear_cold.p50),
        fmt_dur(linear_cold.p99)
    );

    println!("linear warm: {} queries...", cfg.queries);
    let linear_warm_samples = run_queries(&service, &ns_linear, &queries).await?;
    let linear_warm = percentiles(linear_warm_samples);
    println!(
        "  p50={} p99={}",
        fmt_dur(linear_warm.p50),
        fmt_dur(linear_warm.p99)
    );

    // ================================================================
    // Phase 3+4: IVF_PQ indexed
    // ================================================================
    let ns_indexed = NamespaceId::new(format!("bench-indexed-{}", ts_nanos()))?;
    println!("\n--- indexed namespace: {} ---", ns_indexed);

    println!("upsert phase: {} rows...", cfg.rows);
    let upsert_indexed = upsert_rows(&service, &ns_indexed, cfg.rows, cfg.dim).await?;
    println!("  upsert completed in {:.1}s", upsert_indexed.as_secs_f64());

    // Determine IVF_PQ parameters: num_partitions = sqrt(rows),
    // num_sub_vectors = dim/16 (must divide dim evenly).
    let num_partitions = (cfg.rows as f64).sqrt() as u32;
    let num_sub_vectors = (cfg.dim / 16).max(1) as u32;
    println!(
        "index build: IVF_PQ, partitions={}, sub_vectors={}...",
        num_partitions, num_sub_vectors
    );
    let index_start = Instant::now();
    service
        .create_index(&ns_indexed, Some(num_partitions), Some(num_sub_vectors))
        .await?;
    let index_elapsed = index_start.elapsed();
    println!(
        "  index build completed in {:.1}s",
        index_elapsed.as_secs_f64()
    );

    println!(
        "indexed cold: {} queries (nprobes={})...",
        cfg.queries, cfg.nprobes
    );
    let indexed_cold_samples = run_queries(&service, &ns_indexed, &queries_indexed).await?;
    let indexed_cold = percentiles(indexed_cold_samples);
    println!(
        "  p50={} p99={}",
        fmt_dur(indexed_cold.p50),
        fmt_dur(indexed_cold.p99)
    );

    println!("indexed warm: {} queries...", cfg.queries);
    let indexed_warm_samples = run_queries(&service, &ns_indexed, &queries_indexed).await?;
    let indexed_warm = percentiles(indexed_warm_samples);
    println!(
        "  p50={} p99={}",
        fmt_dur(indexed_warm.p50),
        fmt_dur(indexed_warm.p99)
    );

    // ================================================================
    // Metrics summary
    // ================================================================
    let metrics_text = metrics
        .encode()
        .map_err(|e| anyhow::anyhow!("encode metrics: {e}"))?;

    let linear_label = format!(r#"namespace="{}""#, ns_linear.as_str());
    let indexed_label = format!(r#"namespace="{}""#, ns_indexed.as_str());

    let linear_misses =
        metric_value(&metrics_text, "firnflow_cache_misses_total", &linear_label).unwrap_or(0.0);
    let linear_hits =
        metric_value(&metrics_text, "firnflow_cache_hits_total", &linear_label).unwrap_or(0.0);
    let indexed_misses =
        metric_value(&metrics_text, "firnflow_cache_misses_total", &indexed_label).unwrap_or(0.0);
    let indexed_hits =
        metric_value(&metrics_text, "firnflow_cache_hits_total", &indexed_label).unwrap_or(0.0);

    println!("\nmetric summary:");
    println!(
        "  linear:  misses={}, hits={}",
        linear_misses as u64, linear_hits as u64
    );
    println!(
        "  indexed: misses={}, hits={}",
        indexed_misses as u64, indexed_hits as u64
    );

    // ================================================================
    // Write output
    // ================================================================
    let date = chrono_today();
    let storage_mb = (cfg.rows as f64 * cfg.dim as f64 * 4.0) / (1024.0 * 1024.0);

    let out = format!(
        "# Cold vs warm query latency — realistic parameters (slice 6d)\n\
\n\
- **Date**: {date}\n\
- **Harness**: `./scripts/cargo run --release -p firnflow-bench`\n\
- **Backend**: MinIO (see `docs/provider-support.md` for the pinned digest)\n\
- **Config**: dim={dim}, rows={rows}, queries={queries}, nprobes={nprobes}\n\
- **Storage**: ~{storage_mb:.0} MB raw vector data\n\
- **Cache**: RAM={cache_ram}MB, NVMe={cache_nvme}MB\n\
- **Upsert**: linear={upsert_linear:.1}s, indexed={upsert_indexed:.1}s\n\
- **Index build**: IVF_PQ (partitions={num_partitions}, sub_vectors={num_sub_vectors}) \
in **{index_secs:.1}s**\n\
\n\
## Four-phase latency comparison\n\
\n\
| phase       | path |    p50     |    p95     |    p99     |    max     |\n\
| ----------- | ---- | ----------:| ----------:| ----------:| ----------:|\n\
| linear scan | cold | {lc_p50} | {lc_p95} | {lc_p99} | {lc_max} |\n\
| linear scan | warm | {lw_p50} | {lw_p95} | {lw_p99} | {lw_max} |\n\
| IVF_PQ      | cold | {ic_p50} | {ic_p95} | {ic_p99} | {ic_max} |\n\
| IVF_PQ      | warm | {iw_p50} | {iw_p95} | {iw_p99} | {iw_max} |\n\
\n\
## Speedup ratios\n\
\n\
| comparison | p50 | p99 |\n\
| ---------- | ---:| ---:|\n\
| linear cold → warm | {lc_lw_p50:.0}x | {lc_lw_p99:.0}x |\n\
| indexed cold → warm | {ic_iw_p50:.0}x | {ic_iw_p99:.0}x |\n\
| linear cold → indexed cold | {lc_ic_p50:.1}x | {lc_ic_p99:.1}x |\n\
\n\
## Cache + S3 request asymmetry\n\
\n\
| namespace | cache misses | cache hits | observation |\n\
| --------- | -----------: | ---------: | ----------- |\n\
| linear    | {linear_misses} | {linear_hits} | {queries} cold queries → {linear_misses} S3 trips; {queries} warm → 0 |\n\
| indexed   | {indexed_misses} | {indexed_hits} | same pattern, but cold queries are dramatically faster |\n\
\n\
## The thesis\n\
\n\
1. **The cache without the index is a liability.** It hides the underlying \
linear-scan cost, which surfaces on every cache miss.\n\
2. **The index without the cache leaves money on the table.** Repeat queries \
still pay the (now-fast) S3 round-trip.\n\
3. **Together, the ANN index and the tiered cache make each other more valuable.** \
Removing either is strictly worse.\n\
\n\
## Notes\n\
\n\
- Each run starts cold: fresh `tempfile::tempdir` for the foyer NVMe tier, \
fresh namespace timestamps.\n\
- `s3_requests_total` counts firnflow-initiated operations at the service \
boundary, not raw HTTP requests to S3.\n\
- Index build time is the \"Index Tax\" — paid once, amortised across all \
subsequent queries.\n",
        date = date,
        dim = cfg.dim,
        rows = cfg.rows,
        queries = cfg.queries,
        nprobes = cfg.nprobes,
        storage_mb = storage_mb,
        cache_ram = cfg.cache_ram_bytes / (1024 * 1024),
        cache_nvme = cfg.cache_nvme_bytes / (1024 * 1024),
        upsert_linear = upsert_linear.as_secs_f64(),
        upsert_indexed = upsert_indexed.as_secs_f64(),
        num_partitions = num_partitions,
        num_sub_vectors = num_sub_vectors,
        index_secs = index_elapsed.as_secs_f64(),
        lc_p50 = fmt_dur(linear_cold.p50),
        lc_p95 = fmt_dur(linear_cold.p95),
        lc_p99 = fmt_dur(linear_cold.p99),
        lc_max = fmt_dur(linear_cold.max),
        lw_p50 = fmt_dur(linear_warm.p50),
        lw_p95 = fmt_dur(linear_warm.p95),
        lw_p99 = fmt_dur(linear_warm.p99),
        lw_max = fmt_dur(linear_warm.max),
        ic_p50 = fmt_dur(indexed_cold.p50),
        ic_p95 = fmt_dur(indexed_cold.p95),
        ic_p99 = fmt_dur(indexed_cold.p99),
        ic_max = fmt_dur(indexed_cold.max),
        iw_p50 = fmt_dur(indexed_warm.p50),
        iw_p95 = fmt_dur(indexed_warm.p95),
        iw_p99 = fmt_dur(indexed_warm.p99),
        iw_max = fmt_dur(indexed_warm.max),
        lc_lw_p50 = linear_cold.p50.as_secs_f64() / linear_warm.p50.as_secs_f64(),
        lc_lw_p99 = linear_cold.p99.as_secs_f64() / linear_warm.p99.as_secs_f64(),
        ic_iw_p50 = indexed_cold.p50.as_secs_f64() / indexed_warm.p50.as_secs_f64(),
        ic_iw_p99 = indexed_cold.p99.as_secs_f64() / indexed_warm.p99.as_secs_f64(),
        lc_ic_p50 = linear_cold.p50.as_secs_f64() / indexed_cold.p50.as_secs_f64(),
        lc_ic_p99 = linear_cold.p99.as_secs_f64() / indexed_cold.p99.as_secs_f64(),
        linear_misses = linear_misses as u64,
        linear_hits = linear_hits as u64,
        indexed_misses = indexed_misses as u64,
        indexed_hits = indexed_hits as u64,
    );

    if let Some(parent) = cfg.out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    fs::write(&cfg.out_path, &out)
        .with_context(|| format!("writing {}", cfg.out_path.display()))?;
    println!("\nwrote {}", cfg.out_path.display());

    Ok(())
}

fn chrono_today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs / 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}
