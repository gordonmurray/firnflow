use std::net::SocketAddr;

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
    // `into_make_service_with_connect_info` makes the peer
    // `SocketAddr` available as a request extension. The auth
    // middleware reads it for the synthetic `Principal::Anonymous`
    // and the optional pre-auth IP limiter buckets on it. Tests
    // drive the router via `oneshot` and synthesise a fallback in
    // `auth::peer_ip` instead.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serve")?;
    Ok(())
}
