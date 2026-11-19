use crate::drivers::{DataQueryOptions, DatabaseAdapter};
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use guidb_shared::sql_gen::preview_mutations;
use guidb_shared::{
    CellValue, ColumnDef, DdlAction, DdlReport, EngineKind, ErdGraph, ItemInfo, MutationReport,
    QueryResult, StagedMutation, StructureDef, TestConnectionResponse,
};
use sqlx::AssertSqlSafe;
use sqlx::{Row, SqlitePool};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub struct KvAdapter {
    pool: Option<SqlitePool>,
    target: String,
}

impl KvAdapter {
    pub fn new(pool: Option<SqlitePool>, target: String) -> Self {
        Self { pool, target }
    }

    async fn require_pool(&self) -> AppResult<&SqlitePool> {
        let pool = self.pool.as_ref().ok_or_else(|| {
            AppError::BadRequest("Live redis:// URL uses direct RESP wire commands".into())
        })?;
        sqlx::query("DELETE FROM _guidb_kv WHERE ttl_seconds >= 0 AND (julianday('now') - julianday(updated_at))*86400 >= ttl_seconds")
            .execute(pool).await.map_err(|e| AppError::Database(e.to_string()))?;
        Ok(pool)
    }
}

pub async fn send_resp_command(target: &str, args: &[&str]) -> AppResult<String> {
    let stripped = target.trim_start_matches("redis://");
    let host_port = stripped.split('/').next().unwrap_or("127.0.0.1:6379");
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        TcpStream::connect(host_port),
    )
    .await
    .map_err(|_| AppError::Database(format!("Redis TCP timeout connecting to {host_port}")))?
    .map_err(|e| AppError::Database(format!("Redis TCP error connecting to {host_port}: {e}")))?;

    let mut payload = format!("*{}\r\n", args.len());
    for arg in args {
        payload.push_str(&format!("${}\r\n{}\r\n", arg.len(), arg));
    }
    stream
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let mut reader = BufReader::new(stream);
    read_resp_value(&mut reader).await
}

