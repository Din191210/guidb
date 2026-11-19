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
    async fn build_erd(&self, items: &[ItemInfo]) -> AppResult<ErdGraph>;
}

fn column_definition(
    column: &guidb_shared::NewColumnSpec,
    engine: EngineKind,
) -> AppResult<String> {
    let ty = column.data_type.trim().to_ascii_uppercase();
    let (base, args) = ty.split_once('(').unwrap_or((&ty, ""));
    let valid_type = matches!(
        base.trim(),
        "INT"
            | "INTEGER"
            | "BIGINT"
            | "SMALLINT"
            | "TINYINT"
            | "SERIAL"
            | "BIGSERIAL"
            | "TEXT"
            | "CHAR"
            | "VARCHAR"
            | "CHARACTER VARYING"
            | "BOOLEAN"
            | "BOOL"
            | "NUMERIC"
            | "REAL"
            | "FLOAT"
            | "DOUBLE"
            | "DOUBLE PRECISION"
            | "DECIMAL"
            | "JSON"
            | "JSONB"
            | "BLOB"
            | "BYTEA"
            | "DATE"
            | "TIME"
            | "TIMESTAMP"
            | "TIMESTAMPTZ"
            | "DATETIME"
            | "UUID"
    );
    let valid_args = !ty.contains('(')
        || args.strip_suffix(')').is_some_and(|v| {
            v.split(',').count() <= 2 && v.split(',').all(|n| n.trim().parse::<u16>().is_ok())
        });
    if !valid_type || !valid_args {
        return Err(AppError::BadRequest(format!(
            "Tipe kolom tidak didukung: {ty}"
        )));
    }
    let mut definition = format!("{} {ty}", ddl_ident(&column.name, engine)?);
    if column.is_pk {
        definition.push_str(" PRIMARY KEY");
        if engine == EngineKind::Sqlite && ty == "INTEGER" {
            definition.push_str(" AUTOINCREMENT");
        }
    } else if !column.nullable {
        definition.push_str(" NOT NULL");
    }
    if let Some(raw) = column
        .default_value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        definition.push_str(" DEFAULT ");
        if matches!(
            raw.to_ascii_uppercase().as_str(),
            "NULL" | "CURRENT_TIMESTAMP" | "TRUE" | "FALSE"
        ) || raw.parse::<f64>().is_ok_and(f64::is_finite)
        {
            definition.push_str(raw);
        } else {
            let value = raw
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .map(|v| v.replace("''", "'"))
                .unwrap_or_else(|| raw.to_owned());
            if engine == EngineKind::Mysql && value.contains('\\') {
                return Err(AppError::BadRequest(
                    "Nilai default dengan backslash harus diatur melalui editor SQL.".into(),
                ));
            }
            definition.push_str(&format!("'{}'", value.replace('\'', "''")));
        }
    }
    Ok(definition)
}

fn ddl_ident(name: &str, engine: EngineKind) -> AppResult<String> {
    use guidb_shared::sql_gen::{quote_ident, validate_identifier};
    Ok(quote_ident(
        &validate_identifier(name).map_err(AppError::BadRequest)?,
        engine,
    ))
}

pub async fn resolve_adapter(
    state: &AppState,
    conn: &ConnectionProfile,
) -> AppResult<Box<dyn DatabaseAdapter>> {
    let read_only = conn.protection_mode == guidb_shared::ProtectionMode::ReadOnly;
    match conn.engine {
        EngineKind::Sqlite => {
            let pool = if read_only {
                let path = catalog::resolve_sqlite_path(&state.data_dir, &conn.target);
                let options = sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(path)
                    .read_only(true)
                    .pragma("query_only", "ON");
                sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect_with(options)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?
            } else {
                get_or_open_sqlite(state, conn).await?
            };
            Ok(Box::new(sqlite::SqliteAdapter::new(pool)))
        }
        EngineKind::Postgres => {
            let pool = get_or_open_postgres(state, conn).await?;
            Ok(Box::new(postgres::PostgresAdapter::new(pool, read_only)))
        }
        EngineKind::Mysql => {
            let pool = get_or_open_mysql(state, conn).await?;
            Ok(Box::new(mysql::MysqlAdapter::new(pool, read_only)))
        }
        EngineKind::MongoDb => {
            if conn.target.starts_with("mongodb://") {
                return Err(AppError::BadRequest(
                    "Koneksi MongoDB eksternal hanya mendukung deteksi TCP. Gunakan file lokal untuk editor dokumen.".into(),
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
