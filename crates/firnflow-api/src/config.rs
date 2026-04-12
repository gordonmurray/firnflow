//! Env-var driven application config.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;

/// Runtime configuration for the axum binary.
#[derive(Debug, Clone)]
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

        Ok(Self {
            bind,
            bucket,
            cache_memory_bytes,
            cache_nvme_path,
            cache_nvme_bytes,
            storage_options,
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
