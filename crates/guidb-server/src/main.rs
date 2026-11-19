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

    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Failed binding TCP listener on {bind_addr}"))?;
    let password = std::env::var("GUIDB_ADMIN_PASSWORD")
        .ok()
        .filter(|s| !s.trim().is_empty());
    anyhow::ensure!(
        listener.local_addr()?.ip().is_loopback() || password.is_some(),
        "Set GUIDB_ADMIN_PASSWORD before binding outside loopback"
    );
    let state = catalog::init_app_state(data_dir)
        .await
        .context("Failed initializing GuiDB catalog and starter databases")?;

    guidb_server::backup::spawn_backup_scheduler(state.clone());

    let app = build_router(state, static_dir);
    let app = if let Some(password) = password {
        use axum::{
            extract::Request,
            http::{header, StatusCode},
            middleware::{self, Next},
            response::IntoResponse,
        };
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use subtle::ConstantTimeEq;
        let credential =
            base64::engine::general_purpose::STANDARD.encode(format!("admin:{password}"));
        let expected = Sha256::digest(format!("Basic {credential}"));
        app.layer(middleware::from_fn(
            move |request: Request, next: Next| async move {
                let supplied = request
                    .headers()
                    .get(header::AUTHORIZATION)
                    .map(|h| h.as_bytes())
                    .unwrap_or_default();
                if bool::from(Sha256::digest(supplied).ct_eq(&expected)) {
                    next.run(request).await
                } else {
                    (
                        StatusCode::UNAUTHORIZED,
                        [(header::WWW_AUTHENTICATE, "Basic realm=\"GuiDB\"")],
                    )
                        .into_response()
                }
            },
        ))
    } else {
        app
    };

    let app = app.layer(axum::middleware::map_response(|mut res: axum::response::Response| async move {
        use axum::http::{header, HeaderValue};
        let headers = res.headers_mut();
        headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
        ));
        headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        res
    }));

    tracing::info!(addr = %listener.local_addr()?, "GuiDB Studio listening");
    axum::serve(listener, app)
        .await
        .context("Axum HTTP server terminated with error")?;

    Ok(())
}
