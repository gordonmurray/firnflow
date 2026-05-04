//! Bearer-token authentication and scope-checking middleware.
//!
//! Two static keys, both opt-in via env:
//!
//! * `FIRNFLOW_API_KEY` — read/write tier (`upsert`, `query`, `list`,
//!   `warmup`).
//! * `FIRNFLOW_ADMIN_API_KEY` — destructive ops (`delete`, `index`,
//!   `fts-index`, `scalar-index`, `compact`).
//!
//! When neither is configured, the API stays open — every request
//! gets a synthetic `Principal::Anonymous` keyed on peer IP, the
//! middleware logs a single startup `WARN` from `state::build_state`,
//! and tests inherit this default by constructing
//! [`AuthConfig::disabled`].
//!
//! Scope semantics:
//!
//! * Admin key authorises every route.
//! * Write key authorises read/write routes; on admin routes it
//!   yields `403 Forbidden` *unless* `admin_key` is unset, in which
//!   case the single-key fallback promotes the write key to admin
//!   authority for the duration of the deployment.
//!
//! Status codes:
//!
//! | Situation                                   | Status |
//! | ------------------------------------------- | ------ |
//! | Missing or malformed `Authorization` header | 401    |
//! | Token does not match any configured key     | 401    |
//! | Valid token, insufficient scope             | 403    |
//!
//! Constant-time compare via `subtle::ConstantTimeEq` so the secret
//! is not exposed to a remote timing side channel.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use firnflow_core::CoreMetrics;
use subtle::ConstantTimeEq;

use crate::error::ApiError;

/// Loopback used as the synthetic peer address when `ConnectInfo`
/// is absent — the only path that hits this is `oneshot`-driven
/// integration tests, which do not register a `ConnectInfo`
/// extension. Production traffic always carries one because
/// `firnflow-api::main` mounts the router with
/// `into_make_service_with_connect_info::<SocketAddr>()`.
const FALLBACK_PEER_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

/// Reserved label values for `firnflow_auth_rejections_total{reason}`.
/// Centralised so call sites do not drift away from the documented
/// cardinality.
pub mod rejection_reason {
    pub const MISSING: &str = "missing";
    pub const INVALID: &str = "invalid";
    pub const FORBIDDEN: &str = "forbidden";
    pub const RATE_LIMITED: &str = "rate_limited";
}

/// A redacted secret. Wraps the bytes that compose a configured API
/// key so accidental `tracing::info!(?config)` cannot dump the key
/// to logs. The constant-time comparison helper lives on this type
/// rather than on raw `&[u8]` so callers cannot bypass it by
/// dereferencing first.
#[derive(Clone)]
pub struct Secret(Arc<[u8]>);

impl Secret {
    /// Wrap a UTF-8 secret. Stores the raw bytes; tokens with
    /// non-UTF-8 content are not supported because `Authorization`
    /// header parsing yields `&str`.
    pub fn new<S: AsRef<[u8]>>(value: S) -> Self {
        Self(Arc::from(value.as_ref()))
    }

    /// Length-aware constant-time equality against an attacker-
    /// supplied byte slice. The length channel is unavoidable and
    /// already fundamental: we have to return *false* on length
    /// mismatch without comparing every byte. `subtle::ConstantTimeEq`
    /// handles the equal-length case in constant time.
    pub fn ct_eq(&self, other: &[u8]) -> bool {
        if self.0.len() != other.len() {
            return false;
        }
        bool::from(self.0.ct_eq(other))
    }

    /// Constant-time equality against another [`Secret`]. Used at
    /// startup to detect the misconfiguration where the read/write
    /// and admin keys are accidentally set to the same value — a
    /// case where the scope split would silently collapse because
    /// `principal_for` checks the admin key first.
    pub fn ct_eq_secret(&self, other: &Secret) -> bool {
        self.ct_eq(&other.0)
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// Permission tier required by a route. `Admin` ⊃ `Write` —
/// see `Principal::satisfies`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Write,
    Admin,
}

/// What kind of authority a successfully-presented bearer token
/// carries. `Anonymous` is reserved for default-open mode where no
/// key is configured at all and every request is admitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrincipalKind {
    Write,
    Admin,
}

