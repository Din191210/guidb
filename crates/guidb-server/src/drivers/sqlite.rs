use crate::drivers::{DataQueryOptions, DatabaseAdapter};
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use guidb_shared::sql_gen::{preview_mutations, quote_ident, validate_identifier};
use guidb_shared::{
    CellValue, ColumnDef, DdlAction, DdlReport, EngineKind, ErdEdge, ErdGraph, ErdNode,
    ForeignKeyDef, IndexDef, ItemInfo, MutationReport, QueryResult, StagedMutation, StructureDef,
    TestConnectionResponse,
};
use sqlx::sqlite::{SqlitePool, SqliteRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::collections::HashSet;
use std::time::Instant;

pub struct SqliteAdapter {
    pool: SqlitePool,
}

impl SqliteAdapter {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

pub fn decode_sqlite_row(row: &SqliteRow) -> Vec<CellValue> {
    (0..row.len())
        .map(|idx| {
            let Ok(raw) = row.try_get_raw(idx) else {
                return CellValue::Null;
            };
            if raw.is_null() {
                return CellValue::Null;
            }
            let rt_type = raw.type_info().name().to_uppercase();
            if rt_type.contains("INT") {
                if let Ok(v) = row.try_get::<i64, _>(idx) {
                    return CellValue::Int(v);
                }
            }
            if rt_type.contains("REAL") || rt_type.contains("FLOAT") || rt_type.contains("DOUBLE") {
                if let Ok(v) = row.try_get::<f64, _>(idx) {
                    return CellValue::Float(v);
                }
            }
            if rt_type.contains("BOOL") {
                if let Ok(v) = row.try_get::<bool, _>(idx) {
                    return CellValue::Bool(v);
                }
            }
            if rt_type.contains("BLOB") {
                if let Ok(bytes) = row.try_get::<Vec<u8>, _>(idx) {
                    return CellValue::Bytes(format!("<BLOB {} bytes>", bytes.len()));
                }
            }
            if let Ok(s) = row.try_get::<String, _>(idx) {
                return CellValue::Text(s);
            }
            if let Ok(i) = row.try_get::<i64, _>(idx) {
                return CellValue::Int(i);
            }
            if let Ok(f) = row.try_get::<f64, _>(idx) {
                return CellValue::Float(f);
            }
            CellValue::Null
        })
        .collect()
}

fn bind_cell<'q>(
    q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    val: &'q CellValue,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    match val {
        CellValue::Null => q.bind(Option::<String>::None),
        CellValue::Bool(b) => q.bind(if *b { 1_i64 } else { 0_i64 }),
        CellValue::Int(i) => q.bind(*i),
        CellValue::Float(f) => q.bind(*f),
        CellValue::Text(s) => q.bind(s.as_str()),
        CellValue::Json(v) => q.bind(v.to_string()),
        CellValue::Bytes(b) => q.bind(b.as_str()),
    }
}

#[async_trait]
impl DatabaseAdapter for SqliteAdapter {
    async fn test_connection(&self) -> AppResult<TestConnectionResponse> {
        let start = Instant::now();
        let ver: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(TestConnectionResponse {
            reachable: true,
            latency_ms: start.elapsed().as_millis() as u64,
            server_version: format!("SQLite {ver}"),
            details: "SQLite connection active".to_string(),
        })
    }

