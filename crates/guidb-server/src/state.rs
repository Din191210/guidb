use dashmap::DashMap;
use sqlx::mysql::MySqlPool;
use sqlx::postgres::PgPool;
use sqlx::sqlite::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub data_dir: PathBuf,
    pub catalog_pool: SqlitePool,
    pub sqlite_pools: Arc<DashMap<String, SqlitePool>>,
    pub pg_pools: Arc<DashMap<String, PgPool>>,
    pub mysql_pools: Arc<DashMap<String, MySqlPool>>,
}
