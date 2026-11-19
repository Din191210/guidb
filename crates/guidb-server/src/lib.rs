pub mod api;
pub mod backup;
pub mod catalog;
pub mod discovery;
pub mod drivers;
pub mod error;
pub mod safety;
pub mod state;
pub mod ui;

use crate::state::AppState;
use axum::routing::{delete, get, post, put};
use axum::Router;
use std::path::PathBuf;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

pub fn build_router(state: AppState, static_dir: PathBuf) -> Router {
    Router::new()
        .route("/", get(ui::index_page))
        .route("/api/bootstrap", get(api::get_bootstrap))
        .route("/api/discovery/scan", get(api::discovery_scan))
        .route("/api/discovery/provision", post(api::discovery_provision))
        .route(
            "/api/connections",
            get(api::list_connections).post(api::create_connection),
        )
        .route("/api/connections/test", post(api::test_connection))
        .route("/api/connections/{id}", delete(api::delete_connection))
        .route(
            "/api/connections/{id}/protection",
            put(api::update_protection),
        )
        .route("/api/connections/{id}/workspace", get(api::get_workspace))
        .route("/api/connections/{id}/query", post(api::execute_query))
        .route("/api/connections/{id}/mutate", post(api::mutate_staged))
        .route("/api/connections/{id}/ddl", post(api::execute_ddl))
        .route("/api/connections/{id}/export", get(api::export_item))
        .route("/api/backups", get(api::get_backups))
        .route("/api/backups/create", post(api::create_backup))
        .route("/api/backups/{id}/verify", post(api::verify_backup))
        .route("/api/backups/{id}/restore", post(api::restore_backup))
        .route("/api/backups/{id}/download", get(api::download_backup))
        .route("/api/backups/schedules", post(api::create_schedule))
        .route(
            "/api/backups/schedules/{id}/toggle",
            post(api::toggle_schedule),
        )
        .route("/api/backups/schedules/{id}/run", post(api::run_schedule))
        .route("/api/backups/schedules/{id}", delete(api::delete_schedule))
        .route("/api/schema-sync/diff", post(api::schema_sync_diff))
        .route("/api/schema-sync/apply", post(api::schema_sync_apply))
        .route("/api/history", get(api::list_history))
        .route(
            "/api/saved-queries",
            get(api::list_saved_queries).post(api::create_saved_query),
        )
        .route("/api/saved-queries/{id}", delete(api::delete_saved_query))
        .nest_service("/static", ServeDir::new(static_dir))
        .with_state(state)
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<_>| {
                tracing::info_span!(
                    "http_request",
                    id = %Uuid::new_v4(),
                    method = %req.method(),
                    path = %req.uri().path()
                )
            }),
        )
}