    async fn list_items(&self) -> AppResult<Vec<ItemInfo>> {
        let rows = sqlx::query(
            "SELECT name, type FROM sqlite_master
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'
             ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END, name ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

        let mut items = Vec::with_capacity(rows.len());
        for r in rows {
            let name: String = r.get("name");
            let item_type: String = r.get("type");
            let quoted = quote_ident(&name, EngineKind::Sqlite);
            let count_sql = format!("SELECT COUNT(*) FROM {quoted}");
            let row_count: i64 = sqlx::query_scalar(&count_sql)
                .fetch_one(&self.pool)
                .await
                .unwrap_or(0);
            items.push(ItemInfo {
                name,
                item_type: item_type.to_uppercase(),
                row_count,
            });
        }
        Ok(items)
    }

    async fn inspect_structure(&self, item: &str) -> AppResult<StructureDef> {
        let valid_name = validate_identifier(item).map_err(AppError::BadRequest)?;
        let master_row = sqlx::query(
            "SELECT type, COALESCE(sql, '') AS ddl FROM sqlite_master WHERE name = ?1 AND type IN ('table', 'view')",
        )
        .bind(&valid_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("Table or view '{valid_name}' not found")))?;

        let item_type: String = master_row.get("type");
        let ddl: String = master_row.get("ddl");
        let quoted = quote_ident(&valid_name, EngineKind::Sqlite);

        let col_rows = sqlx::query(&format!("PRAGMA table_info({quoted})"))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let columns: Vec<ColumnDef> = col_rows
            .into_iter()
            .map(|r| ColumnDef {
                name: r.get("name"),
                data_type: r.get::<String, _>("type"),
                nullable: r.get::<i64, _>("notnull") == 0,
                is_pk: r.get::<i64, _>("pk") > 0,
                default_value: r.get("dflt_value"),
            })
            .collect();

        let idx_rows = sqlx::query(&format!("PRAGMA index_list({quoted})"))
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

        let mut indexes = Vec::new();
        for idx in idx_rows {
            let idx_name: String = idx.get("name");
            if idx_name.starts_with("sqlite_autoindex_") {
                continue;
            }
            let unique = idx.get::<i64, _>("unique") != 0;
            let iq = quote_ident(&idx_name, EngineKind::Sqlite);
            let idx_cols = sqlx::query(&format!("PRAGMA index_info({iq})"))
                .fetch_all(&self.pool)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|c| c.get::<Option<String>, _>("name"))
                .collect();
            indexes.push(IndexDef {
                name: idx_name,
                columns: idx_cols,
                unique,
            });
        }

        let fk_rows = sqlx::query(&format!("PRAGMA foreign_key_list({quoted})"))
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

