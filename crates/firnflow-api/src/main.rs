use anyhow::Context;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use firnflow_api::{build_state, config::AppConfig, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = AppConfig::from_env().context("load config")?;
    tracing::info!(
        bind = %config.bind,
        bucket = %config.bucket,
        "starting firnflow-api"
    );

    let state = build_state(&config).await.context("build app state")?;
    let app = router(state);

    let listener = TcpListener::bind(config.bind).await.context("bind")?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, app).await.context("serve")?;
    Ok(())
}
