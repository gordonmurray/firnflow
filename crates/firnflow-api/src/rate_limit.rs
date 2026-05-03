//! Rate-limit configuration and key extractors for the protected
//! sub-router.
//!
//! Two layers, both optional:
//!
//! * **Per-principal limiter** — applied *after* auth so the bucket
//!   key is the validated `Principal`, never an unvalidated header.
//!   Driven by `FIRNFLOW_RATE_LIMIT_RPS` + `FIRNFLOW_RATE_LIMIT_BURST`.
//!
//! * **Pre-auth IP limiter (optional)** — applied *before* auth as
//!   the outermost layer on the protected router. Caps
//!   credential-stuffing throughput by peer IP. Driven by
//!   `FIRNFLOW_PREAUTH_IP_LIMIT_RPS`. Reuses the burst size of the
//!   per-principal limiter when set, defaulting to 30 otherwise.
//!
//! Both layers convert `GovernorError::TooManyRequests` into our
//! `ApiError::RateLimited(d)` (status 429 + `Retry-After`) and bump
//! `firnflow_auth_rejections_total{reason="rate_limited"}` so a
//! single Prometheus dashboard surfaces both auth and rate-limit
//! pressure.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::response::IntoResponse;
use firnflow_core::CoreMetrics;
use governor::clock::QuantaInstant;
use governor::middleware::NoOpMiddleware;
use tower_governor::errors::GovernorError;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::KeyExtractor;
use tower_governor::GovernorLayer;

use crate::auth::{rejection_reason, Principal};
use crate::error::ApiError;

/// Knobs read from env at startup. `None` everywhere ⇒ both
/// limiters are disabled.
#[derive(Debug, Clone, Default)]
pub struct RateLimitSettings {
    pub per_principal_rps: Option<u64>,
    pub burst_size: Option<u32>,
    pub preauth_ip_rps: Option<u64>,
    pub trust_proxy_headers: bool,
}

/// Type alias for the per-principal limiter layer. Spelling out
/// `NoOpMiddleware<QuantaInstant>` is necessary because
/// `tower_governor` does not re-export it; importing it from
/// `governor` directly is the simpler alternative.
pub type PrincipalLimiter = GovernorLayer<PrincipalKeyExtractor, NoOpMiddleware<QuantaInstant>>;

/// Type alias for the pre-auth IP limiter layer.
pub type IpLimiter = GovernorLayer<IpKeyExtractor, NoOpMiddleware<QuantaInstant>>;

/// Build the per-principal limiter. Returns `None` when disabled.
pub fn build_principal_limiter(
    settings: &RateLimitSettings,
    metrics: Arc<CoreMetrics>,
) -> Option<PrincipalLimiter> {
    let rps = settings.per_principal_rps?;
    if rps == 0 {
        return None;
    }
    let burst = settings.burst_size.unwrap_or(30).max(1);
    let extractor = PrincipalKeyExtractor;
    let metrics_for_handler = metrics;
    let cfg = GovernorConfigBuilder::default()
        .per_second(rps)
        .burst_size(burst)
        .key_extractor(extractor)
        .error_handler(move |e| rate_limit_error_response(e, &metrics_for_handler))
        .finish()
        .expect("rps > 0 and burst > 0 are enforced above");
    Some(GovernorLayer {
        config: Arc::new(cfg),
    })
}

/// Build the pre-auth IP limiter. Returns `None` when disabled.
pub fn build_preauth_ip_limiter(
    settings: &RateLimitSettings,
    metrics: Arc<CoreMetrics>,
) -> Option<IpLimiter> {
    let rps = settings.preauth_ip_rps?;
    if rps == 0 {
        return None;
    }
    let burst = settings.burst_size.unwrap_or(30).max(1);
    let extractor = IpKeyExtractor {
        trust_proxy_headers: settings.trust_proxy_headers,
    };
    let metrics_for_handler = metrics;
    let cfg = GovernorConfigBuilder::default()
        .per_second(rps)
        .burst_size(burst)
        .key_extractor(extractor)
        .error_handler(move |e| rate_limit_error_response(e, &metrics_for_handler))
        .finish()
        .expect("rps > 0 and burst > 0 are enforced above");
    Some(GovernorLayer {
        config: Arc::new(cfg),
    })
}

/// Convert `GovernorError` into our axum `Response`. Counts the
/// rejection so operators see rate-limit pressure on the same
/// `firnflow_auth_rejections_total` counter that surfaces auth
/// rejections.
fn rate_limit_error_response(
    mut err: GovernorError,
    metrics: &Arc<CoreMetrics>,
) -> axum::http::Response<Body> {
    metrics.record_auth_rejection(rejection_reason::RATE_LIMITED);
    match err {
        GovernorError::TooManyRequests { wait_time, .. } => {
            ApiError::RateLimited(Duration::from_secs(wait_time)).into_response()
        }
        // Falls through for `UnableToExtractKey` (router misconfigured
        // — pre-auth IP limiter without a peer IP and without proxy
        // trust) and the catch-all `Other`. Surface them as 500 with
        // a generic body so the metrics counter still moves but no
        // information leaks.
        _ => err.as_response::<Body>(),
    }
}