        let foreign_keys: Vec<ForeignKeyDef> = fk_rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| ForeignKeyDef {
                name: format!("fk_{valid_name}_{i}"),
                from_column: r.get("from"),
                to_table: r.get("table"),
                to_column: r.get("to"),
                on_delete: r.get("on_delete"),
            })
            .collect();

        Ok(StructureDef {
            item_name: valid_name,
            item_type: item_type.to_uppercase(),
            columns,
            indexes,
            foreign_keys,
            ddl,
        })
    }

    async fn fetch_item_data(&self, item: &str, opts: DataQueryOptions) -> AppResult<QueryResult> {
        let start = Instant::now();
        let structure = self.inspect_structure(item).await?;
        let quoted_table = quote_ident(&structure.item_name, EngineKind::Sqlite);

        let col_names: Vec<String> = structure.columns.iter().map(|c| c.name.clone()).collect();
        let col_types: Vec<String> = structure
            .columns
            .iter()
            .map(|c| c.data_type.clone())
            .collect();
        let pk_columns: Vec<String> = structure
            .columns
            .iter()
            .filter(|c| c.is_pk)
            .map(|c| c.name.clone())
            .collect();

        let valid_col_set: HashSet<&str> = col_names.iter().map(String::as_str).collect();
        let mut where_clause = String::new();
        let filter_pattern = opts
            .filter
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));

        if filter_pattern.is_some() && !col_names.is_empty() {
            let predicates: Vec<String> = col_names
                .iter()
                .map(|c| {
                    format!(
                        "CAST({} AS TEXT) LIKE ?1",
                        quote_ident(c, EngineKind::Sqlite)
                    )
                })
                .collect();
            where_clause = format!(" WHERE ({})", predicates.join(" OR "));
        }

        let order_clause = match opts.sort_col.as_deref() {
            Some(col) if valid_col_set.contains(col) => {
                let dir = if opts
                    .sort_dir
                    .as_deref()
                    .is_some_and(|d| d.eq_ignore_ascii_case("desc"))
                {
                    "DESC"
                } else {
                    "ASC"
                };
                format!(" ORDER BY {} {dir}", quote_ident(col, EngineKind::Sqlite))
            }
            _ => String::new(),
        };

        let limit = opts.limit.clamp(1, 500);
        let offset = opts.offset.max(0);

        let count_sql = format!("SELECT COUNT(*) FROM {quoted_table}{where_clause}");
        let total_rows: i64 = if let Some(ref pat) = filter_pattern {
            sqlx::query_scalar(&count_sql)
                .bind(pat)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?
        } else {
            sqlx::query_scalar(&count_sql)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?
        };

        let data_sql =
            format!("SELECT * FROM {quoted_table}{where_clause}{order_clause} LIMIT {limit} OFFSET {offset}");
        let rows_raw = if let Some(ref pat) = filter_pattern {
            sqlx::query(&data_sql)
                .bind(pat)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?
        } else {
            sqlx::query(&data_sql)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?
        };

        let rows = rows_raw.iter().map(decode_sqlite_row).collect();
        Ok(QueryResult {
            columns: col_names,
            column_types: col_types,
            pk_columns,
            rows,
            total_rows: Some(total_rows),
            affected_rows: 0,
            duration_ms: start.elapsed().as_millis() as u64,
            notice: None,
        })
    }

    async fn execute_query(&self, _item: Option<&str>, query: &str) -> AppResult<QueryResult> {
        let start = Instant::now();
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(AppError::BadRequest("Query cannot be empty".into()));
        }
        let upper = trimmed.to_uppercase();
        let returns_rows = upper.starts_with("SELECT")
            || upper.starts_with("PRAGMA")
            || upper.starts_with("WITH")
            || upper.starts_with("EXPLAIN");

        if returns_rows {
            let rows_raw = sqlx::query(trimmed)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;

            let mut columns = Vec::new();
            let mut column_types = Vec::new();
            if let Some(first) = rows_raw.first() {
                for col in first.columns() {
                    columns.push(col.name().to_string());
                    column_types.push(col.type_info().name().to_string());
                }
            }
            let rows: Vec<Vec<CellValue>> = rows_raw.iter().map(decode_sqlite_row).collect();
            let count = rows.len() as i64;
            return Ok(QueryResult {
                columns,
                column_types,
                pk_columns: Vec::new(),
                rows,
                total_rows: Some(count),
                affected_rows: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                notice: None,
            });
        }

        let res = sqlx::raw_sql(trimmed)
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let affected = res.rows_affected();
        Ok(QueryResult {
            columns: vec!["status".into(), "affected_rows".into()],
            column_types: vec!["TEXT".into(), "INTEGER".into()],
            pk_columns: Vec::new(),
            rows: vec![vec![
                CellValue::Text("Executed".into()),
                CellValue::Int(affected as i64),
            ]],
            total_rows: Some(1),
            affected_rows: affected,
            duration_ms: start.elapsed().as_millis() as u64,
            notice: Some(format!("Statement executed; {affected} rows affected.")),
        })
    }

    async fn apply_staged_mutations(
        &self,
        mutations: &[StagedMutation],
    ) -> AppResult<MutationReport> {
        if mutations.is_empty() {
            return Err(AppError::BadRequest("No staged mutations provided".into()));
        }
        let start = Instant::now();
        let previews = preview_mutations(mutations, EngineKind::Sqlite);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        for m in mutations {
            match m {
                StagedMutation::Insert { item, values } => {
                    let table = validate_identifier(item).map_err(AppError::BadRequest)?;
                    if values.is_empty() {
                        return Err(AppError::BadRequest("Insert payload has no columns".into()));
                    }
                    let mut cols = Vec::with_capacity(values.len());
                    let mut placeholders = Vec::with_capacity(values.len());
                    for (i, col) in values.keys().enumerate() {
                        let valid_col = validate_identifier(col).map_err(AppError::BadRequest)?;
                        cols.push(quote_ident(&valid_col, EngineKind::Sqlite));
                        placeholders.push(format!("?{}", i + 1));
                    }
                    let sql = format!(
                        "INSERT INTO {} ({}) VALUES ({})",
                        quote_ident(&table, EngineKind::Sqlite),
                        cols.join(", "),
                        placeholders.join(", ")
                    );
                    let mut q = sqlx::query(&sql);
                    for val in values.values() {
                        q = bind_cell(q, val);
                    }
                    q.execute(&mut *tx)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?;
                }
                StagedMutation::Update {
                    item,
                    pk_match,
                    changes,
                } => {
                    let table = validate_identifier(item).map_err(AppError::BadRequest)?;
                    if pk_match.is_empty() || changes.is_empty() {
                        return Err(AppError::BadRequest(
                            "Update requires primary key match and changed columns".into(),
                        ));
                    }
                    let mut bind_idx = 1_usize;
                    let mut set_parts = Vec::with_capacity(changes.len());
                    for col in changes.keys() {
                        let valid_col = validate_identifier(col).map_err(AppError::BadRequest)?;
                        set_parts.push(format!(
                            "{} = ?{bind_idx}",
                            quote_ident(&valid_col, EngineKind::Sqlite)
                        ));
                        bind_idx += 1;
                    }
                    let mut where_parts = Vec::with_capacity(pk_match.len());
                    for (col, val) in pk_match {
                        let valid_col = validate_identifier(col).map_err(AppError::BadRequest)?;
                        if matches!(val, CellValue::Null) {
                            where_parts.push(format!(
                                "{} IS NULL",
                                quote_ident(&valid_col, EngineKind::Sqlite)
                            ));
                        } else {
                            where_parts.push(format!(
                                "{} = ?{bind_idx}",
                                quote_ident(&valid_col, EngineKind::Sqlite)
                            ));
                            bind_idx += 1;
                        }
                    }
                    let sql = format!(
                        "UPDATE {} SET {} WHERE {}",
                        quote_ident(&table, EngineKind::Sqlite),
                        set_parts.join(", "),
                        where_parts.join(" AND ")
                    );
                    let mut q = sqlx::query(&sql);
                    for val in changes.values() {
                        q = bind_cell(q, val);
                    }
                    for val in pk_match.values() {
                        if !matches!(val, CellValue::Null) {
                            q = bind_cell(q, val);
                        }
                    }
                    q.execute(&mut *tx)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?;
                }
                StagedMutation::Delete { item, pk_match } => {
                    let table = validate_identifier(item).map_err(AppError::BadRequest)?;
                    if pk_match.is_empty() {
                        return Err(AppError::BadRequest(
                            "Delete requires primary key match".into(),
                        ));
                    }
                    let mut bind_idx = 1_usize;
                    let mut where_parts = Vec::with_capacity(pk_match.len());
                    for (col, val) in pk_match {
                        let valid_col = validate_identifier(col).map_err(AppError::BadRequest)?;
                        if matches!(val, CellValue::Null) {
                            where_parts.push(format!(
                                "{} IS NULL",
                                quote_ident(&valid_col, EngineKind::Sqlite)
                            ));
                        } else {
                            where_parts.push(format!(
                                "{} = ?{bind_idx}",
                                quote_ident(&valid_col, EngineKind::Sqlite)
                            ));
                            bind_idx += 1;
                        }
                    }
                    let sql = format!(
                        "DELETE FROM {} WHERE {}",
                        quote_ident(&table, EngineKind::Sqlite),
                        where_parts.join(" AND ")
                    );
                    let mut q = sqlx::query(&sql);
                    for val in pk_match.values() {
                        if !matches!(val, CellValue::Null) {
                            q = bind_cell(q, val);
                        }
                    }
                    q.execute(&mut *tx)
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
        let sql = match action {
            DdlAction::CreateTable {
                table_name,
                columns,
            } => {
                let valid_table = validate_identifier(&table_name).map_err(AppError::BadRequest)?;
                if columns.is_empty() {
                    return Err(AppError::BadRequest(
                        "At least one column is required".into(),
                    ));
                }
                let mut col_defs = Vec::with_capacity(columns.len());
                for c in columns {
                    let cname = validate_identifier(&c.name).map_err(AppError::BadRequest)?;
                    let ctype = validate_sql_type(&c.data_type)?;
                    let mut def = format!("{} {ctype}", quote_ident(&cname, EngineKind::Sqlite));
                    if c.is_pk {
                        def.push_str(" PRIMARY KEY");
                        if ctype == "INTEGER" {
                            def.push_str(" AUTOINCREMENT");
                        }
                    } else if !c.nullable {
                        def.push_str(" NOT NULL");
                    }
                    if let Some(d) = c
                        .default_value
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        def.push_str(&format!(" DEFAULT {}", sanitize_default(d)));
                    }
                    col_defs.push(def);
                }
                format!(
                    "CREATE TABLE {} (\n  {}\n);",
                    quote_ident(&valid_table, EngineKind::Sqlite),
                    col_defs.join(",\n  ")
                )
            }
            DdlAction::AddColumn { table_name, column } => {
                let valid_table = validate_identifier(&table_name).map_err(AppError::BadRequest)?;
                let cname = validate_identifier(&column.name).map_err(AppError::BadRequest)?;
                let ctype = validate_sql_type(&column.data_type)?;
                let mut def = format!(
                    "ALTER TABLE {} ADD COLUMN {} {ctype}",
                    quote_ident(&valid_table, EngineKind::Sqlite),
                    quote_ident(&cname, EngineKind::Sqlite)
                );
                if !column.nullable {
                    let fallback = column
                        .default_value
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(if ctype.contains("INT") { "0" } else { "''" });
                    def.push_str(&format!(" NOT NULL DEFAULT {}", sanitize_default(fallback)));
                } else if let Some(d) = column
                    .default_value
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    def.push_str(&format!(" DEFAULT {}", sanitize_default(d)));
                }
                def.push(';');
                def
            }
            DdlAction::CreateIndex {
                table_name,
                index_name,
                columns,
                unique,
            } => {
                let valid_table = validate_identifier(&table_name).map_err(AppError::BadRequest)?;
                let valid_idx = validate_identifier(&index_name).map_err(AppError::BadRequest)?;
                if columns.is_empty() {
                    return Err(AppError::BadRequest("Select at least one column".into()));
                }
                let mut quoted_cols = Vec::with_capacity(columns.len());
                for c in columns {
                    let vc = validate_identifier(&c).map_err(AppError::BadRequest)?;
                    quoted_cols.push(quote_ident(&vc, EngineKind::Sqlite));
                }
                let unique_kw = if unique { "UNIQUE " } else { "" };
                format!(
                    "CREATE {unique_kw}INDEX {} ON {} ({});",
                    quote_ident(&valid_idx, EngineKind::Sqlite),
                    quote_ident(&valid_table, EngineKind::Sqlite),
                    quoted_cols.join(", ")
                )
            }
            DdlAction::TruncateItem { item_name } => {
                let valid_table = validate_identifier(&item_name).map_err(AppError::BadRequest)?;
                format!(
                    "DELETE FROM {};",
                    quote_ident(&valid_table, EngineKind::Sqlite)
                )
            }
            DdlAction::DropItem { item_name } => {
                let valid_table = validate_identifier(&item_name).map_err(AppError::BadRequest)?;
                format!(
                    "DROP TABLE {};",
                    quote_ident(&valid_table, EngineKind::Sqlite)
                )
            }
        };

        sqlx::raw_sql(&sql)
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(DdlReport {
            executed_sql: sql,
            message: "DDL operation applied successfully".to_string(),
        })
    }

    async fn build_erd(&self) -> AppResult<ErdGraph> {
        let items = self.list_items().await?;
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for item in items.into_iter().filter(|i| i.item_type == "TABLE") {
            let st = self.inspect_structure(&item.name).await?;
            for fk in &st.foreign_keys {
                edges.push(ErdEdge {
                    from_table: st.item_name.clone(),
                    from_column: fk.from_column.clone(),
                    to_table: fk.to_table.clone(),
                    to_column: fk.to_column.clone(),
                });
            }
            nodes.push(ErdNode {
                table_name: st.item_name,
                row_count: item.row_count,
                columns: st.columns,
            });
        }
        Ok(ErdGraph { nodes, edges })
    }
}

fn validate_sql_type(raw: &str) -> AppResult<String> {
    let trimmed = raw.trim().to_uppercase();
    let allowed = [
        "INTEGER",
        "INT",
        "BIGINT",
        "TEXT",
        "VARCHAR(255)",
        "REAL",
        "FLOAT",
        "NUMERIC",
        "BOOLEAN",
        "BLOB",
        "TIMESTAMP",
        "JSON",
    ];
    if allowed.contains(&trimmed.as_str())
        || (trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '(' | ')' | ',' | ' ' | '_'))
            && !trimmed.is_empty()
            && trimmed.len() <= 32)
    {
        Ok(trimmed)
    } else {
        Err(AppError::BadRequest(format!(
            "Unsupported data type '{raw}'"
        )))
    }
}

fn sanitize_default(raw: &str) -> String {
    let t = raw.trim();
    if t.eq_ignore_ascii_case("NULL")
        || t.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
        || t.parse::<f64>().is_ok()
        || (t.starts_with('\'') && t.ends_with('\''))
    {
        t.to_string()
    } else {
        format!("'{}'", t.replace('\'', "''"))
    }
}