async fn read_resp_value(reader: &mut BufReader<TcpStream>) -> AppResult<String> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
    if let Some(rest) = trimmed.strip_prefix('+') {
        return Ok(rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix('-') {
        return Err(AppError::Database(rest.to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix(':') {
        return Ok(rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix('$') {
        let len: isize = rest.parse().unwrap_or(-1);
        if len < 0 {
            return Ok("(nil)".to_string());
        }
        let mut buf = vec![0_u8; len as usize];
        reader
            .read_exact(&mut buf)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut crlf = [0_u8; 2];
        let _ = reader.read_exact(&mut crlf).await;
        return Ok(String::from_utf8_lossy(&buf).to_string());
    }
    if let Some(rest) = trimmed.strip_prefix('*') {
        let count: isize = rest.parse().unwrap_or(0);
        if count <= 0 {
            return Ok("[]".to_string());
        }
        let mut items = Vec::with_capacity(count as usize);
        for _ in 0..count {
            items.push(Box::pin(read_resp_value(reader)).await?);
        }
        return Ok(serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string()));
    }
    Ok(trimmed.to_string())
}

#[async_trait]
impl DatabaseAdapter for KvAdapter {
    async fn test_connection(&self) -> AppResult<TestConnectionResponse> {
        let start = Instant::now();
        if self.target.starts_with("redis://") {
            let pong = send_resp_command(&self.target, &["PING"]).await?;
            return Ok(TestConnectionResponse {
                reachable: true,
                latency_ms: start.elapsed().as_millis() as u64,
                server_version: "Redis RESP3/RESP2 Server".to_string(),
                details: pong,
            });
        }
        let pool = self.require_pool().await?;
        let keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _guidb_kv")
            .fetch_one(pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(TestConnectionResponse {
            reachable: true,
            latency_ms: start.elapsed().as_millis() as u64,
            server_version: "GuiDB Key-Value Store (Redis-Compatible)".to_string(),
            details: format!("{keys} active keys"),
        })
    }

    async fn list_items(&self) -> AppResult<Vec<ItemInfo>> {
        if self.target.starts_with("redis://") {
            let dbsize = send_resp_command(&self.target, &["DBSIZE"]).await?;
            return Ok(vec![ItemInfo {
                name: "*".to_string(),
                item_type: "KEYSPACE".to_string(),
                row_count: dbsize.parse::<i64>().unwrap_or(0),
            }]);
        }
        let pool = self.require_pool().await?;
        let rows = sqlx::query(
            "SELECT namespace, COUNT(*) AS cnt FROM _guidb_kv GROUP BY namespace ORDER BY namespace ASC",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _guidb_kv")
            .fetch_one(pool)
            .await
            .unwrap_or(0);

        let mut items = vec![ItemInfo {
            name: "*".to_string(),
            item_type: "ALL KEYS".to_string(),
            row_count: total,
        }];
        for r in rows {
            items.push(ItemInfo {
                name: r.get("namespace"),
                item_type: "KEYSPACE".to_string(),
                row_count: r.get("cnt"),
            });
        }
        Ok(items)
    }

    async fn inspect_structure(&self, item: &str) -> AppResult<StructureDef> {
        Ok(StructureDef {
            item_name: item.to_string(),
            item_type: "KEYSPACE".to_string(),
            columns: vec![
                ColumnDef {
                    name: "key".into(),
                    data_type: "REDIS_KEY".into(),
                    nullable: false,
                    is_pk: true,
                    default_value: None,
                },
                ColumnDef {
                    name: "data_type".into(),
                    data_type: "STRING | HASH | LIST | SET | ZSET".into(),
                    nullable: false,
                    is_pk: false,
                    default_value: Some("STRING".into()),
                },
                ColumnDef {
                    name: "value".into(),
                    data_type: "PAYLOAD".into(),
                    nullable: false,
                    is_pk: false,
                    default_value: None,
                },
                ColumnDef {
                    name: "ttl_seconds".into(),
                    data_type: "INTEGER (-1 = persistent)".into(),
                    nullable: false,
                    is_pk: false,
                    default_value: Some("-1".into()),
                },
                ColumnDef {
                    name: "byte_size".into(),
                    data_type: "INTEGER".into(),
                    nullable: false,
                    is_pk: false,
                    default_value: Some("0".into()),
                },
            ],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            ddl: format!("SCAN 0 MATCH {item} COUNT 100"),
        })
    }

    async fn fetch_item_data(&self, item: &str, opts: DataQueryOptions) -> AppResult<QueryResult> {
        let start = Instant::now();
        if self.target.starts_with("redis://") {
            let pattern = if item.is_empty() || item == "*" {
                "*"
            } else {
                item
            };
            let raw = send_resp_command(&self.target, &["KEYS", pattern]).await?;
            let keys: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
            let mut rows = Vec::new();
            for k in keys.into_iter().take(opts.limit.clamp(1, 200) as usize) {
                let typ = send_resp_command(&self.target, &["TYPE", &k])
                    .await
                    .unwrap_or_default();
                let ttl_str = send_resp_command(&self.target, &["TTL", &k])
                    .await
                    .unwrap_or_else(|_| "-1".into());
                let val = if typ.eq_ignore_ascii_case("string") {
                    send_resp_command(&self.target, &["GET", &k])
                        .await
                        .unwrap_or_default()
                } else {
                    format!("<{typ}>")
                };
                let size = val.len() as i64;
                rows.push(vec![
                    CellValue::Text(k),
                    CellValue::Text(typ.to_uppercase()),
                    CellValue::Text(val),
                    CellValue::Int(ttl_str.parse::<i64>().unwrap_or(-1)),
                    CellValue::Int(size),
                ]);
            }
            let total = rows.len() as i64;
            return Ok(QueryResult {
                columns: vec![
                    "key".into(),
                    "data_type".into(),
                    "value".into(),
                    "ttl_seconds".into(),
                    "byte_size".into(),
                ],
                column_types: vec![
                    "TEXT".into(),
                    "TEXT".into(),
                    "TEXT".into(),
                    "INTEGER".into(),
                    "INTEGER".into(),
                ],
                pk_columns: vec!["key".into()],
                rows,
                total_rows: Some(total),
                affected_rows: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                notice: None,
            });
        }

        let pool = self.require_pool().await?;
        let ns_filter = if item.is_empty() || item == "*" {
            None
        } else {
            Some(item.to_string())
        };
        let text_filter = opts
            .filter
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));

        let mut sql = String::from(
            "SELECT key, data_type, value_text, CASE WHEN ttl_seconds < 0 THEN -1 ELSE max(0, ttl_seconds - CAST((julianday('now') - julianday(updated_at))*86400 AS INTEGER)) END AS ttl_seconds, byte_size FROM _guidb_kv WHERE 1=1",
        );
        if ns_filter.is_some() {
            sql.push_str(" AND namespace = ?1");
        }
        if text_filter.is_some() {
            if ns_filter.is_some() {
                sql.push_str(" AND (key LIKE ?2 OR value_text LIKE ?2)");
            } else {
                sql.push_str(" AND (key LIKE ?1 OR value_text LIKE ?1)");
            }
        }
        sql.push_str(" ORDER BY key ASC LIMIT ? OFFSET ?");

        let mut q = sqlx::query(AssertSqlSafe(sql.as_str()));
        if let Some(ref ns) = ns_filter {
            q = q.bind(ns);
        }
        if let Some(ref tf) = text_filter {
            q = q.bind(tf);
        }

        let rows_raw = q
            .bind(opts.limit.clamp(1, 500))
            .bind(opts.offset.max(0))
            .fetch_all(pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<Vec<CellValue>> = rows_raw
            .into_iter()
            .map(|r| {
                vec![
                    CellValue::Text(r.get("key")),
                    CellValue::Text(r.get("data_type")),
                    CellValue::Text(r.get("value_text")),
                    CellValue::Int(r.get("ttl_seconds")),
                    CellValue::Int(r.get("byte_size")),
                ]
            })
            .collect();
        let total = rows.len() as i64;
        Ok(QueryResult {
            columns: vec![
                "key".into(),
                "data_type".into(),
                "value".into(),
                "ttl_seconds".into(),
                "byte_size".into(),
            ],
            column_types: vec![
                "TEXT".into(),
                "TEXT".into(),
                "TEXT".into(),
                "INTEGER".into(),
                "INTEGER".into(),
            ],
            pk_columns: vec!["key".into()],
            rows,
            total_rows: Some(total),
            affected_rows: 0,
            duration_ms: start.elapsed().as_millis() as u64,
            notice: None,
        })
    }

    async fn execute_query(&self, _item: Option<&str>, query: &str) -> AppResult<QueryResult> {
        let start = Instant::now();
        let parts = split_shell_words(query.trim());
        if parts.is_empty() {
            return Err(AppError::BadRequest("Redis command cannot be empty".into()));
        }
        if self.target.starts_with("redis://") {
            let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
            let resp = send_resp_command(&self.target, &refs).await?;
            return Ok(QueryResult {
                columns: vec!["command".into(), "response".into()],
                column_types: vec!["TEXT".into(), "TEXT".into()],
                pk_columns: Vec::new(),
                rows: vec![vec![
                    CellValue::Text(query.trim().to_string()),
                    CellValue::Text(resp),
                ]],
                total_rows: Some(1),
                affected_rows: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                notice: None,
            });
        }

        let pool = self.require_pool().await?;
        let cmd = parts[0].to_ascii_uppercase();
        let now = chrono::Utc::now().to_rfc3339();
        match cmd.as_str() {
            "PING" => Ok(single_kv_result("PONG", start)),
            "DBSIZE" => {
                let cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _guidb_kv")
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                Ok(single_kv_result(&cnt.to_string(), start))
            }
            "KEYS" | "SCAN" => {
                let pat = parts.get(1).map(String::as_str).unwrap_or("*");
                let sql_pat = pat.replace('*', "%");
                let rows = sqlx::query(
                    "SELECT key, data_type, value_text, CASE WHEN ttl_seconds < 0 THEN -1 ELSE max(0, ttl_seconds - CAST((julianday('now') - julianday(updated_at))*86400 AS INTEGER)) END AS ttl_seconds, byte_size FROM _guidb_kv WHERE key LIKE ?1 ORDER BY key ASC",
                )
                .bind(sql_pat)
                .fetch_all(pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
                let out: Vec<Vec<CellValue>> = rows
                    .into_iter()
                    .map(|r| {
                        vec![
                            CellValue::Text(r.get("key")),
                            CellValue::Text(r.get("data_type")),
                            CellValue::Text(r.get("value_text")),
                            CellValue::Int(r.get("ttl_seconds")),
                            CellValue::Int(r.get("byte_size")),
                        ]
                    })
                    .collect();
                let cnt = out.len() as i64;
                Ok(QueryResult {
                    columns: vec![
                        "key".into(),
                        "data_type".into(),
                        "value".into(),
                        "ttl_seconds".into(),
                        "byte_size".into(),
                    ],
                    column_types: vec![
                        "TEXT".into(),
                        "TEXT".into(),
                        "TEXT".into(),
                        "INTEGER".into(),
                        "INTEGER".into(),
                    ],
                    pk_columns: vec!["key".into()],
                    rows: out,
                    total_rows: Some(cnt),
                    affected_rows: 0,
                    duration_ms: start.elapsed().as_millis() as u64,
                    notice: None,
                })
            }
            "GET" | "HGETALL" => {
                let key = parts
                    .get(1)
                    .ok_or_else(|| AppError::BadRequest("Missing key argument".into()))?;
                let row = sqlx::query(
                    "SELECT key, data_type, value_text, CASE WHEN ttl_seconds < 0 THEN -1 ELSE max(0, ttl_seconds - CAST((julianday('now') - julianday(updated_at))*86400 AS INTEGER)) END AS ttl_seconds, byte_size FROM _guidb_kv WHERE key = ?1",
                )
                .bind(key)
                .fetch_optional(pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
                let Some(r) = row else {
                    return Ok(single_kv_result("(nil)", start));
                };
                let dt: String = r.get("data_type");
                let val: String = r.get("value_text");
                let ttl: i64 = r.get("ttl_seconds");
                let size: i64 = r.get("byte_size");
                Ok(QueryResult {
                    columns: vec![
                        "key".into(),
                        "data_type".into(),
                        "value".into(),
                        "ttl_seconds".into(),
                        "byte_size".into(),
                    ],
                    column_types: vec![
                        "TEXT".into(),
                        "TEXT".into(),
                        "TEXT".into(),
                        "INTEGER".into(),
                        "INTEGER".into(),
                    ],
                    pk_columns: vec!["key".into()],
                    rows: vec![vec![
                        CellValue::Text(key.clone()),
                        CellValue::Text(dt),
                        CellValue::Text(val),
                        CellValue::Int(ttl),
                        CellValue::Int(size),
                    ]],
                    total_rows: Some(1),
                    affected_rows: 0,
                    duration_ms: start.elapsed().as_millis() as u64,
                    notice: None,
                })
            }
            "SET" => {
                if parts.len() < 3 {
                    return Err(AppError::BadRequest("Usage: SET <key> <value> [EX <sec>]".into()));
                }
                let key = &parts[1];
                let val = &parts[2];
                let ttl = if parts.get(3).is_some_and(|s| s.eq_ignore_ascii_case("EX")) {
                    parts.get(4).and_then(|s| s.parse::<i64>().ok()).filter(|n| *n > 0).ok_or_else(|| AppError::BadRequest("EX requires positive seconds".into()))?
                } else {
                    -1
                };
                let ns = derive_namespace(key);
                sqlx::query(
                    "INSERT INTO _guidb_kv (key, namespace, data_type, value_text, ttl_seconds, byte_size, updated_at)
                     VALUES (?1, ?2, 'STRING', ?3, ?4, ?5, ?6)
                     ON CONFLICT(key) DO UPDATE SET value_text = excluded.value_text, ttl_seconds = excluded.ttl_seconds, byte_size = excluded.byte_size, updated_at = excluded.updated_at",
                )
                .bind(key)
                .bind(ns)
                .bind(val)
                .bind(ttl)
                .bind(val.len() as i64)
                .bind(&now)
                .execute(pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
                Ok(QueryResult {
                    columns: vec!["result".into()],
                    column_types: vec!["TEXT".into()],
                    pk_columns: Vec::new(),
                    rows: vec![vec![CellValue::Text("OK".into())]],
                    total_rows: Some(1),
                    affected_rows: 1,
                    duration_ms: start.elapsed().as_millis() as u64,
                    notice: Some(format!("Key '{key}' stored")),
                })
            }
            "EXPIRE" => {
                if parts.len() < 3 {
                    return Err(AppError::BadRequest("Usage: EXPIRE <key> <seconds>".into()));
                }
                let key = &parts[1];
                let ttl = parts[2].parse::<i64>().map_err(|_| AppError::BadRequest("Invalid expiration".into()))?.max(0);
                let res = sqlx::query("UPDATE _guidb_kv SET ttl_seconds = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE key = ?2")
                    .bind(ttl)
                    .bind(key)
                    .execute(pool)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                Ok(single_kv_result(&res.rows_affected().to_string(), start))
            }
            "TTL" => {
                let key = parts
                    .get(1)
                    .ok_or_else(|| AppError::BadRequest("Usage: TTL <key>".into()))?;
                let ttl: Option<i64> =
                    sqlx::query_scalar("SELECT CASE WHEN ttl_seconds < 0 THEN -1 ELSE max(0, ttl_seconds - CAST((julianday('now') - julianday(updated_at))*86400 AS INTEGER)) END FROM _guidb_kv WHERE key = ?1")
                        .bind(key)
                        .fetch_optional(pool)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?;
                Ok(single_kv_result(&ttl.unwrap_or(-2).to_string(), start))
            }
            "DEL" => {
                let key = parts
                    .get(1)
                    .ok_or_else(|| AppError::BadRequest("Usage: DEL <key>".into()))?;
                let res = sqlx::query("DELETE FROM _guidb_kv WHERE key = ?1")
                    .bind(key)
                    .execute(pool)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                Ok(QueryResult {
                    columns: vec!["deleted".into()],
                    column_types: vec!["INTEGER".into()],
                    pk_columns: Vec::new(),
                    rows: vec![vec![CellValue::Int(res.rows_affected() as i64)]],
                    total_rows: Some(1),
                    affected_rows: res.rows_affected(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    notice: None,
                })
            }
            "INFO" => {
                let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _guidb_kv")
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                let bytes: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(byte_size), 0) FROM _guidb_kv")
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                Ok(QueryResult {
                    columns: vec!["metric".into(), "value".into()],
                    column_types: vec!["TEXT".into(), "TEXT".into()],
                    pk_columns: Vec::new(),
                    rows: vec![
                        vec![CellValue::Text("engine".into()), CellValue::Text("GuiDB KV (Redis-Compatible)".into())],
                        vec![CellValue::Text("keys_total".into()), CellValue::Text(total.to_string())],
                        vec![CellValue::Text("used_payload_bytes".into()), CellValue::Text(bytes.to_string())],
                    ],
                    total_rows: Some(3),
                    affected_rows: 0,
                    duration_ms: start.elapsed().as_millis() as u64,
                    notice: None,
                })
            }
            _ => Err(AppError::BadRequest(format!(
                "Unsupported Redis command '{cmd}'. Try PING, KEYS *, GET <key>, SET <key> <val>, EXPIRE <key> <sec>, TTL <key>, DEL <key>, HGETALL <key>, DBSIZE, INFO"
            ))),
        }
    }

    async fn apply_staged_mutations(
        &self,
        mutations: &[StagedMutation],
    ) -> AppResult<MutationReport> {
        let start = Instant::now();
        let previews = preview_mutations(mutations, EngineKind::Redis);
        let pool = self.require_pool().await?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        for m in mutations {
            match m {
                StagedMutation::Insert { values, .. } => {
                    let key = values
                        .get("key")
                        .map(CellValue::display_text)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| AppError::BadRequest("Key is required".into()))?;
                    let dt = values
                        .get("data_type")
                        .map(CellValue::display_text)
                        .unwrap_or_else(|| "STRING".to_string());
                    let val = values
                        .get("value")
                        .map(CellValue::display_text)
                        .unwrap_or_default();
                    let ttl = values
                        .get("ttl_seconds")
                        .and_then(|v| v.display_text().parse::<i64>().ok())
                        .unwrap_or(-1);
                    let ns = derive_namespace(&key);
                    sqlx::query(
                        "INSERT INTO _guidb_kv (key, namespace, data_type, value_text, ttl_seconds, byte_size, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    )
                    .bind(&key)
                    .bind(ns)
                    .bind(dt)
                    .bind(&val)
                    .bind(ttl)
                    .bind(val.len() as i64)
                    .bind(&now)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                }
                StagedMutation::Update {
                    pk_match, changes, ..
                } => {
                    let key = pk_match
                        .get("key")
                        .map(CellValue::display_text)
                        .ok_or_else(|| AppError::BadRequest("Missing key for update".into()))?;
                    if let Some(v) = changes.get("value") {
                        let txt = v.display_text();
                        sqlx::query(
                            "UPDATE _guidb_kv SET value_text = ?1, byte_size = ?2 WHERE key = ?3",
                        )
                        .bind(&txt)
                        .bind(txt.len() as i64)
                        .bind(&key)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?;
                    }
                    if let Some(t) = changes.get("ttl_seconds") {
                        let ttl = t.display_text().parse::<i64>().unwrap_or(-1);
                        sqlx::query("UPDATE _guidb_kv SET ttl_seconds = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE key = ?2")
                            .bind(ttl)
                            .bind(&key)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| AppError::Database(e.to_string()))?;
                    }
                    if let Some(dt) = changes.get("data_type") {
                        sqlx::query("UPDATE _guidb_kv SET data_type = ?1 WHERE key = ?2")
                            .bind(dt.display_text())
                            .bind(&key)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| AppError::Database(e.to_string()))?;
                    }
                }
                StagedMutation::Delete { pk_match, .. } => {
                    let key = pk_match
                        .get("key")
                        .map(CellValue::display_text)
                        .ok_or_else(|| AppError::BadRequest("Missing key for delete".into()))?;
                    sqlx::query("DELETE FROM _guidb_kv WHERE key = ?1")
                        .bind(&key)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?;
                }
            }
        }
        tx.commit()
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(MutationReport {
            committed_count: mutations.len(),
            executed_statements: previews,
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }

    async fn execute_ddl(&self, action: DdlAction) -> AppResult<DdlReport> {
        let pool = self.require_pool().await?;
        match action {
            DdlAction::TruncateItem { item_name } => {
                if item_name == "*" {
                    sqlx::query("DELETE FROM _guidb_kv")
                        .execute(pool)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?;
                } else {
                    sqlx::query("DELETE FROM _guidb_kv WHERE namespace = ?1")
                        .bind(&item_name)
                        .execute(pool)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?;
                }
                Ok(DdlReport {
                    executed_sql: format!("FLUSH namespace {item_name}"),
                    message: format!("Cleared keys in '{item_name}'"),
                })
            }
            _ => Err(AppError::BadRequest(
                "Use SET/DEL commands or staged row editing for Redis keyspaces".into(),
            )),
        }
    }

    async fn build_erd(&self, _: &[ItemInfo]) -> AppResult<ErdGraph> {
        Ok(ErdGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
        })
    }
}

fn derive_namespace(key: &str) -> String {
    if let Some((prefix, _)) = key.split_once(':') {
        format!("{prefix}:*")
    } else {
        "default:*".to_string()
    }
}

fn single_kv_result(val: &str, start: Instant) -> QueryResult {
    QueryResult {
        columns: vec!["response".into()],
        column_types: vec!["TEXT".into()],
        pk_columns: Vec::new(),
        rows: vec![vec![CellValue::Text(val.to_string())]],
        total_rows: Some(1),
        affected_rows: 0,
        duration_ms: start.elapsed().as_millis() as u64,
        notice: None,
    }
}

fn split_shell_words(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote: Option<char> = None;
    for ch in input.chars() {
        if let Some(q) = in_quote {
            if ch == q {
                in_quote = None;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = Some(ch);
        } else if ch.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
