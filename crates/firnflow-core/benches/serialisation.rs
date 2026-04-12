//! Spike-3 benchmark: serialisation round-trip for cached query
//! result sets.
//!
//! CLAUDE.md § "Known hard problems" 3:
//!
//! > Benchmark serialise + deserialise round-trip time against
//! > realistic result set sizes: 10 results, 100 results, 1000
//! > results, each with a 1536-dim vector payload. If bincode
//! > round-trip exceeds 1 ms at p99 for 100-result sets, evaluate
//! > rkyv and flatbuffers as alternatives.
//!
//! `harness = false` in Cargo.toml means this file is compiled as a
//! plain binary with its own `main`, which lets us hand-roll exact
//! p50/p95/p99 measurement — criterion's default stdout doesn't
//! print p99, and CLAUDE.md gates the design decision on p99
//! specifically.
//!
//! Run with:
//!
//! ```text
//! ./scripts/cargo bench -p firnflow-core --bench serialisation
//! ```

use std::time::{Duration, Instant};

use bincode::config::{self, Configuration};
use firnflow_core::{QueryResult, QueryResultSet};

const VECTOR_DIM: usize = 1536;
const WARMUP_ITERS: usize = 200;
const SAMPLE_ITERS: usize = 2000;

fn make_result_set(n: usize) -> QueryResultSet {
    let results: Vec<QueryResult> = (0..n)
        .map(|i| QueryResult {
            id: i as u64,
            score: 1.0 - (i as f32) * 0.001,
            // Deterministic but varied — not all-zero, so any run of
            // the serialiser that might special-case sparse/uniform
            // data is still exercised against realistic-ish values.
            vector: (0..VECTOR_DIM)
                .map(|j| ((i * 7 + j * 13) as f32) * 0.0001)
                .collect(),
        })
        .collect();
    QueryResultSet {
        query_id: format!("bench-query-{n}"),
        results,
    }
}

struct Percentiles {
    p50: Duration,
    p95: Duration,
    p99: Duration,
    max: Duration,
}

fn percentiles(mut samples: Vec<Duration>) -> Percentiles {
    samples.sort_unstable();
    let n = samples.len();
    Percentiles {
        p50: samples[n / 2],
        p95: samples[(n * 95) / 100],
        p99: samples[(n * 99) / 100],
        max: *samples.last().unwrap(),
    }
}

fn bench_encode(rs: &QueryResultSet, cfg: Configuration) -> Percentiles {
    for _ in 0..WARMUP_ITERS {
        let _ = bincode::serde::encode_to_vec(rs, cfg).unwrap();
    }
    let mut samples = Vec::with_capacity(SAMPLE_ITERS);
    for _ in 0..SAMPLE_ITERS {
        let start = Instant::now();
        let bytes = bincode::serde::encode_to_vec(rs, cfg).unwrap();
        samples.push(start.elapsed());
        std::hint::black_box(bytes);
    }
    percentiles(samples)
}

fn bench_decode(bytes: &[u8], cfg: Configuration) -> Percentiles {
    for _ in 0..WARMUP_ITERS {
        let _: (QueryResultSet, usize) = bincode::serde::decode_from_slice(bytes, cfg).unwrap();
    }
    let mut samples = Vec::with_capacity(SAMPLE_ITERS);
    for _ in 0..SAMPLE_ITERS {
        let start = Instant::now();
        let (decoded, _): (QueryResultSet, usize) =
            bincode::serde::decode_from_slice(bytes, cfg).unwrap();
        samples.push(start.elapsed());
        std::hint::black_box(decoded);
    }
    percentiles(samples)
}

fn bench_round_trip(rs: &QueryResultSet, cfg: Configuration) -> Percentiles {
    for _ in 0..WARMUP_ITERS {
        let bytes = bincode::serde::encode_to_vec(rs, cfg).unwrap();
        let _: (QueryResultSet, usize) = bincode::serde::decode_from_slice(&bytes, cfg).unwrap();
    }
    let mut samples = Vec::with_capacity(SAMPLE_ITERS);
    for _ in 0..SAMPLE_ITERS {
        let start = Instant::now();
        let bytes = bincode::serde::encode_to_vec(rs, cfg).unwrap();
        let (decoded, _): (QueryResultSet, usize) =
            bincode::serde::decode_from_slice(&bytes, cfg).unwrap();
        samples.push(start.elapsed());
        std::hint::black_box(decoded);
    }
    percentiles(samples)
}

fn fmt_us(d: Duration) -> String {
    format!("{:>9.2} µs", d.as_secs_f64() * 1_000_000.0)
}

fn print_row(label: &str, size: usize, p: &Percentiles) {
    println!(
        "{label:<12} {size:>10}  {}  {}  {}  {}",
        fmt_us(p.p50),
        fmt_us(p.p95),
        fmt_us(p.p99),
        fmt_us(p.max),
    );
}

fn main() {
    let cfg = config::standard();

    println!();
    println!(
        "bincode 2 (serde path) — 1536-dim f32 vectors, {WARMUP_ITERS} warmup / {SAMPLE_ITERS} samples"
    );
    println!(
        "{:<12} {:>10}  {:>12}  {:>12}  {:>12}  {:>12}",
        "phase", "bytes", "p50", "p95", "p99", "max"
    );
    println!("{}", "-".repeat(78));

    for &n in &[10usize, 100, 1000] {
        let rs = make_result_set(n);
        let encoded = bincode::serde::encode_to_vec(&rs, cfg).unwrap();
        let size = encoded.len();

        let label = format!("encode/{n}");
        let p = bench_encode(&rs, cfg);
        print_row(&label, size, &p);

        let label = format!("decode/{n}");
        let p = bench_decode(&encoded, cfg);
        print_row(&label, size, &p);

        let label = format!("rt/{n}");
        let p = bench_round_trip(&rs, cfg);
        print_row(&label, size, &p);

        println!();
    }
}
