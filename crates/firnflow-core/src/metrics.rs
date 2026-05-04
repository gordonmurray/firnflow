//! Prometheus metric handles plumbed through the library layers.
//!
//! [`CoreMetrics`] owns a `prometheus::Registry` plus typed counter,
//! histogram and gauge handles for the exact metric set CLAUDE.md
//! names:
//!
//! * `firnflow_cache_hits_total{namespace}`
//! * `firnflow_cache_misses_total{namespace}`
//! * `firnflow_query_duration_seconds{namespace, query_type}`
//! * `firnflow_write_duration_seconds{namespace}`
//! * `firnflow_active_namespaces`
//! * `firnflow_s3_requests_total{namespace, operation}`
//! * `firnflow_cached_handles`
//! * `firnflow_auth_rejections_total{reason}`
//!
//! Constructed once at process start (in
//! `firnflow-api::state::build_state`), wrapped in `Arc`, and
//! threaded into `NamespaceCache::new` and `NamespaceService::new`.
//! Both layers record directly against their own `Arc<CoreMetrics>`
//! — callers never thread a hit/miss outcome through the API.
//!
//! `s3_requests_total` is tracked at the service boundary (one
//! increment per upsert, one per cache-miss query), which is an
//! intentional *approximation*: the raw lance-io call count isn't
//! exposed, and the purpose of this metric is to answer "did the
//! cache save an S3 trip", not "how many S3 TCP connections
//! happened". Help text documents the approximation.

use std::sync::Arc;

use dashmap::DashSet;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};

use crate::{FirnflowError, NamespaceId};

/// Process-wide metrics registry and typed handles.
///
/// Cheaply cloneable as an `Arc<CoreMetrics>`; internally the
/// prometheus crate's types are already `Send + Sync` with cheap
/// interior mutability.
pub struct CoreMetrics {
    registry: Registry,
    cache_hits: IntCounterVec,
    cache_misses: IntCounterVec,
    query_duration: HistogramVec,
    write_duration: HistogramVec,
    active_namespaces: IntGauge,
    s3_requests: IntCounterVec,
    index_build_duration: HistogramVec,
    compaction_duration: HistogramVec,
    cached_handles: IntGauge,
    auth_rejections: IntCounterVec,
    seen_namespaces: DashSet<NamespaceId>,
}

