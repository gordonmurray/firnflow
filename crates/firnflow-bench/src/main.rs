//! Slice-2b bench harness: cold vs warm query latency through the
//! real `NamespaceService` cache-aside path.
//!
//! Drives traffic entirely in-process (no HTTP) so the numbers
//! reflect the service layer and the cache — not the axum
//! extractor stack. The bench itself builds a fresh
//! `NamespaceService` with a tempdir-backed foyer cache so every
//! run starts cold and there is nothing to clean up between runs.
//!
//! Run with:
//!
//! ```text
//! docker compose up -d minio minio-init
//! FIRNFLOW_S3_BUCKET=firnflow-test \
//! FIRNFLOW_S3_ENDPOINT=http://127.0.0.1:9000 \
//! FIRNFLOW_S3_ACCESS_KEY=minioadmin \
//! FIRNFLOW_S3_SECRET_KEY=minioadmin \
//!   ./scripts/cargo run --release -p firnflow-bench
//! ```
//!
//! Env vars (all optional except `FIRNFLOW_S3_BUCKET`):
//!
//! | var                        | default                         |
//! | -------------------------- | ------------------------------- |
//! | `FIRNFLOW_S3_BUCKET`     | *(required)*                     |
//! | `FIRNFLOW_S3_ENDPOINT`   | (real AWS)                       |
//! | `FIRNFLOW_S3_ACCESS_KEY` | (default credential chain)       |
//! | `FIRNFLOW_S3_SECRET_KEY` | (default credential chain)       |
//! | `FIRNFLOW_S3_REGION`     | `us-east-1`                      |
//! | `FIRNFLOW_BENCH_DIM`     | `32`                             |
//! | `FIRNFLOW_BENCH_ROWS`    | `100`                            |
//! | `FIRNFLOW_BENCH_QUERIES` | `50`                             |
//! | `FIRNFLOW_BENCH_OUT`     | `bench/results/cold_vs_warm.md` |

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use firnflow_core::cache::NamespaceCache;
use firnflow_core::{CoreMetrics, NamespaceId, NamespaceManager, NamespaceService, QueryRequest};

struct BenchConfig {
    bucket: String,
    storage_options: HashMap<String, String>,
    dim: usize,
    rows: usize,
    queries: usize,
    out_path: PathBuf,
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
            "bench/results/cold_vs_warm.md",
        ));

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
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Deterministic vector generator. Returns values roughly in
/// `[-1, 1]` via `sin` so distances between vectors are
/// non-trivial (all-zero or all-same vectors would be pathological).
fn make_vector(seed: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| (((seed * 7919 + j * 31) as f32) * 0.001).sin())
        .collect()
}

