//! Env-var driven application config.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;

use crate::auth::Secret;
use crate::rate_limit::RateLimitSettings;

/// Runtime configuration for the axum binary.
///
/// Auth-bearing fields (`api_key`, `admin_api_key`, `metrics_token`)
/// hold the redacting [`Secret`] newtype rather than raw strings so
/// `tracing::info!(?config)` cannot leak the configured API keys.
/// `storage_options` carries `object_store` parameters that may
/// include S3 credentials such as `aws_secret_access_key`; the
/// custom `Debug` impl below redacts the values for any key
/// recognised as credential-bearing instead of relying on the
/// derive.
#[derive(Clone)]
pub struct AppConfig {
    /// Address to bind the HTTP listener to.
    pub bind: SocketAddr,
    /// S3 bucket that roots every namespace.
    pub bucket: String,
    /// RAM tier capacity for the foyer cache, in bytes.
    pub cache_memory_bytes: usize,
    /// Directory for the foyer NVMe-tier block file.
    pub cache_nvme_path: PathBuf,
    /// NVMe tier capacity, in bytes.
    pub cache_nvme_bytes: usize,
    /// `object_store`-style S3 options passed straight through to
    /// `NamespaceManager` and, transitively, to lancedb.
    pub storage_options: HashMap<String, String>,
    /// `FIRNFLOW_API_KEY` — required for the read/write tier when
    /// auth is enabled. `None` ⇒ disabled.
    pub api_key: Option<Secret>,
    /// `FIRNFLOW_ADMIN_API_KEY` — required for destructive ops
    /// when set. `None` ⇒ single-key fallback (write key
    /// authorises admin too).
    pub admin_api_key: Option<Secret>,
    /// `FIRNFLOW_METRICS_TOKEN` — gates `/metrics`. `None` ⇒
    /// `/metrics` is public (current pre-0.5.0 behaviour).
    pub metrics_token: Option<Secret>,
    /// Rate-limiter knobs. `None` everywhere ⇒ both limiters off.
    pub rate_limit: RateLimitSettings,
}

/// True when the given storage-options key holds credential-bearing
/// material that must be redacted from `Debug` output. Matches by
/// substring (case-insensitive) so future credential keys
/// (`*_secret_access_key`, `*_session_token`, `*_password`,
/// vendor-specific names) are caught by the same check without
/// needing a code update.
fn is_sensitive_storage_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["secret", "password", "token", "access_key", "credential"]
        .iter()
        .any(|needle| lower.contains(needle))
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct RedactedOptions<'a>(&'a HashMap<String, String>);
        impl std::fmt::Debug for RedactedOptions<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let mut m = f.debug_map();
                for (k, v) in self.0 {
                    if is_sensitive_storage_key(k) {
                        m.entry(k, &"***");
                    } else {
                        m.entry(k, v);
                    }
                }
                m.finish()
            }
        }

        f.debug_struct("AppConfig")
            .field("bind", &self.bind)
            .field("bucket", &self.bucket)
            .field("cache_memory_bytes", &self.cache_memory_bytes)
            .field("cache_nvme_path", &self.cache_nvme_path)
            .field("cache_nvme_bytes", &self.cache_nvme_bytes)
            .field("storage_options", &RedactedOptions(&self.storage_options))
            .field("api_key", &self.api_key)
            .field("admin_api_key", &self.admin_api_key)
            .field("metrics_token", &self.metrics_token)
            .field("rate_limit", &self.rate_limit)
            .finish()
    }
}

impl AppConfig {
    /// Load config from the environment. Only `FIRNFLOW_S3_BUCKET`
    /// is strictly required; everything else has a sensible default.
    pub fn from_env() -> anyhow::Result<Self> {
        let bind: SocketAddr = env_or("FIRNFLOW_BIND", "0.0.0.0:3000")
            .parse()
            .context("FIRNFLOW_BIND")?;
        let bucket =
            std::env::var("FIRNFLOW_S3_BUCKET").context("FIRNFLOW_S3_BUCKET must be set")?;
        let cache_memory_bytes: usize = env_or("FIRNFLOW_CACHE_MEMORY_BYTES", "67108864")
            .parse()
            .context("FIRNFLOW_CACHE_MEMORY_BYTES")?;
        let cache_nvme_path =
            PathBuf::from(env_or("FIRNFLOW_CACHE_NVME_PATH", "/tmp/firnflow-cache"));
        let cache_nvme_bytes: usize = env_or("FIRNFLOW_CACHE_NVME_BYTES", "268435456")
            .parse()
            .context("FIRNFLOW_CACHE_NVME_BYTES")?;

        let storage_options = build_storage_options();

        let api_key = optional_secret("FIRNFLOW_API_KEY");
        let admin_api_key = optional_secret("FIRNFLOW_ADMIN_API_KEY");
        let metrics_token = optional_secret("FIRNFLOW_METRICS_TOKEN");

        let trust_proxy_headers = env_bool("FIRNFLOW_TRUST_PROXY_HEADERS", false)?;
        let per_principal_rps = optional_u64("FIRNFLOW_RATE_LIMIT_RPS")?;
        let preauth_ip_rps = optional_u64("FIRNFLOW_PREAUTH_IP_LIMIT_RPS")?;
        let burst_size = optional_u32("FIRNFLOW_RATE_LIMIT_BURST")?;

        let rate_limit = RateLimitSettings {
            per_principal_rps,
            burst_size,
            preauth_ip_rps,
            trust_proxy_headers,
        };

        Ok(Self {
            bind,
            bucket,
            cache_memory_bytes,
            cache_nvme_path,
            cache_nvme_bytes,
            storage_options,
            api_key,
            admin_api_key,
            metrics_token,
            rate_limit,
        })
    }
}