/// Bucket key extractor for the per-principal limiter. Reads the
/// `Principal` extension that the auth middleware attaches and
/// returns its `id_string()`. The auth middleware always attaches
/// a `Principal` (either `Authenticated` or `Anonymous`), so the
/// `expect` path here is unreachable in normal request flow — if
/// it ever fires it is a router-wiring bug, not a runtime input
/// failure, and a 500 is the right answer.
#[derive(Clone, Copy, Debug)]
pub struct PrincipalKeyExtractor;

impl KeyExtractor for PrincipalKeyExtractor {
    type Key = String;

    fn extract<B>(&self, req: &Request<B>) -> Result<Self::Key, GovernorError> {
        match req.extensions().get::<Principal>() {
            Some(p) => Ok(p.id_string()),
            None => Err(GovernorError::UnableToExtractKey),
        }
    }
}

/// Bucket key extractor for the optional pre-auth IP limiter. Reads
/// `ConnectInfo<SocketAddr>` (and optionally forwarded headers)
/// and falls back to a sentinel address when neither is present —
/// the `oneshot`-driven test path needs that fallback so the
/// extractor never fails open. Honours the same
/// `FIRNFLOW_TRUST_PROXY_HEADERS` knob the auth middleware reads.
#[derive(Clone, Copy, Debug)]
pub struct IpKeyExtractor {
    trust_proxy_headers: bool,
}

impl KeyExtractor for IpKeyExtractor {
    type Key = IpAddr;

    fn extract<B>(&self, req: &Request<B>) -> Result<Self::Key, GovernorError> {
        if self.trust_proxy_headers {
            if let Some(ip) = forwarded_ip(req) {
                return Ok(ip);
            }
        }
        let ip = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip())
            .unwrap_or(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        Ok(ip)
    }
}

fn forwarded_ip<B>(req: &Request<B>) -> Option<IpAddr> {
    let headers = req.headers();
    if let Some(v) = headers.get("x-forwarded-for") {
        if let Ok(s) = v.to_str() {
            if let Some(ip) = s.split(',').find_map(|p| p.trim().parse().ok()) {
                return Some(ip);
            }
        }
    }
    if let Some(v) = headers.get("x-real-ip") {
        if let Ok(s) = v.to_str() {
            if let Ok(ip) = s.trim().parse() {
                return Some(ip);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_req() -> Request<Body> {
        Request::builder().body(Body::empty()).unwrap()
    }

    #[test]
    fn principal_extractor_yields_id_string() {
        let mut req = empty_req();
        req.extensions_mut().insert(Principal::Anonymous {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        });
        let extractor = PrincipalKeyExtractor;
        let key = extractor.extract(&req).unwrap();
        assert_eq!(key, "ip:10.0.0.1");
    }

    #[test]
    fn principal_extractor_errors_when_missing() {
        let req = empty_req();
        let extractor = PrincipalKeyExtractor;
        let err = extractor.extract(&req).unwrap_err();
        assert!(
            matches!(err, GovernorError::UnableToExtractKey),
            "expected UnableToExtractKey, got {err:?}"
        );
    }

    #[test]
    fn ip_extractor_falls_back_to_loopback() {
        let req = empty_req();
        let extractor = IpKeyExtractor {
            trust_proxy_headers: false,
        };
        let ip = extractor.extract(&req).unwrap();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn ip_extractor_honours_x_forwarded_for_when_trusted() {
        let req = Request::builder()
            .header("x-forwarded-for", "203.0.113.5, 10.0.0.1")
            .body(Body::empty())
            .unwrap();
        let extractor = IpKeyExtractor {
            trust_proxy_headers: true,
        };
        let ip = extractor.extract(&req).unwrap();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)));
    }

    #[test]
    fn ip_extractor_ignores_x_forwarded_for_by_default() {
        let req = Request::builder()
            .header("x-forwarded-for", "203.0.113.5")
            .body(Body::empty())
            .unwrap();
        let extractor = IpKeyExtractor {
            trust_proxy_headers: false,
        };
        let ip = extractor.extract(&req).unwrap();
        assert_eq!(
            ip,
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            "untrusted forwarded header must not change the bucket key"
        );
    }

    #[test]
    fn build_principal_limiter_disabled_when_zero() {
        let metrics = firnflow_core::metrics::test_metrics();
        let settings = RateLimitSettings {
            per_principal_rps: Some(0),
            ..Default::default()
        };
        assert!(build_principal_limiter(&settings, metrics).is_none());
    }

    #[test]
    fn build_principal_limiter_enabled_when_set() {
        let metrics = firnflow_core::metrics::test_metrics();
        let settings = RateLimitSettings {
            per_principal_rps: Some(1),
            burst_size: Some(2),
            ..Default::default()
        };
        assert!(build_principal_limiter(&settings, metrics).is_some());
    }
}