/// The authenticated identity attached to a request by
/// [`require_write`] / [`require_admin`]. The principal limiter
/// (`crate::rate_limit`) reads this extension and buckets on its
/// `id_string()` so a single key cannot exhaust other principals'
/// quotas.
#[derive(Clone, Debug)]
pub enum Principal {
    Authenticated { kind: PrincipalKind, key_id: u64 },
    Anonymous { ip: IpAddr },
}

impl Principal {
    /// True iff this principal is allowed onto a route requiring
    /// `required`. Anonymous principals only reach scope checks when
    /// `AuthConfig::is_open` is true, in which case the middleware
    /// short-circuits before calling this — kept here for
    /// completeness and so unit tests can exercise the full table.
    pub fn satisfies(&self, required: Scope) -> bool {
        match self {
            Principal::Authenticated {
                kind: PrincipalKind::Admin,
                ..
            } => true,
            Principal::Authenticated {
                kind: PrincipalKind::Write,
                ..
            } => required == Scope::Write,
            Principal::Anonymous { .. } => false,
        }
    }

    /// String form used as the principal limiter's bucket key. Two
    /// authenticated requests with the same `key_id` share a bucket;
    /// two anonymous requests from the same peer share a bucket.
    pub fn id_string(&self) -> String {
        match self {
            Principal::Authenticated { key_id, .. } => format!("k:{key_id:016x}"),
            Principal::Anonymous { ip } => format!("ip:{ip}"),
        }
    }
}

/// Runtime auth/rate-limit configuration. Cloned cheaply (everything
/// inside is `Arc` or `Copy`).
///
/// `Default` produces the disabled-everything configuration, which
/// is what tests get for free and what production gets if no env
/// vars are set at startup.
#[derive(Clone, Default)]
pub struct AuthConfig {
    write_key: Option<Secret>,
    admin_key: Option<Secret>,
    metrics_key: Option<Secret>,
    /// `true` ⇒ trust `X-Forwarded-For` / `X-Real-IP` / `Forwarded`
    /// headers when extracting peer IP. Default `false` because
    /// firnflow may be reachable directly and forged headers would
    /// let any client rotate apparent IPs at will.
    pub trust_proxy_headers: bool,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("write_key", &self.write_key)
            .field("admin_key", &self.admin_key)
            .field("metrics_key", &self.metrics_key)
            .field("trust_proxy_headers", &self.trust_proxy_headers)
            .finish()
    }
}

impl AuthConfig {
    /// Disabled-everything config. Same as `Default::default()` but
    /// reads more clearly at call sites in tests.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Set the read/write key. `None` removes a previously-set key.
    pub fn with_write_key(mut self, key: Option<Secret>) -> Self {
        self.write_key = key;
        self
    }

    pub fn with_admin_key(mut self, key: Option<Secret>) -> Self {
        self.admin_key = key;
        self
    }

    pub fn with_metrics_key(mut self, key: Option<Secret>) -> Self {
        self.metrics_key = key;
        self
    }

    pub fn with_trust_proxy_headers(mut self, trust: bool) -> Self {
        self.trust_proxy_headers = trust;
        self
    }

    /// True when neither read/write nor admin key is configured.
    /// In this mode `require_write` / `require_admin` admit every
    /// request and attach a `Principal::Anonymous` for the rate
    /// limiter to bucket on.
    pub fn is_open(&self) -> bool {
        self.write_key.is_none() && self.admin_key.is_none()
    }

    /// True when both `write_key` and `admin_key` are configured and
    /// hold identical bytes. `principal_for` checks the admin key
    /// first, so a copy/paste of the same value into both env vars
    /// would silently route every authenticated request to the
    /// admin tier — `state::build_state` refuses to start in this
    /// case rather than letting the scope split collapse silently.
    pub fn duplicate_write_and_admin_keys(&self) -> bool {
        match (&self.write_key, &self.admin_key) {
            (Some(w), Some(a)) => w.ct_eq_secret(a),
            _ => false,
        }
    }

    /// True when a metrics token is configured. `/metrics` is
    /// public when this is false.
    pub fn metrics_token_configured(&self) -> bool {
        self.metrics_key.is_some()
    }