fn build_storage_options() -> HashMap<String, String> {
    let mut opts = HashMap::new();
    if let Ok(v) = std::env::var("FIRNFLOW_S3_ENDPOINT") {
        // Custom endpoint implies MinIO / a local S3 emulator: force
        // path-style addressing and allow plain HTTP. A deployment
        // against real AWS leaves FIRNFLOW_S3_ENDPOINT unset and
        // skips this whole block.
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
    opts
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn optional_secret(key: &str) -> Option<Secret> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(Secret::new(v)),
        _ => None,
    }
}

fn optional_u64(key: &str) -> anyhow::Result<Option<u64>> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(Some(v.parse().with_context(|| key.to_string())?)),
        _ => Ok(None),
    }
}

fn optional_u32(key: &str) -> anyhow::Result<Option<u32>> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(Some(v.parse().with_context(|| key.to_string())?)),
        _ => Ok(None),
    }
}

fn env_bool(key: &str, default: bool) -> anyhow::Result<bool> {
    match std::env::var(key) {
        Ok(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" | "" => Ok(false),
            other => Err(anyhow::anyhow!(
                "{key} must be one of: true, false, 1, 0, yes, no, on, off — got {other:?}"
            )),
        },
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_storage_keys_are_recognised() {
        assert!(is_sensitive_storage_key("aws_secret_access_key"));
        assert!(is_sensitive_storage_key("aws_access_key_id"));
        assert!(is_sensitive_storage_key("AWS_SECRET_ACCESS_KEY"));
        assert!(is_sensitive_storage_key("aws_session_token"));
        assert!(is_sensitive_storage_key("gcs_credentials"));
        assert!(is_sensitive_storage_key("user_password"));
        assert!(!is_sensitive_storage_key("aws_endpoint"));
        assert!(!is_sensitive_storage_key("aws_region"));
        assert!(!is_sensitive_storage_key("allow_http"));
        assert!(!is_sensitive_storage_key(
            "aws_virtual_hosted_style_request"
        ));
    }

    #[test]
    fn debug_redacts_storage_credentials() {
        let mut opts = HashMap::new();
        opts.insert(
            "aws_secret_access_key".into(),
            "UNIQUE_S3_SECRET_DO_NOT_LEAK".into(),
        );
        opts.insert(
            "aws_access_key_id".into(),
            "UNIQUE_S3_ACCESS_DO_NOT_LEAK".into(),
        );
        opts.insert("aws_endpoint".into(), "http://minio:9000".into());
        opts.insert("aws_region".into(), "eu-west-1".into());

        let cfg = AppConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            bucket: "test".into(),
            cache_memory_bytes: 0,
            cache_nvme_path: std::env::temp_dir(),
            cache_nvme_bytes: 0,
            storage_options: opts,
            api_key: None,
            admin_api_key: None,
            metrics_token: None,
            rate_limit: RateLimitSettings::default(),
        };

        let dbg = format!("{:?}", cfg);
        assert!(
            !dbg.contains("UNIQUE_S3_SECRET"),
            "S3 secret leaked in Debug: {dbg}"
        );
        assert!(
            !dbg.contains("UNIQUE_S3_ACCESS"),
            "S3 access key leaked in Debug: {dbg}"
        );
        // Non-sensitive options must still be visible — they are
        // useful for diagnosing config (which endpoint? which region?).
        assert!(
            dbg.contains("http://minio:9000"),
            "non-sensitive endpoint should not be redacted: {dbg}"
        );
        assert!(
            dbg.contains("eu-west-1"),
            "non-sensitive region should not be redacted: {dbg}"
        );
    }
}