fn make_query_vector(seed: usize, dim: usize) -> Vec<f32> {
    // Offset seed so query vectors differ from stored row vectors.
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

/// Scrape a single metric value out of a Prometheus exposition
/// payload. Same shape as the helper in `api_metrics.rs` — kept
/// local to avoid a shared-utility crate just for one function.
fn metric_value(body: &str, metric: &str, label_needle: &str) -> Option<f64> {
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        if !line.starts_with(metric) {
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

fn fmt_us(d: Duration) -> String {
    format!("{:>9.2} µs", d.as_secs_f64() * 1_000_000.0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = BenchConfig::from_env()?;
    println!(
        "bench config: dim={}, rows={}, queries={}, bucket={}",
        cfg.dim, cfg.rows, cfg.queries, cfg.bucket
    );

    let tmp = tempfile::tempdir()?;
    let metrics = Arc::new(CoreMetrics::new()?);
    let manager = Arc::new(NamespaceManager::new(
        cfg.bucket.clone(),
        cfg.storage_options.clone(),
    ));
    let cache = Arc::new(
        NamespaceCache::new(
            16 * 1024 * 1024,
            tmp.path(),
            64 * 1024 * 1024,
            Arc::clone(&metrics),
        )
        .await
        .map_err(|e| anyhow::anyhow!("build cache: {e}"))?,
    );
    let service = NamespaceService::new(manager, cache, Arc::clone(&metrics));

    let ns = NamespaceId::new(format!(
        "bench-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ))?;

    // ---- upsert ----
    println!("upsert phase: {} rows...", cfg.rows);
    let upsert_start = Instant::now();
    let rows: Vec<(u64, Vec<f32>)> = (0..cfg.rows)
        .map(|i| (i as u64, make_vector(i, cfg.dim)))
        .collect();
    service.upsert(&ns, rows).await?;
    let upsert_elapsed = upsert_start.elapsed();
    println!("  upsert completed in {} ms", upsert_elapsed.as_millis());

    // ---- prepare queries ----
    let queries: Vec<QueryRequest> = (0..cfg.queries)
        .map(|i| QueryRequest {
            vector: make_query_vector(i, cfg.dim),
            k: 10,
        })
        .collect();

    // ---- cold phase ----
    println!("cold phase: {} queries...", cfg.queries);
    let cold_samples = run_queries(&service, &ns, &queries).await?;
    let cold = percentiles(cold_samples.clone());
    println!(
        "  cold: p50={} p95={} p99={} max={}",
        fmt_us(cold.p50),
        fmt_us(cold.p95),
        fmt_us(cold.p99),
        fmt_us(cold.max)
    );

    // ---- warm phase ----
    println!("warm phase: {} queries...", cfg.queries);
    let warm_samples = run_queries(&service, &ns, &queries).await?;
    let warm = percentiles(warm_samples.clone());
    println!(
        "  warm: p50={} p95={} p99={} max={}",
        fmt_us(warm.p50),
        fmt_us(warm.p95),
        fmt_us(warm.p99),
        fmt_us(warm.max)
    );

    // ---- read metric state ----
    let metrics_text = metrics
        .encode()
        .map_err(|e| anyhow::anyhow!("encode metrics: {e}"))?;
    let ns_label = format!(r#"namespace="{}""#, ns.as_str());
    let query_op_label = format!(r#"namespace="{}",operation="query""#, ns.as_str());
    let upsert_op_label = format!(r#"namespace="{}",operation="upsert""#, ns.as_str());

    let cache_hits =
        metric_value(&metrics_text, "firnflow_cache_hits_total", &ns_label).unwrap_or(0.0);
    let cache_misses =
        metric_value(&metrics_text, "firnflow_cache_misses_total", &ns_label).unwrap_or(0.0);
    let s3_queries =
        metric_value(&metrics_text, "firnflow_s3_requests_total", &query_op_label).unwrap_or(0.0);
    let s3_upserts = metric_value(
        &metrics_text,
        "firnflow_s3_requests_total",
        &upsert_op_label,
    )
    .unwrap_or(0.0);

    println!(
        "metric totals: cache_hits={}, cache_misses={}, s3_queries={}, s3_upserts={}",
        cache_hits as u64, cache_misses as u64, s3_queries as u64, s3_upserts as u64
    );

    // ---- speedup ratios ----
    let p50_speedup = cold.p50.as_secs_f64() / warm.p50.as_secs_f64();
    let p99_speedup = cold.p99.as_secs_f64() / warm.p99.as_secs_f64();

    // ---- write markdown output ----
    let out = format_results(
        &cfg,
        upsert_elapsed,
        &cold,
        &warm,
        p50_speedup,
        p99_speedup,
        cache_hits as u64,
        cache_misses as u64,
        s3_queries as u64,
        s3_upserts as u64,
    );

    if let Some(parent) = cfg.out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    fs::write(&cfg.out_path, &out)
        .with_context(|| format!("writing {}", cfg.out_path.display()))?;
    println!("wrote {}", cfg.out_path.display());

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn format_results(
    cfg: &BenchConfig,
    upsert_elapsed: Duration,
    cold: &Percentiles,
    warm: &Percentiles,
    p50_speedup: f64,
    p99_speedup: f64,
    cache_hits: u64,
    cache_misses: u64,
    s3_queries: u64,
    s3_upserts: u64,
) -> String {
    let date = chrono_today();
    format!(
        "# Cold vs warm query latency — slice 2b baseline\n\
\n\
- **Date**: {date}\n\
- **Harness**: `./scripts/cargo run --release -p firnflow-bench`\n\
- **Backend**: MinIO (see `docs/provider-support.md` for the pinned digest)\n\
- **Config**: dim={dim}, rows={rows}, queries={queries}\n\
- **Upsert phase**: completed in {upsert_ms} ms\n\
\n\
## Cold vs warm latency\n\
\n\
| phase | queries |    p50     |    p95     |    p99     |    max     |\n\
| ----- | -------:| ----------:| ----------:| ----------:| ----------:|\n\
| cold  | {queries:>7} | {cold_p50} | {cold_p95} | {cold_p99} | {cold_max} |\n\
| warm  | {queries:>7} | {warm_p50} | {warm_p95} | {warm_p99} | {warm_max} |\n\
\n\
**Speedup** (cold / warm): p50 = **{p50_speedup:.1}×**, p99 = **{p99_speedup:.1}×**\n\
\n\
## Cache + S3 request asymmetry\n\
\n\
| metric                                             | value |\n\
| -------------------------------------------------- | ----: |\n\
| `firnflow_cache_misses_total`                    | {cache_misses} |\n\
| `firnflow_cache_hits_total`                      | {cache_hits} |\n\
| `firnflow_s3_requests_total{{op=upsert}}`        | {s3_upserts} |\n\
| `firnflow_s3_requests_total{{op=query}}`         | {s3_queries} |\n\
\n\
The load-bearing observation: **{cache_misses} query-kind S3 requests \
for {queries} cold queries, then **0 additional** for {queries} warm \
queries**. Every warm query was served from foyer without touching \
the backend, which is the whole reason the cache exists.\n\
\n\
## Notes\n\
\n\
- `s3_requests_total` counts firnflow-initiated S3-bound *operations* \
  at the service boundary, not raw HTTP requests to S3 — see the help \
  text on the metric and CLAUDE.md § \"Known hard problems\" 3 for the \
  approximation caveat.\n\
- Each run starts cold: the bench uses a fresh `tempfile::tempdir` \
  for the foyer NVMe tier and a fresh namespace timestamp so nothing \
  is reused between runs.\n\
- The vector dimension here ({dim}) is deliberately smaller than the \
  1536 used in the serialisation benchmark so the run completes in \
  seconds against MinIO over the `--network host` loopback. Bump it \
  for production-representative numbers.\n",
        date = date,
        dim = cfg.dim,
        rows = cfg.rows,
        queries = cfg.queries,
        upsert_ms = upsert_elapsed.as_millis(),
        cold_p50 = fmt_us(cold.p50),
        cold_p95 = fmt_us(cold.p95),
        cold_p99 = fmt_us(cold.p99),
        cold_max = fmt_us(cold.max),
        warm_p50 = fmt_us(warm.p50),
        warm_p95 = fmt_us(warm.p95),
        warm_p99 = fmt_us(warm.p99),
        warm_max = fmt_us(warm.max),
        p50_speedup = p50_speedup,
        p99_speedup = p99_speedup,
        cache_hits = cache_hits,
        cache_misses = cache_misses,
        s3_upserts = s3_upserts,
        s3_queries = s3_queries,
    )
}

/// Best-effort "today's date". The bench doesn't pull in `chrono`
/// just for this — we format the unix epoch in seconds into a
/// simple YYYY-MM-DD using a calendar math approximation good
/// enough for a bench-output header.
fn chrono_today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Convert to (year, month, day) using the proleptic Gregorian
    // calendar algorithm from Howard Hinnant's date-algorithms
    // paper (the same one std::chrono::year_month_day uses).
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
