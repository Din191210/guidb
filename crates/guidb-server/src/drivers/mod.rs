pub mod document;
pub mod kv;
pub mod mysql;
pub mod postgres;
pub mod sqlite;
pub mod sync;

use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use async_trait::async_trait;
use guidb_shared::{
    ConnectionProfile, DdlAction, DdlReport, EngineKind, ErdGraph, ItemInfo, MutationReport,
    QueryResult, StagedMutation, StructureDef, TestConnectionResponse,
};

#[derive(Debug, Clone)]
pub struct DataQueryOptions {
    pub filter: Option<String>,
    pub sort_col: Option<String>,
    pub sort_dir: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

#[async_trait]
pub trait DatabaseAdapter: Send + Sync {
    async fn test_connection(&self) -> AppResult<TestConnectionResponse>;
    async fn list_items(&self) -> AppResult<Vec<ItemInfo>>;
    async fn inspect_structure(&self, item: &str) -> AppResult<StructureDef>;
    async fn fetch_item_data(&self, item: &str, opts: DataQueryOptions) -> AppResult<QueryResult>;
    async fn execute_query(&self, item: Option<&str>, query: &str) -> AppResult<QueryResult>;
    async fn apply_staged_mutations(
        &self,
        mutations: &[StagedMutation],
    ) -> AppResult<MutationReport>;
    async fn execute_ddl(&self, action: DdlAction) -> AppResult<DdlReport>;
    async fn build_erd(&self) -> AppResult<ErdGraph>;
}

pub async fn resolve_adapter(
    state: &AppState,
    conn: &ConnectionProfile,
) -> AppResult<Box<dyn DatabaseAdapter>> {
    match conn.engine {
        EngineKind::Sqlite => {
            let pool = get_or_open_sqlite(state, conn).await?;
            Ok(Box::new(sqlite::SqliteAdapter::new(pool)))
        }
        EngineKind::Postgres => {
            let pool = get_or_open_postgres(state, conn).await?;
            Ok(Box::new(postgres::PostgresAdapter::new(pool)))
        }
        EngineKind::Mysql => {
            let pool = get_or_open_mysql(state, conn).await?;
            Ok(Box::new(mysql::MysqlAdapter::new(pool)))
        }
        EngineKind::MongoDb => {
            if conn.target.starts_with("mongodb://") {
                return Err(AppError::BadRequest(
                    "External mongodb:// wire driver requires active mongod TCP server; use local collection path for embedded Document Engine or test reachability first".into(),
                ));
            }
            let pool = get_or_open_sqlite(state, conn).await?;
            Ok(Box::new(document::DocumentAdapter::new(pool)))
        }
        EngineKind::Redis => {
            let pool = if conn.target.starts_with("redis://") {
                None
            } else {
                Some(get_or_open_sqlite(state, conn).await?)
            };
            Ok(Box::new(kv::KvAdapter::new(pool, conn.target.clone())))
        }
    }
}

pub async fn get_or_open_sqlite(
    state: &AppState,
    conn: &ConnectionProfile,
) -> AppResult<sqlx::SqlitePool> {
    if let Some(existing) = state.sqlite_pools.get(&conn.id) {
        return Ok(existing.clone());
    }
    let path = catalog::resolve_sqlite_path(&state.data_dir, &conn.target);
    let pool = catalog::open_sqlite_pool(&path)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    state.sqlite_pools.insert(conn.id.clone(), pool.clone());
    Ok(pool)
}

async fn get_or_open_postgres(
    state: &AppState,
    conn: &ConnectionProfile,
) -> AppResult<sqlx::PgPool> {
    if let Some(existing) = state.pg_pools.get(&conn.id) {
        return Ok(existing.clone());
    }
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(4))
        .connect(&conn.target)
        .await
        .map_err(|e| AppError::Database(format!("PostgreSQL connection error: {e}")))?;
    state.pg_pools.insert(conn.id.clone(), pool.clone());
    Ok(pool)
}

