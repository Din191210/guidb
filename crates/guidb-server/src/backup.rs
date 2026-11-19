use crate::catalog;
use crate::drivers::{resolve_adapter, DataQueryOptions};
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use guidb_shared::sql_gen::{format_sql_literal, quote_ident};
use guidb_shared::{
    BackupDashboardResponse, BackupKind, BackupRecord, BackupSchedule, BackupScope,
    CreateConnectionRequest, CreateScheduleRequest, DataModel, EngineKind, EnvironmentKind,
    ProtectionMode, RestoreBackupRequest, RestoreBackupResponse, VerifyBackupResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::AssertSqlSafe;
use sqlx::Row;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

fn backups_dir(state: &AppState) -> PathBuf {
    state.data_dir.join("backups")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[derive(Serialize, Deserialize)]
struct NosqlBackupBundle {
    engine: EngineKind,
    source_connection: String,
    created_at: String,
    collections: BTreeMap<String, Vec<serde_json::Map<String, serde_json::Value>>>,
}

pub async fn get_backup_dashboard(state: &AppState) -> AppResult<BackupDashboardResponse> {
    let backups = list_backups(state).await?;
    let schedules = list_schedules(state).await?;
    Ok(BackupDashboardResponse {
        backups,
        schedules,
        backup_directory: backups_dir(state).display().to_string(),
    })
}

pub async fn list_backups(state: &AppState) -> AppResult<Vec<BackupRecord>> {
    let rows = sqlx::query(
        "SELECT id, connection_id, connection_name, engine, kind, scope, label, file_name,
                file_size_bytes, sha256, object_count, row_count, created_at
         FROM backup_records ORDER BY created_at DESC",
    )
    .fetch_all(&state.catalog_pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|r| BackupRecord {
            id: r.get("id"),
            connection_id: r.get("connection_id"),
            connection_name: r.get("connection_name"),
            engine: catalog::parse_engine(&r.get::<String, _>("engine")),
            kind: BackupKind::from_slug(&r.get::<String, _>("kind")),
            scope: BackupScope::from_slug(&r.get::<String, _>("scope")),
            label: r.get("label"),
            file_name: r.get("file_name"),
            file_size_bytes: r.get::<i64, _>("file_size_bytes").max(0) as u64,
            sha256: r.get("sha256"),
            object_count: r.get::<i64, _>("object_count").max(0) as usize,
            row_count: r.get::<i64, _>("row_count").max(0) as usize,
            created_at: r.get("created_at"),
        })
        .collect())
}

pub async fn list_schedules(state: &AppState) -> AppResult<Vec<BackupSchedule>> {
    let rows = sqlx::query(
        "SELECT id, connection_id, connection_name, interval_minutes, scope, retention_count,
                enabled, last_run_at, next_run_at, created_at
         FROM backup_schedules ORDER BY created_at DESC",
    )
    .fetch_all(&state.catalog_pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|r| BackupSchedule {
            id: r.get("id"),
            connection_id: r.get("connection_id"),
            connection_name: r.get("connection_name"),
            interval_minutes: r.get("interval_minutes"),
            scope: BackupScope::from_slug(&r.get::<String, _>("scope")),
            retention_count: r.get("retention_count"),
            enabled: r.get::<i64, _>("enabled") != 0,
            last_run_at: r.get("last_run_at"),
            next_run_at: r.get("next_run_at"),
            created_at: r.get("created_at"),
        })
        .collect())
}

pub async fn create_backup(
    state: &AppState,
    conn_id: &str,
    kind: BackupKind,
    scope: BackupScope,
    label: Option<String>,
) -> AppResult<BackupRecord> {
    create_backup_record(state, conn_id, kind, scope, label, None).await
}