impl CoreMetrics {
    /// Build a fresh registry and register every metric family.
    ///
    /// Fails only if prometheus rejects one of the metric
    /// definitions, which in practice means a programming error
    /// (duplicate name, malformed opts) — not a runtime failure.
    pub fn new() -> Result<Self, FirnflowError> {
        let registry = Registry::new();

        let cache_hits = IntCounterVec::new(
            Opts::new(
                "firnflow_cache_hits_total",
                "Cache hits, keyed by namespace.",
            ),
            &["namespace"],
        )
        .map_err(metrics_err)?;
        registry
            .register(Box::new(cache_hits.clone()))
            .map_err(metrics_err)?;

        let cache_misses = IntCounterVec::new(
            Opts::new(
                "firnflow_cache_misses_total",
                "Cache misses, keyed by namespace.",
            ),
            &["namespace"],
        )
        .map_err(metrics_err)?;
        registry
            .register(Box::new(cache_misses.clone()))
            .map_err(metrics_err)?;

        let query_duration = HistogramVec::new(
            HistogramOpts::new(
                "firnflow_query_duration_seconds",
                "End-to-end query latency, including cache lookup \
                 and any cache-miss backend round-trip.",
            ),
            &["namespace", "query_type"],
        )
        .map_err(metrics_err)?;
        registry
            .register(Box::new(query_duration.clone()))
            .map_err(metrics_err)?;

        let write_duration = HistogramVec::new(
            HistogramOpts::new(
                "firnflow_write_duration_seconds",
                "End-to-end upsert latency, including the cache \
                 invalidation step.",
            ),
            &["namespace"],
        )
        .map_err(metrics_err)?;
        registry
            .register(Box::new(write_duration.clone()))
            .map_err(metrics_err)?;

        let active_namespaces = IntGauge::new(
            "firnflow_active_namespaces",
            "Distinct namespaces touched by this process since startup.",
        )
        .map_err(metrics_err)?;
        registry
            .register(Box::new(active_namespaces.clone()))
            .map_err(metrics_err)?;

        let s3_requests = IntCounterVec::new(
            Opts::new(
                "firnflow_s3_requests_total",
                "firnflow-initiated operations that bypassed the cache \
                 and would have issued S3 traffic. Counted at the service \
                 boundary (one per upsert, one per cache-miss query), not \
                 at raw `object_store` call granularity — this is the \
                 signal for 'did the cache save a round-trip', not a \
                 faithful count of HTTP requests to S3.",
            ),
            &["namespace", "operation"],
        )
        .map_err(metrics_err)?;
        registry
            .register(Box::new(s3_requests.clone()))
            .map_err(metrics_err)?;

        let index_build_duration = HistogramVec::new(
            HistogramOpts::new(
                "firnflow_index_build_duration_seconds",
                "Time to build a vector index. The 'Index Tax' — \
                 operators pay this once per index build in exchange \
                 for dramatically faster queries.",
            )
            .buckets(vec![1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0]),
            &["namespace", "kind"],
        )
        .map_err(metrics_err)?;
        registry
            .register(Box::new(index_build_duration.clone()))
            .map_err(metrics_err)?;

        let compaction_duration = HistogramVec::new(
            HistogramOpts::new(
                "firnflow_compaction_duration_seconds",
                "Time to compact a namespace's data files. Merges \
                 small fragments into fewer, larger files to reduce \
                 S3 round-trips on the read path.",
            )
            .buckets(vec![1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0]),
            &["namespace"],
        )
        .map_err(metrics_err)?;
        registry
            .register(Box::new(compaction_duration.clone()))
            .map_err(metrics_err)?;

        let cached_handles = IntGauge::new(
            "firnflow_cached_handles",
            "Number of namespaces with a warm `lancedb::Connection` + \
             `lancedb::Table` handle in the in-process pool. Compare \
             against `firnflow_active_namespaces` — the delta is the \
             number of active namespaces that will pay cold-open cost \
             on their next request.",
        )
        .map_err(metrics_err)?;
        registry
            .register(Box::new(cached_handles.clone()))
            .map_err(metrics_err)?;

        let auth_rejections = IntCounterVec::new(
            Opts::new(
                "firnflow_auth_rejections_total",
                "Requests rejected before reaching their handler. \
                 The `reason` label is one of: `missing` (no \
                 Authorization header), `invalid` (header present \
                 but token does not match a configured key), \
                 `forbidden` (valid token, insufficient scope for \
                 the route), `rate_limited` (rejected by either \
                 rate limiter). Use this to detect misconfigured \
                 keys after a rotation (spike in `missing`) or \
                 credential-stuffing pressure (spike in `invalid` \
                 or `rate_limited`).",
            ),
            &["reason"],
        )
        .map_err(metrics_err)?;
        registry
            .register(Box::new(auth_rejections.clone()))
            .map_err(metrics_err)?;

        Ok(Self {
            registry,
            cache_hits,
            cache_misses,
            query_duration,
            write_duration,
            active_namespaces,
            s3_requests,
            index_build_duration,
            compaction_duration,
            cached_handles,
            auth_rejections,
            seen_namespaces: DashSet::new(),
        })
    }

    /// Borrow the underlying registry for the /metrics handler.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Serialise the current metric state as a Prometheus text
    /// exposition payload (`Content-Type: text/plain; version=0.0.4`).
    pub fn encode(&self) -> Result<String, FirnflowError> {
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        encoder
            .encode(&self.registry.gather(), &mut buffer)
            .map_err(metrics_err)?;
        String::from_utf8(buffer).map_err(|e| FirnflowError::Metrics(e.to_string()))
    }