async fn get_or_open_mysql(
    state: &AppState,
    conn: &ConnectionProfile,
) -> AppResult<sqlx::MySqlPool> {
    if let Some(existing) = state.mysql_pools.get(&conn.id) {
        return Ok(existing.clone());
    }
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(4))
        .connect(&conn.target)
        .await
        .map_err(|e| AppError::Database(format!("MySQL connection error: {e}")))?;
    state.mysql_pools.insert(conn.id.clone(), pool.clone());
    Ok(pool)
}

pub async fn test_connection_direct(
    state: &AppState,
    engine: EngineKind,
    target: &str,
) -> AppResult<TestConnectionResponse> {
    let start = std::time::Instant::now();
    match engine {
        EngineKind::Sqlite => {
            let path = catalog::resolve_sqlite_path(&state.data_dir, target);
            let pool = catalog::open_sqlite_pool(&path)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            let ver: String = sqlx::query_scalar("SELECT sqlite_version()")
                .fetch_one(&pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            pool.close().await;
            Ok(TestConnectionResponse {
                reachable: true,
                latency_ms: start.elapsed().as_millis() as u64,
                server_version: format!("SQLite {ver}"),
                details: format!("Opened {}", path.display()),
            })
        }
        EngineKind::Postgres => {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(4))
                .connect(target)
                .await
                .map_err(|e| AppError::Database(format!("PostgreSQL connect failed: {e}")))?;
            let ver: String = sqlx::query_scalar("SELECT version()")
                .fetch_one(&pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            pool.close().await;
            Ok(TestConnectionResponse {
                reachable: true,
                latency_ms: start.elapsed().as_millis() as u64,
                server_version: ver.split(',').next().unwrap_or(&ver).to_string(),
                details: "PostgreSQL handshake OK".to_string(),
            })
        }
        EngineKind::Mysql => {
            let pool = sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(4))
                .connect(target)
                .await
                .map_err(|e| AppError::Database(format!("MySQL connect failed: {e}")))?;
            let ver: String = sqlx::query_scalar("SELECT VERSION()")
                .fetch_one(&pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            pool.close().await;
            Ok(TestConnectionResponse {
                reachable: true,
                latency_ms: start.elapsed().as_millis() as u64,
                server_version: format!("MySQL/MariaDB {ver}"),
                details: "MySQL handshake OK".to_string(),
            })
        }
        EngineKind::MongoDb => {
            if target.starts_with("mongodb://") {
                let addr = target
                    .trim_start_matches("mongodb://")
                    .split('/')
                    .next()
                    .unwrap_or("127.0.0.1:27017");
                tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::net::TcpStream::connect(addr),
                )
                .await
                .map_err(|_| AppError::Database(format!("TCP timeout connecting to {addr}")))?
                .map_err(|e| AppError::Database(format!("TCP connect error to {addr}: {e}")))?;
                return Ok(TestConnectionResponse {
                    reachable: true,
                    latency_ms: start.elapsed().as_millis() as u64,
                    server_version: "MongoDB TCP Wire".to_string(),
                    details: format!("Reachable at {addr}"),
                });
            }
            let path = catalog::resolve_sqlite_path(&state.data_dir, target);
            let pool = catalog::open_sqlite_pool(&path)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_guidb_collections'",
            )
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
            pool.close().await;
            Ok(TestConnectionResponse {
                reachable: true,
                latency_ms: start.elapsed().as_millis() as u64,
                server_version: "GuiDB Document Store v1".to_string(),
                details: format!("Document store initialized (catalog_present={})", count > 0),
            })
        }
        EngineKind::Redis => {
            if target.starts_with("redis://") {
                let pong = kv::send_resp_command(target, &["PING"]).await?;
                return Ok(TestConnectionResponse {
                    reachable: true,
                    latency_ms: start.elapsed().as_millis() as u64,
                    server_version: "Redis RESP Server".to_string(),
                    details: pong,
                });
            }
            let path = catalog::resolve_sqlite_path(&state.data_dir, target);
            let pool = catalog::open_sqlite_pool(&path)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            pool.close().await;
            Ok(TestConnectionResponse {
                reachable: true,
                latency_ms: start.elapsed().as_millis() as u64,
                server_version: "GuiDB Redis KV Store v1".to_string(),
                details: format!("Opened {}", path.display()),
            })
        }
    }
}