    /// Identify a presented bearer token. Returns `None` if the
    /// token does not match any configured key — the middleware
    /// turns that into 401. Returns `Some(PrincipalKind)` when the
    /// token matches; the middleware then checks scope and converts
    /// insufficient scope into 403.
    ///
    /// Implements the single-key fallback documented in
    /// `ISSUE_2.md` §1: if `admin_key` is unset, the write key
    /// authorises admin routes too.
    fn principal_for(&self, presented: &[u8]) -> Option<PrincipalKind> {
        if let Some(k) = self.admin_key.as_ref() {
            if k.ct_eq(presented) {
                return Some(PrincipalKind::Admin);
            }
        }
        if let Some(k) = self.write_key.as_ref() {
            if k.ct_eq(presented) {
                let kind = if self.admin_key.is_none() {
                    PrincipalKind::Admin
                } else {
                    PrincipalKind::Write
                };
                return Some(kind);
            }
        }
        None
    }

    /// Constant-time check against the metrics token.
    fn metrics_token_matches(&self, presented: &[u8]) -> bool {
        match self.metrics_key.as_ref() {
            Some(k) => k.ct_eq(presented),
            None => false,
        }
    }
}

/// State threaded into every auth middleware. Bundles the
/// `AuthConfig` with the metrics handle so rejections can be
/// counted without an extra extractor on every request.
#[derive(Clone)]
pub struct AuthState {
    pub config: Arc<AuthConfig>,
    pub metrics: Arc<CoreMetrics>,
}

/// Mounted via `from_fn_with_state(state, auth::require_write)`.
pub async fn require_write(
    State(state): State<AuthState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    require_scope_inner(&state, Scope::Write, req, next).await
}

/// Mounted via `from_fn_with_state(state, auth::require_admin)`.
pub async fn require_admin(
    State(state): State<AuthState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    require_scope_inner(&state, Scope::Admin, req, next).await
}

/// Mounted on `/metrics` only. Same Bearer parser as the data plane
/// — Prometheus's `bearer_token` / `bearer_token_file` scrape config
/// works against this directly. Does NOT attach a `Principal`,
/// because `/metrics` never traverses the principal limiter.
pub async fn require_metrics_token(
    State(state): State<AuthState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    if !state.config.metrics_token_configured() {
        return Ok(next.run(req).await);
    }
    let token = match parse_bearer(&req) {
        Some(t) => t,
        None => {
            state
                .metrics
                .record_auth_rejection(rejection_reason::MISSING);
            return Err(ApiError::Unauthorized);
        }
    };
    if !state.config.metrics_token_matches(token.as_bytes()) {
        state
            .metrics
            .record_auth_rejection(rejection_reason::INVALID);
        return Err(ApiError::Unauthorized);
    }
    Ok(next.run(req).await)
}

async fn require_scope_inner(
    state: &AuthState,
    required: Scope,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    if state.config.is_open() {
        let ip = peer_ip(&req, state.config.trust_proxy_headers);
        req.extensions_mut().insert(Principal::Anonymous { ip });
        return Ok(next.run(req).await);
    }

    let token = match parse_bearer(&req) {
        Some(t) => t,
        None => {
            state
                .metrics
                .record_auth_rejection(rejection_reason::MISSING);
            return Err(ApiError::Unauthorized);
        }
    };

    let kind = match state.config.principal_for(token.as_bytes()) {
        Some(k) => k,
        None => {
            state
                .metrics
                .record_auth_rejection(rejection_reason::INVALID);
            return Err(ApiError::Unauthorized);
        }
    };

    let principal = Principal::Authenticated {
        kind,
        key_id: stable_key_id(token.as_bytes()),
    };

    if !principal.satisfies(required) {
        state
            .metrics
            .record_auth_rejection(rejection_reason::FORBIDDEN);
        return Err(ApiError::Forbidden);
    }

    req.extensions_mut().insert(principal);
    Ok(next.run(req).await)
}