    /// Bump `active_namespaces` the first time we see a namespace.
    fn touch(&self, ns: &NamespaceId) {
        if self.seen_namespaces.insert(ns.clone()) {
            self.active_namespaces.inc();
        }
    }

    /// Record a cache hit for `ns`.
    pub fn record_cache_hit(&self, ns: &NamespaceId) {
        self.touch(ns);
        self.cache_hits.with_label_values(&[ns.as_str()]).inc();
    }

    /// Record a cache miss for `ns`.
    pub fn record_cache_miss(&self, ns: &NamespaceId) {
        self.touch(ns);
        self.cache_misses.with_label_values(&[ns.as_str()]).inc();
    }

    /// Record a query duration observation.
    pub fn record_query(&self, ns: &NamespaceId, query_type: &str, duration_secs: f64) {
        self.touch(ns);
        self.query_duration
            .with_label_values(&[ns.as_str(), query_type])
            .observe(duration_secs);
    }

    /// Record a write duration observation.
    pub fn record_write(&self, ns: &NamespaceId, duration_secs: f64) {
        self.touch(ns);
        self.write_duration
            .with_label_values(&[ns.as_str()])
            .observe(duration_secs);
    }

    /// Record a firnflow-initiated S3-bound operation. See the
    /// help text on `firnflow_s3_requests_total` for why this is
    /// an approximation.
    pub fn record_s3_request(&self, ns: &NamespaceId, operation: &str) {
        self.touch(ns);
        self.s3_requests
            .with_label_values(&[ns.as_str(), operation])
            .inc();
    }

    /// Record an index build duration observation.
    pub fn record_index_build(&self, ns: &NamespaceId, kind: &str, duration_secs: f64) {
        self.touch(ns);
        self.index_build_duration
            .with_label_values(&[ns.as_str(), kind])
            .observe(duration_secs);
    }

    /// Record a compaction duration observation.
    pub fn record_compaction(&self, ns: &NamespaceId, duration_secs: f64) {
        self.touch(ns);
        self.compaction_duration
            .with_label_values(&[ns.as_str()])
            .observe(duration_secs);
    }

    /// Bump the cached-handles gauge when `NamespaceManager` inserts
    /// a fresh `NamespaceHandle` into its pool.
    pub fn inc_cached_handles(&self) {
        self.cached_handles.inc();
    }

    /// Drop the cached-handles gauge when `NamespaceManager` evicts
    /// a `NamespaceHandle` (namespace delete / index build / compact).
    pub fn dec_cached_handles(&self) {
        self.cached_handles.dec();
    }

    /// Current value of the `firnflow_cached_handles` gauge. Used
    /// by tests; production code should read it via `/metrics`.
    pub fn cached_handles_value(&self) -> i64 {
        self.cached_handles.get()
    }

    /// Record an auth/rate-limit rejection. `reason` must be one of
    /// the documented values: `"missing"`, `"invalid"`, `"forbidden"`,
    /// `"rate_limited"`. Other values are accepted but cardinality is
    /// the operator's responsibility.
    pub fn record_auth_rejection(&self, reason: &str) {
        self.auth_rejections.with_label_values(&[reason]).inc();
    }

    /// Current value of `firnflow_auth_rejections_total{reason=…}`.
    /// Test-only accessor; production code reads `/metrics`.
    pub fn auth_rejections_value(&self, reason: &str) -> u64 {
        self.auth_rejections.with_label_values(&[reason]).get()
    }
}

/// Build an `Arc<CoreMetrics>` for tests and stand-alone binaries
/// that don't need to share a registry across layers. Panics only
/// if the prometheus crate rejects its own metric definitions.
pub fn test_metrics() -> Arc<CoreMetrics> {
    Arc::new(CoreMetrics::new().expect("construct CoreMetrics"))
}

fn metrics_err(e: impl std::fmt::Display) -> FirnflowError {
    FirnflowError::Metrics(e.to_string())
}