async fn create_backup_record(
    state: &AppState,
    conn_id: &str,
    kind: BackupKind,
    scope: BackupScope,
    label: Option<String>,
    schedule_id: Option<&str>,
) -> AppResult<BackupRecord> {
    let conn = catalog::get_connection(&state.catalog_pool, conn_id).await?;
    let adapter = resolve_adapter(state, &conn).await?;
    let items = adapter.list_items().await?;

    let dir = backups_dir(state);
    std::fs::create_dir_all(&dir).map_err(|e| AppError::Database(e.to_string()))?;

    let now_utc = chrono::Utc::now();
    let created_at = now_utc.to_rfc3339();
    let stamp = now_utc.format("%Y%m%d_%H%M%S").to_string();
    let id = format!("bkp-{}", Uuid::new_v4().simple());
    let safe_slug: String = conn
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();

    let mut total_rows = 0_usize;
    let object_count = items.len();

    let (ext, bytes) = if conn.engine == EngineKind::Sqlite
        && scope == BackupScope::FullSchemaAndData
    {
        let pool = crate::drivers::get_or_open_sqlite(state, &conn).await?;
        let snapshot = dir.join(format!("{id}.snapshot"));
        sqlx::query("VACUUM INTO ?")
            .bind(snapshot.to_string_lossy().as_ref())
            .execute(&pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let bytes = std::fs::read(&snapshot).map_err(anyhow::Error::from)?;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&snapshot)
            .read_only(true);
        let copy = sqlx::SqlitePool::connect_with(options)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_all(&copy)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
        for table in tables {
            let query = format!(
                "SELECT COUNT(*) FROM {}",
                quote_ident(&table, EngineKind::Sqlite)
            );
            total_rows += sqlx::query_scalar::<_, i64>(AssertSqlSafe(query.as_str()))
                .fetch_one(&copy)
                .await
                .map_err(|e| AppError::Database(e.to_string()))? as usize;
        }
        copy.close().await;
        std::fs::remove_file(snapshot).map_err(anyhow::Error::from)?;
        ("sqlite", bytes)
    } else if conn.engine.model() == DataModel::Relational {
        let mut sql = String::new();
        sql.push_str(&format!(
            "-- GuiDB Studio Backup\n-- Connection: {} ({})\n-- Scope: {}\n-- Created: {}\n\n",
            conn.name,
            catalog::engine_to_str(conn.engine),
            scope.as_str(),
            created_at
        ));
        if conn.engine == EngineKind::Sqlite {
            sql.push_str("PRAGMA foreign_keys = OFF;\n");
        }
        sql.push_str("BEGIN;\n\n");

        // Dump tables first, then views
        let tables: Vec<_> = items.iter().filter(|i| i.item_type == "TABLE").collect();
        let views: Vec<_> = items.iter().filter(|i| i.item_type == "VIEW").collect();

        for item in &tables {
            let st = adapter.inspect_structure(&item.name).await?;
            let ddl = st.ddl.trim();
            if !ddl.is_empty() {
                sql.push_str(ddl);
                if !ddl.ends_with(';') {
                    sql.push(';');
                }
                sql.push_str("\n\n");
            }
            for idx in &st.indexes {
                if !idx.columns.is_empty() {
                    let cols: Vec<String> = idx
                        .columns
                        .iter()
                        .map(|c| quote_ident(c, conn.engine))
                        .collect();
                    let u = if idx.unique { "UNIQUE " } else { "" };
                    sql.push_str(&format!(
                        "CREATE {u}INDEX IF NOT EXISTS {} ON {} ({});\n",
                        quote_ident(&idx.name, conn.engine),
                        quote_ident(&item.name, conn.engine),
                        cols.join(", ")
                    ));
                }
            }
            sql.push('\n');

            if scope == BackupScope::FullSchemaAndData {
                let mut offset = 0_i64;
                loop {
                    let chunk = adapter
                        .fetch_item_data(
                            &item.name,
                            DataQueryOptions {
                                filter: None,
                                sort_col: None,
                                sort_dir: None,
                                limit: 500,
                                offset,
                            },
                        )
                        .await?;
                    if chunk.rows.is_empty() {
                        break;
                    }
                    let qt = quote_ident(&item.name, conn.engine);
                    let cols: Vec<String> = chunk
                        .columns
                        .iter()
                        .map(|c| quote_ident(c, conn.engine))
                        .collect();
                    for row in &chunk.rows {
                        let vals: Vec<String> = row.iter().map(format_sql_literal).collect();
                        sql.push_str(&format!(
                            "INSERT INTO {qt} ({}) VALUES ({});\n",
                            cols.join(", "),
                            vals.join(", ")
                        ));
                        total_rows += 1;
                    }
                    if chunk.rows.len() < 500 {
                        break;
                    }
                    offset += 500;
                }
                sql.push('\n');
            }
        }

        for view in &views {
            if let Ok(st) = adapter.inspect_structure(&view.name).await {
                let ddl = st.ddl.trim();
                if !ddl.is_empty() {
                    sql.push_str(ddl);
                    if !ddl.ends_with(';') {
                        sql.push(';');
                    }
                    sql.push_str("\n\n");
                }
            }
        }

        sql.push_str("COMMIT;\n");
        if conn.engine == EngineKind::Sqlite {
            sql.push_str("PRAGMA foreign_keys = ON;\n");
        }
        ("sql", sql.into_bytes())
    } else {
        if conn.target.starts_with("redis://") {
            return Err(AppError::BadRequest(
                "Gunakan snapshot RDB native untuk backup Redis eksternal.".into(),
            ));
        }
        let pool = crate::drivers::get_or_open_sqlite(state, &conn).await?;
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut collections = BTreeMap::new();
        if conn.engine == EngineKind::Redis {
            let mut keys = Vec::new();
            if scope == BackupScope::FullSchemaAndData {
                for row in sqlx::query(
                    "SELECT key, data_type, value_text, CASE WHEN ttl_seconds < 0 THEN -1 ELSE max(0, ttl_seconds - CAST((julianday('now') - julianday(updated_at))*86400 AS INTEGER)) END AS ttl_seconds FROM _guidb_kv WHERE ttl_seconds < 0 OR (julianday('now') - julianday(updated_at))*86400 < ttl_seconds ORDER BY key",
                )
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?
                {
                    let record = serde_json::Map::from_iter([
                        ("key".into(), row.get::<String, _>("key").into()),
                        ("data_type".into(), row.get::<String, _>("data_type").into()),
                        ("value".into(), row.get::<String, _>("value_text").into()),
                        (
                            "ttl_seconds".into(),
                            row.get::<i64, _>("ttl_seconds").into(),
                        ),
                    ]);
                    keys.push(record);
                }
            }
            total_rows = keys.len();
            collections.insert("*".into(), keys);
        } else {
            let names: Vec<String> =
                sqlx::query_scalar("SELECT name FROM _guidb_collections ORDER BY name")
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
            for name in names {
                let mut docs = Vec::new();
                if scope == BackupScope::FullSchemaAndData {
                    for row in sqlx::query("SELECT doc_id, doc_json FROM _guidb_documents WHERE collection = ? ORDER BY doc_id").bind(&name).fetch_all(&mut *tx).await.map_err(|e| AppError::Database(e.to_string()))? {
                        let mut doc: serde_json::Map<String, serde_json::Value> = serde_json::from_str(row.get("doc_json")).map_err(|e| AppError::Database(format!("Invalid document: {e}")))?;
                        doc.insert("_id".into(), row.get::<String,_>("doc_id").into());
                        docs.push(doc);
                    }
                }
                total_rows += docs.len();
                collections.insert(name, docs);
            }
        }
        tx.commit()
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let bundle = NosqlBackupBundle {
            engine: conn.engine,
            source_connection: conn.name.clone(),
            created_at: created_at.clone(),
            collections,
        };
        let json_bytes =
            serde_json::to_vec_pretty(&bundle).map_err(|e| AppError::Database(e.to_string()))?;
        ("json", json_bytes)
    };

    let file_name = format!("{safe_slug}_{stamp}_{id}.{ext}");
    let file_path = dir.join(&file_name);
    let temp = file_path.with_extension("part");
    {
        use std::io::Write;
        let mut file = std::fs::File::create_new(&temp).map_err(anyhow::Error::from)?;
        file.write_all(&bytes).map_err(anyhow::Error::from)?;
        file.sync_all().map_err(anyhow::Error::from)?;
    }
    std::fs::rename(&temp, &file_path).map_err(anyhow::Error::from)?;
    std::fs::File::open(&dir)
        .and_then(|dir| dir.sync_all())
        .map_err(anyhow::Error::from)?;
    let file_size_bytes = bytes.len() as u64;
    let sha256 = sha256_hex(&bytes);
    let final_label = label
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            format!(
                "{} Backup ({stamp})",
                match kind {
                    BackupKind::Manual => "Manual",
                    BackupKind::Scheduled => "Scheduled",
                    BackupKind::PreMigrationAuto => "Pre-Sync",
                }
            )
        });

    sqlx::query(
        "INSERT INTO backup_records (id, connection_id, connection_name, engine, kind, scope, label, file_name, file_size_bytes, sha256, object_count, row_count, created_at, schedule_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )
    .bind(&id)
    .bind(&conn.id)
    .bind(&conn.name)
    .bind(catalog::engine_to_str(conn.engine))
    .bind(kind.as_str())
    .bind(scope.as_str())
    .bind(&final_label)
    .bind(&file_name)
    .bind(file_size_bytes as i64)
    .bind(&sha256)
    .bind(object_count as i64)
    .bind(total_rows as i64)
    .bind(&created_at)
    .bind(schedule_id)
    .execute(&state.catalog_pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    catalog::record_history(
        &state.catalog_pool,
        &conn.id,
        &conn.name,
        &format!("BACKUP ({}) -> {}", kind.as_str(), file_name),
        "OK (BACKUP)",
        total_rows as i64,
        1,
    )
    .await;

    Ok(BackupRecord {
        id,
        connection_id: conn.id,
        connection_name: conn.name,
        engine: conn.engine,
        kind,
        scope,
        label: final_label,
        file_name,
        file_size_bytes,
        sha256,
        object_count,
        row_count: total_rows,
        created_at,
    })
}

async fn get_backup_record(state: &AppState, backup_id: &str) -> AppResult<BackupRecord> {
    let row = sqlx::query(
        "SELECT id, connection_id, connection_name, engine, kind, scope, label, file_name,
                file_size_bytes, sha256, object_count, row_count, created_at
         FROM backup_records WHERE id = ?1",
    )
    .bind(backup_id)
    .fetch_optional(&state.catalog_pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?
    .ok_or_else(|| AppError::NotFound(format!("Backup '{backup_id}' not found")))?;

    Ok(BackupRecord {
        id: row.get("id"),
        connection_id: row.get("connection_id"),
        connection_name: row.get("connection_name"),
        engine: catalog::parse_engine(&row.get::<String, _>("engine")),
        kind: BackupKind::from_slug(&row.get::<String, _>("kind")),
        scope: BackupScope::from_slug(&row.get::<String, _>("scope")),
        label: row.get("label"),
        file_name: row.get("file_name"),
        file_size_bytes: row.get::<i64, _>("file_size_bytes").max(0) as u64,
        sha256: row.get("sha256"),
        object_count: row.get::<i64, _>("object_count").max(0) as usize,
        row_count: row.get::<i64, _>("row_count").max(0) as usize,
        created_at: row.get("created_at"),
    })
}

fn sanitize_backup_path(state: &AppState, file_name: &str) -> AppResult<PathBuf> {
    let name_only = Path::new(file_name)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| AppError::BadRequest("Invalid backup filename".into()))?;
    Ok(backups_dir(state).join(name_only))
}

pub async fn get_backup_file_content(
    state: &AppState,
    backup_id: &str,
) -> AppResult<(String, Vec<u8>)> {
    let rec = get_backup_record(state, backup_id).await?;
    let path = sanitize_backup_path(state, &rec.file_name)?;
    let bytes = std::fs::read(&path)
        .map_err(|e| AppError::NotFound(format!("Backup file missing on disk: {e}")))?;
    Ok((rec.file_name, bytes))
}

pub async fn verify_backup(state: &AppState, backup_id: &str) -> AppResult<VerifyBackupResponse> {
    let rec = get_backup_record(state, backup_id).await?;
    let path = sanitize_backup_path(state, &rec.file_name)?;
    let bytes = std::fs::read(&path)
        .map_err(|e| AppError::NotFound(format!("Backup file missing on disk: {e}")))?;
    let actual_sha256 = sha256_hex(&bytes);
    let valid = actual_sha256 == rec.sha256;
    Ok(VerifyBackupResponse {
        backup_id: rec.id,
        valid,
        expected_sha256: rec.sha256,
        actual_sha256,
        file_size_bytes: bytes.len() as u64,
        message: if valid {
            "SHA-256 terverifikasi utuh dan cocok.".to_string()
        } else {
            "PERINGATAN: Checksum SHA-256 file backup tidak cocok!".to_string()
        },
    })
}

/// Restore a backup into a brand-new isolated database file so existing databases are never overwritten.
pub async fn restore_backup_isolated(
    state: &AppState,
    backup_id: &str,
    req: RestoreBackupRequest,
) -> AppResult<RestoreBackupResponse> {
    let rec = get_backup_record(state, backup_id).await?;
    let (_, bytes) = get_backup_file_content(state, backup_id).await?;
    if sha256_hex(&bytes) != rec.sha256 {
        return Err(AppError::Database("Checksum backup tidak cocok.".into()));
    }
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let short_id = Uuid::new_v4().simple();
    let target_file = format!("restored_{stamp}_{short_id}.sqlite");
    let resolved = state.data_dir.join(&target_file);
    let conn_name = req
        .new_connection_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Restored: {} ({stamp})", rec.connection_name));

    if rec.file_name.ends_with(".sqlite") {
        std::fs::write(&resolved, bytes).map_err(anyhow::Error::from)?;
        let connection = catalog::create_connection(
            state,
            CreateConnectionRequest {
                name: conn_name,
                engine: EngineKind::Sqlite,
                environment: EnvironmentKind::Local,
                target: target_file,
                protection_mode: Some(ProtectionMode::SafeGuard),
            },
        )
        .await?;
        return Ok(RestoreBackupResponse {
            connection,
            restored_statements: 0,
            message: "Snapshot SQLite berhasil dipulihkan ke database baru.".into(),
        });
    }

    if rec.file_name.ends_with(".sql") {
        let sql_text = String::from_utf8(bytes).map_err(|e| AppError::Database(e.to_string()))?;
        let pool = catalog::open_sqlite_pool(&resolved)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        // Convert PostgreSQL/MySQL transaction keywords or execute statements cleanly
        let clean_sql = sql_text;
        sqlx::raw_sql(AssertSqlSafe(clean_sql.as_str()))
            .execute(&pool)
            .await
            .map_err(|e| AppError::Database(format!("Isolated restore SQL failed: {e}")))?;
        let stmt_count = clean_sql
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with("--")
            })
            .count();
        pool.close().await;

        let conn = catalog::create_connection(
            state,
            CreateConnectionRequest {
                name: conn_name,
                engine: EngineKind::Sqlite,
                environment: EnvironmentKind::Local,
                target: target_file,
                protection_mode: Some(ProtectionMode::SafeGuard),
            },
        )
        .await?;

        return Ok(RestoreBackupResponse {
            connection: conn,
            restored_statements: stmt_count,
            message: "Backup SQL berhasil dipulihkan ke database terisolasi baru tanpa menimpa database lama.".into(),
        });
    }

    // NoSQL JSON bundle restore into new isolated embedded Document or KV database
    let bundle: NosqlBackupBundle = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::Database(format!("Invalid JSON backup bundle: {e}")))?;

    let target_engine = if bundle.engine == EngineKind::Redis {
        EngineKind::Redis
    } else {
        EngineKind::MongoDb
    };

    let pool = catalog::open_sqlite_pool(&resolved)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    catalog::ensure_store_schema(&pool, target_engine).await?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let mut restored = 0_usize;
    let now = chrono::Utc::now().to_rfc3339();

    if target_engine == EngineKind::MongoDb {
        for (col_name, docs) in bundle.collections {
            sqlx::query(
                "INSERT OR IGNORE INTO _guidb_collections (name, created_at) VALUES (?1, ?2)",
            )
            .bind(&col_name)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
            for doc in docs {
                let doc_id = doc
                    .get("_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
                let json_str = serde_json::to_string(&doc).unwrap_or_else(|_| "{}".to_string());
                sqlx::query(
                    "INSERT OR REPLACE INTO _guidb_documents (collection, doc_id, doc_json, updated_at) VALUES (?1, ?2, ?3, ?4)",
                )
                .bind(&col_name)
                .bind(doc_id)
                .bind(json_str)
                .bind(&now)
                .execute(&mut *tx)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
                restored += 1;
            }
        }
    } else {
        for (_ns, entries) in bundle.collections {
            for row in entries {
                let key = row.get("key").and_then(|v| v.as_str()).unwrap_or_default();
                if key.is_empty() {
                    continue;
                }
                let dt = row
                    .get("data_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("STRING");
                let val = row.get("value").and_then(|v| v.as_str()).unwrap_or("");
                let ttl = row
                    .get("ttl_seconds")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-1);
                let ns = if let Some((p, _)) = key.split_once(':') {
                    format!("{p}:*")
                } else {
                    "default:*".to_string()
                };
                sqlx::query(
                    "INSERT OR REPLACE INTO _guidb_kv (key, namespace, data_type, value_text, ttl_seconds, byte_size, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .bind(key)
                .bind(ns)
                .bind(dt)
                .bind(val)
                .bind(ttl)
                .bind(val.len() as i64)
                .bind(&now)
                .execute(&mut *tx)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
                restored += 1;
            }
        }
    }
    tx.commit()
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    pool.close().await;
    let conn = catalog::create_connection(
        state,
        CreateConnectionRequest {
            name: conn_name,
            engine: target_engine,
            environment: EnvironmentKind::Local,
            target: target_file,
            protection_mode: Some(ProtectionMode::SafeGuard),
        },
    )
    .await?;

    Ok(RestoreBackupResponse {
        connection: conn,
        restored_statements: restored,
        message: "Backup JSON berhasil dipulihkan ke database terisolasi baru.".into(),
    })
}

pub async fn create_schedule(
    state: &AppState,
    req: CreateScheduleRequest,
) -> AppResult<BackupSchedule> {
    let conn = catalog::get_connection(&state.catalog_pool, &req.connection_id).await?;
    let interval = req.interval_minutes.clamp(1, 43200);
    let retention = req.retention_count.clamp(1, 100);
    let id = format!("sched-{}", Uuid::new_v4().simple());
    let now = chrono::Utc::now();
    let created_at = now.to_rfc3339();
    let next_run_at = (now + chrono::Duration::minutes(interval)).to_rfc3339();

    sqlx::query(
        "INSERT INTO backup_schedules (id, connection_id, connection_name, interval_minutes, scope, retention_count, enabled, last_run_at, next_run_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, NULL, ?7, ?8)",
    )
    .bind(&id)
    .bind(&conn.id)
    .bind(&conn.name)
    .bind(interval)
    .bind(req.scope.as_str())
    .bind(retention)
    .bind(&next_run_at)
    .bind(&created_at)
    .execute(&state.catalog_pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(BackupSchedule {
        id,
        connection_id: conn.id,
        connection_name: conn.name,
        interval_minutes: interval,
        scope: req.scope,
        retention_count: retention,
        enabled: true,
        last_run_at: None,
        next_run_at,
        created_at,
    })
}

pub async fn toggle_schedule(
    state: &AppState,
    id: &str,
    enabled: bool,
) -> AppResult<BackupSchedule> {
    sqlx::query("UPDATE backup_schedules SET enabled = ?1 WHERE id = ?2")
        .bind(if enabled { 1_i64 } else { 0_i64 })
        .bind(id)
        .execute(&state.catalog_pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let list = list_schedules(state).await?;
    list.into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| AppError::NotFound(format!("Schedule '{id}' not found")))
}

pub async fn delete_schedule(state: &AppState, id: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM backup_schedules WHERE id = ?1")
        .bind(id)
        .execute(&state.catalog_pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

pub async fn run_schedule_now(state: &AppState, id: &str) -> AppResult<BackupRecord> {
    run_schedule(state, id, None)
        .await?
        .ok_or_else(|| AppError::BadRequest("Jadwal sedang dijalankan.".into()))
}

async fn run_schedule(
    state: &AppState,
    id: &str,
    expected_due: Option<&str>,
) -> AppResult<Option<BackupRecord>> {
    let schedules = list_schedules(state).await?;
    let sched = schedules
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| AppError::NotFound(format!("Schedule '{id}' not found")))?;

    let locks = state.data_dir.join("schedule-locks");
    std::fs::create_dir_all(&locks).map_err(anyhow::Error::from)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(locks.join(sha256_hex(id.as_bytes())))
        .map_err(anyhow::Error::from)?;
    match file.try_lock() {
        Ok(()) => (),
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(anyhow::Error::from(e).into()),
    }
    if let Some(due) = expected_due {
        let still_due: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM backup_schedules WHERE id=? AND enabled=1 AND next_run_at=?)")
            .bind(id).bind(due).fetch_one(&state.catalog_pool).await.map_err(|e| AppError::Database(e.to_string()))?;
        if !still_due {
            return Ok(None);
        }
    }
    let record = create_backup_record(
        state,
        &sched.connection_id,
        BackupKind::Scheduled,
        sched.scope,
        Some(format!("Scheduled ({}m)", sched.interval_minutes)),
        Some(id),
    )
    .await?;

    let now = chrono::Utc::now();
    let next_run = (now + chrono::Duration::minutes(sched.interval_minutes)).to_rfc3339();
    sqlx::query("UPDATE backup_schedules SET last_run_at = ?1, next_run_at = ?2 WHERE id = ?3")
        .bind(now.to_rfc3339())
        .bind(next_run)
        .bind(&sched.id)
        .execute(&state.catalog_pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    prune_old_scheduled_backups(state, id, sched.retention_count).await;
    Ok(Some(record))
}

async fn prune_old_scheduled_backups(state: &AppState, schedule_id: &str, retention_count: i64) {
    let rows = sqlx::query(
        "SELECT id, file_name FROM backup_records
         WHERE schedule_id = ?1 AND kind = 'scheduled'
         ORDER BY created_at DESC",
    )
    .bind(schedule_id)
    .fetch_all(&state.catalog_pool)
    .await
    .unwrap_or_default();

    for old in rows.into_iter().skip(retention_count.max(1) as usize) {
        let old_id: String = old.get("id");
        let file_name: String = old.get("file_name");
        if let Ok(p) = sanitize_backup_path(state, &file_name) {
            if let Err(error) = std::fs::remove_file(p) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(%error, %old_id, "Cannot prune backup");
                    continue;
                }
            }
        }
        let _ = sqlx::query("DELETE FROM backup_records WHERE id = ?1")
            .bind(old_id)
            .execute(&state.catalog_pool)
            .await;
    }
}

pub fn spawn_backup_scheduler(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        loop {
            ticker.tick().await;
            let now_str = chrono::Utc::now().to_rfc3339();
            let due_ids: Vec<(String, String)> = sqlx::query_as(
                "SELECT id, next_run_at FROM backup_schedules WHERE enabled = 1 AND next_run_at <= ?1",
            )
            .bind(&now_str)
            .fetch_all(&state.catalog_pool)
            .await
            .unwrap_or_default();

            for (sched_id, due) in due_ids {
                if let Err(err) = run_schedule(&state, &sched_id, Some(&due)).await {
                    tracing::warn!(schedule_id = %sched_id, error = ?err, "Scheduled backup failed");
                }
            }
        }
    });
}