/// Pull `Authorization: Bearer <token>` out of a request. Returns
/// the token verbatim (no trimming beyond the literal `"Bearer "`
/// prefix), or `None` if the header is missing or malformed.
fn parse_bearer<B>(req: &Request<B>) -> Option<&str> {
    let raw = req.headers().get(header::AUTHORIZATION)?;
    let s = raw.to_str().ok()?;
    let token = s
        .strip_prefix("Bearer ")
        .or_else(|| s.strip_prefix("bearer "))?;
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Extract the peer IP for synthetic `Principal::Anonymous` and
/// for the optional pre-auth IP rate limiter. `trust_proxy_headers`
/// (driven by `FIRNFLOW_TRUST_PROXY_HEADERS`) gates the
/// X-Forwarded-For path; default is to read peer IP from
/// `ConnectInfo` only.
pub fn peer_ip<B>(req: &Request<B>, trust_proxy_headers: bool) -> IpAddr {
    if trust_proxy_headers {
        if let Some(ip) = forwarded_ip(req) {
            return ip;
        }
    }
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(FALLBACK_PEER_IP)
}

fn forwarded_ip<B>(req: &Request<B>) -> Option<IpAddr> {
    let headers = req.headers();
    let try_parse = |v: &HeaderValue, split: bool| -> Option<IpAddr> {
        let s = v.to_str().ok()?;
        if split {
            s.split(',').find_map(|p| p.trim().parse().ok())
        } else {
            s.trim().parse().ok()
        }
    };
    if let Some(v) = headers.get("x-forwarded-for") {
        if let Some(ip) = try_parse(v, true) {
            return Some(ip);
        }
    }
    if let Some(v) = headers.get("x-real-ip") {
        if let Some(ip) = try_parse(v, false) {
            return Some(ip);
        }
    }
    None
}

/// 64-bit deterministic, non-cryptographic stable id derived from
/// the token. Used so the principal limiter buckets on a small
/// fixed-size value rather than copying secret material into the
/// limiter's internal map. FNV-1a 64 because we need cross-process
/// determinism (so different replicas produce the same id for the
/// same token in logs) and `std::hash::DefaultHasher` does not
/// promise stability across builds.
fn stable_key_id(token: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in token {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// Status code wiring is in `crate::error::ApiError::IntoResponse`.
const _: StatusCode = StatusCode::UNAUTHORIZED;
const _: StatusCode = StatusCode::FORBIDDEN;

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_keys(write: Option<&str>, admin: Option<&str>) -> AuthConfig {
        AuthConfig::default()
            .with_write_key(write.map(Secret::new))
            .with_admin_key(admin.map(Secret::new))
    }

    #[test]
    fn open_when_no_keys() {
        let cfg = AuthConfig::default();
        assert!(cfg.is_open());
        assert!(!cfg.metrics_token_configured());
    }

    #[test]
    fn principal_for_admin_key() {
        let cfg = cfg_with_keys(Some("write"), Some("admin"));
        assert_eq!(
            cfg.principal_for(b"admin"),
            Some(PrincipalKind::Admin),
            "admin token must map to admin"
        );
    }

    #[test]
    fn principal_for_write_key_when_admin_configured() {
        let cfg = cfg_with_keys(Some("write"), Some("admin"));
        assert_eq!(
            cfg.principal_for(b"write"),
            Some(PrincipalKind::Write),
            "with both keys set, the write token stays Write — admin routes will get 403"
        );
    }

    #[test]
    fn single_key_fallback_promotes_write_to_admin() {
        let cfg = cfg_with_keys(Some("write"), None);
        assert_eq!(
            cfg.principal_for(b"write"),
            Some(PrincipalKind::Admin),
            "with no admin key configured, the write token authorises admin routes"
        );
    }

    #[test]
    fn unknown_token_returns_none() {
        let cfg = cfg_with_keys(Some("write"), Some("admin"));
        assert_eq!(cfg.principal_for(b"nope"), None);
    }

    #[test]
    fn principal_satisfies_table() {
        let admin = Principal::Authenticated {
            kind: PrincipalKind::Admin,
            key_id: 0,
        };
        let write = Principal::Authenticated {
            kind: PrincipalKind::Write,
            key_id: 0,
        };
        let anon = Principal::Anonymous {
            ip: FALLBACK_PEER_IP,
        };

        assert!(admin.satisfies(Scope::Admin));
        assert!(admin.satisfies(Scope::Write));
        assert!(!write.satisfies(Scope::Admin));
        assert!(write.satisfies(Scope::Write));
        // Anonymous reaches `satisfies` only if the middleware
        // forgets to short-circuit on `is_open` — pin the
        // conservative answer.
        assert!(!anon.satisfies(Scope::Write));
        assert!(!anon.satisfies(Scope::Admin));
    }

    #[test]
    fn secret_debug_redacts() {
        let s = Secret::new("super-secret-key");
        let dbg = format!("{:?}", s);
        let disp = format!("{}", s);
        assert!(!dbg.contains("super-secret"), "Debug leaked: {dbg}");
        assert!(!disp.contains("super-secret"), "Display leaked: {disp}");
        assert_eq!(dbg, "Secret(***)");
    }

    #[test]
    fn auth_config_debug_redacts() {
        let cfg = cfg_with_keys(Some("write-key"), Some("admin-key"))
            .with_metrics_key(Some(Secret::new("metrics-key")));
        let dbg = format!("{:?}", cfg);
        assert!(!dbg.contains("write-key"), "leaked write key: {dbg}");
        assert!(!dbg.contains("admin-key"), "leaked admin key: {dbg}");
        assert!(!dbg.contains("metrics-key"), "leaked metrics key: {dbg}");
    }

    #[test]
    fn ct_eq_length_mismatch() {
        let s = Secret::new("abc");
        assert!(!s.ct_eq(b"ab"));
        assert!(!s.ct_eq(b"abcd"));
        assert!(s.ct_eq(b"abc"));
    }

    #[test]
    fn ct_eq_secret_reflects_byte_equality() {
        let a = Secret::new("hunter2");
        let b = Secret::new("hunter2");
        let c = Secret::new("HUNTER2");
        assert!(a.ct_eq_secret(&b));
        assert!(!a.ct_eq_secret(&c));
    }

    #[test]
    fn duplicate_write_and_admin_keys_detected() {
        let same = cfg_with_keys(Some("same-token"), Some("same-token"));
        assert!(
            same.duplicate_write_and_admin_keys(),
            "identical keys must be flagged so build_state can refuse to start"
        );
    }

    #[test]
    fn distinct_write_and_admin_keys_pass() {
        let distinct = cfg_with_keys(Some("write"), Some("admin"));
        assert!(!distinct.duplicate_write_and_admin_keys());
    }

    #[test]
    fn single_key_or_disabled_does_not_count_as_duplicate() {
        let only_write = cfg_with_keys(Some("write"), None);
        let only_admin = cfg_with_keys(None, Some("admin"));
        let disabled = AuthConfig::disabled();
        assert!(!only_write.duplicate_write_and_admin_keys());
        assert!(!only_admin.duplicate_write_and_admin_keys());
        assert!(!disabled.duplicate_write_and_admin_keys());
    }

    #[test]
    fn parse_bearer_accepts_lowercase_scheme() {
        let req = Request::builder()
            .header(header::AUTHORIZATION, "bearer xyz")
            .body(Body::empty())
            .unwrap();
        assert_eq!(parse_bearer(&req), Some("xyz"));
    }

    #[test]
    fn parse_bearer_rejects_empty_token() {
        let req = Request::builder()
            .header(header::AUTHORIZATION, "Bearer ")
            .body(Body::empty())
            .unwrap();
        assert_eq!(parse_bearer(&req), None);
    }

    #[test]
    fn parse_bearer_rejects_other_schemes() {
        let req = Request::builder()
            .header(header::AUTHORIZATION, "Token xyz")
            .body(Body::empty())
            .unwrap();
        assert_eq!(parse_bearer(&req), None);
    }

    #[test]
    fn principal_id_string_distinct_per_kind() {
        let a = Principal::Authenticated {
            kind: PrincipalKind::Admin,
            key_id: 1,
        };
        let b = Principal::Anonymous {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        };
        assert_ne!(a.id_string(), b.id_string());
        assert!(a.id_string().starts_with("k:"));
        assert!(b.id_string().starts_with("ip:"));
    }
}
