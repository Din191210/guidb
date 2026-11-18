use anyhow::Context;
use guidb_server::{build_router, catalog};
use std::path::PathBuf;
use tokio::net::TcpListener;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    let data_dir = std::env::var("GUIDB_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data"));
    let static_dir = std::env::var("GUIDB_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./static"));
    let bind_addr = std::env::var("GUIDB_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".to_string());

    let state = catalog::init_app_state(data_dir)
        .await
        .context("Failed initializing GuiDB catalog and starter databases")?;

    guidb_server::backup::spawn_backup_scheduler(state.clone());

    let app = build_router(state, static_dir);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Failed binding TCP listener on {bind_addr}"))?;

    tracing::info!(addr = %bind_addr, "GuiDB Studio listening");
    axum::serve(listener, app)
        .await
        .context("Axum HTTP server terminated with error")?;

    Ok(())
}
