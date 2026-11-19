use super::ddl_ident;
use crate::drivers::{DataQueryOptions, DatabaseAdapter};
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use guidb_shared::sql_gen::{preview_mutations, quote_ident, validate_identifier};
use guidb_shared::{
    CellValue, ColumnDef, DdlAction, DdlReport, EngineKind, ErdEdge, ErdGraph, ErdNode,
    ForeignKeyDef, IndexDef, ItemInfo, MutationReport, QueryResult, StagedMutation, StructureDef,
    TestConnectionResponse,
};
use sqlx::mysql::{MySqlPool, MySqlRow};
use sqlx::AssertSqlSafe;
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::collections::HashSet;
use std::time::Instant;

pub struct MysqlAdapter {
    pool: MySqlPool,
    read_only: bool,
}

impl MysqlAdapter {
    pub fn new(pool: MySqlPool, read_only: bool) -> Self {
        Self { pool, read_only }
    }
}

fn decode_mysql_row(row: &MySqlRow) -> Vec<CellValue> {
    (0..row.len())
        .map(|idx| {
            let Ok(raw) = row.try_get_raw(idx) else {
                return CellValue::Null;
            };
            if raw.is_null() {
                return CellValue::Null;
            }
            let ty = raw.type_info().name().to_uppercase();
            if ty.contains("BOOL") || ty == "TINYINT(1)" {
                if let Ok(v) = row.try_get::<bool, _>(idx) {
                    return CellValue::Bool(v);
                }
            }
            if ty.contains("INT") {
                if let Ok(v) = row.try_get::<i64, _>(idx) {
                    return CellValue::Int(v);
                }
            }
            if ty.contains("FLOAT") || ty.contains("DOUBLE") || ty.contains("DECIMAL") {
                if let Ok(v) = row.try_get::<f64, _>(idx) {
                    return CellValue::Float(v);
                }
            }
            if ty == "JSON" {
                if let Ok(v) = row.try_get::<serde_json::Value, _>(idx) {
                    return CellValue::Json(v);
                }
            }
            if let Ok(s) = row.try_get::<String, _>(idx) {
                return CellValue::Text(s);
            }
            CellValue::Bytes(format!("<{ty}>"))
        })
        .collect()
}

fn bind_mysql<'q>(
    q: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    val: &'q CellValue,
) -> sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    match val {
        CellValue::Null => q.bind(Option::<String>::None),
        CellValue::Bool(b) => q.bind(*b),
        CellValue::Int(i) => q.bind(*i),
        CellValue::Float(f) => q.bind(*f),
        CellValue::Text(s) => q.bind(s.as_str()),
        CellValue::Json(v) => q.bind(v.to_string()),
        CellValue::Bytes(b) => q.bind(b.as_str()),
    }
}

#[async_trait]
impl DatabaseAdapter for MysqlAdapter {
    async fn test_connection(&self) -> AppResult<TestConnectionResponse> {
        let start = Instant::now();
        let ver: String = sqlx::query_scalar("SELECT VERSION()")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(TestConnectionResponse {
            reachable: true,
            latency_ms: start.elapsed().as_millis() as u64,
            server_version: format!("MySQL/MariaDB {ver}"),
            details: "MySQL pool connected".to_string(),
        })
    }

    async fn list_items(&self) -> AppResult<Vec<ItemInfo>> {
        let rows = sqlx::query(
            "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES
             WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_TYPE ASC, TABLE_NAME ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let name: String = r.get("TABLE_NAME");
            let tt: String = r.get("TABLE_TYPE");
            let q = quote_ident(&name, EngineKind::Mysql);
            let cnt: i64 = sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM {q}")))
                .fetch_one(&self.pool)
                .await
                .unwrap_or(0);
            out.push(ItemInfo {
                name,
                item_type: if tt.contains("VIEW") { "VIEW" } else { "TABLE" }.to_string(),
                row_count: cnt,
            });
        }
        Ok(out)
    }

    async fn inspect_structure(&self, item: &str) -> AppResult<StructureDef> {
        let valid = validate_identifier(item).map_err(AppError::BadRequest)?;
        let col_rows = sqlx::query(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT
             FROM information_schema.COLUMNS
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?
             ORDER BY ORDINAL_POSITION ASC",
        )
        .bind(&valid)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

        if col_rows.is_empty() {
            return Err(AppError::NotFound(format!("Table '{valid}' not found")));
        }

        let columns: Vec<ColumnDef> = col_rows
            .into_iter()
            .map(|r| ColumnDef {
                name: r.get("COLUMN_NAME"),
                data_type: r.get::<String, _>("COLUMN_TYPE").to_uppercase(),
                nullable: r.get::<String, _>("IS_NULLABLE") == "YES",
                is_pk: r.get::<String, _>("COLUMN_KEY") == "PRI",
                default_value: r.get("COLUMN_DEFAULT"),
            })
            .collect();

        let fk_rows = sqlx::query(
            "SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME
             FROM information_schema.KEY_COLUMN_USAGE
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL",
        )
        .bind(&valid)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let foreign_keys: Vec<ForeignKeyDef> = fk_rows
            .into_iter()
            .map(|r| ForeignKeyDef {
                name: r.get("CONSTRAINT_NAME"),
                from_column: r.get("COLUMN_NAME"),
                to_table: r.get("REFERENCED_TABLE_NAME"),
                to_column: r.get("REFERENCED_COLUMN_NAME"),
                on_delete: "RESTRICT".to_string(),
            })
            .collect();

        let idx_rows = sqlx::query(
            "SELECT DISTINCT INDEX_NAME, NON_UNIQUE FROM information_schema.STATISTICS
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?",
        )
        .bind(&valid)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();
        let indexes: Vec<IndexDef> = idx_rows
            .into_iter()
            .map(|r| IndexDef {
                name: r.get("INDEX_NAME"),
                columns: Vec::new(),
                unique: r.get::<i64, _>("NON_UNIQUE") == 0,
            })
            .collect();

        let ddl_cols: Vec<String> = columns
            .iter()
            .map(|c| {
                let nn = if !c.nullable { " NOT NULL" } else { "" };
                let pk = if c.is_pk { " PRIMARY KEY" } else { "" };
                format!(
                    "  {} {}{}{}",
                    quote_ident(&c.name, EngineKind::Mysql),
                    c.data_type,
                    pk,
                    nn
                )
            })
            .collect();
        let ddl = format!(
            "CREATE TABLE {} (\n{}\n);",
            quote_ident(&valid, EngineKind::Mysql),
            ddl_cols.join(",\n")
        );

        Ok(StructureDef {
            item_name: valid,
            item_type: "TABLE".to_string(),
            columns,
            indexes,
            foreign_keys,
            ddl,
        })
    }

    async fn fetch_item_data(&self, item: &str, opts: DataQueryOptions) -> AppResult<QueryResult> {
        let start = Instant::now();
        let st = self.inspect_structure(item).await?;
        let qt = quote_ident(&st.item_name, EngineKind::Mysql);
        let col_names: Vec<String> = st.columns.iter().map(|c| c.name.clone()).collect();
        let col_types: Vec<String> = st.columns.iter().map(|c| c.data_type.clone()).collect();
        let pk_columns: Vec<String> = st
            .columns
            .iter()
            .filter(|c| c.is_pk)
            .map(|c| c.name.clone())
            .collect();
        let valid_set: HashSet<&str> = col_names.iter().map(String::as_str).collect();

        let filter_pat = opts
            .filter
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
        let where_clause = if filter_pat.is_some() && !col_names.is_empty() {
            let preds: Vec<String> = col_names
                .iter()
                .map(|c| format!("CAST({} AS CHAR) LIKE ?", quote_ident(c, EngineKind::Mysql)))
                .collect();
            format!(" WHERE ({})", preds.join(" OR "))
        } else {
            String::new()
        };

        let order_clause = match opts.sort_col.as_deref() {
            Some(c) if valid_set.contains(c) => {
                let dir = if opts
                    .sort_dir
                    .as_deref()
                    .is_some_and(|d| d.eq_ignore_ascii_case("desc"))
                {
                    "DESC"
                } else {
                    "ASC"
                };
                format!(" ORDER BY {} {dir}", quote_ident(c, EngineKind::Mysql))
            }
            _ if !pk_columns.is_empty() => format!(
                " ORDER BY {}",
                pk_columns
                    .iter()
                    .map(|c| quote_ident(c, EngineKind::Mysql))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => String::new(),
        };

        let limit = opts.limit.clamp(1, 500);
        let offset = opts.offset.max(0);
        let count_sql = format!("SELECT COUNT(*) FROM {qt}{where_clause}");
        let mut cq = sqlx::query_scalar::<_, i64>(AssertSqlSafe(count_sql));
        if let Some(ref pat) = filter_pat {
            for _ in &col_names {
                cq = cq.bind(pat);
            }
        }
        let total = cq
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let data_sql =
            format!("SELECT * FROM {qt}{where_clause}{order_clause} LIMIT {limit} OFFSET {offset}");
        let mut dq = sqlx::query(AssertSqlSafe(data_sql));
        if let Some(ref pat) = filter_pat {
            for _ in &col_names {
                dq = dq.bind(pat);
            }
        }
        let rows_raw = dq
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows = rows_raw.iter().map(decode_mysql_row).collect();
        Ok(QueryResult {
            columns: col_names,
            column_types: col_types,
            pk_columns,
            rows,
            total_rows: Some(total),
            affected_rows: 0,
            duration_ms: start.elapsed().as_millis() as u64,
            notice: None,
        })
    }

    async fn execute_query(&self, _item: Option<&str>, query: &str) -> AppResult<QueryResult> {
        let start = Instant::now();
        let trimmed = query.trim();
        let upper = trimmed.to_uppercase();
        if self.read_only
            || upper.starts_with("SELECT")
            || upper.starts_with("SHOW")
            || upper.starts_with("EXPLAIN")
            || upper.starts_with("DESCRIBE")
        {
            let rows_raw = if self.read_only {
                let mut tx = self
                    .pool
                    .begin_with("START TRANSACTION READ ONLY")
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                let rows = sqlx::query(AssertSqlSafe(trimmed))
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                tx.rollback()
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                rows
            } else {
                sqlx::query(AssertSqlSafe(trimmed))
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?
            };
            let mut columns = Vec::new();
            let mut column_types = Vec::new();
            if let Some(first) = rows_raw.first() {
                for col in first.columns() {
                    columns.push(col.name().to_string());
                    column_types.push(col.type_info().name().to_string());
                }
            }
            let rows: Vec<Vec<CellValue>> = rows_raw.iter().map(decode_mysql_row).collect();
            let total = rows.len() as i64;
            return Ok(QueryResult {
                columns,
                column_types,
                pk_columns: Vec::new(),
                rows,
                total_rows: Some(total),
                affected_rows: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                notice: None,
            });
        }
        let res = sqlx::raw_sql(AssertSqlSafe(trimmed))
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(QueryResult {
            columns: vec!["status".into(), "affected_rows".into()],
            column_types: vec!["TEXT".into(), "BIGINT".into()],
            pk_columns: Vec::new(),
            rows: vec![vec![
                CellValue::Text("Executed".into()),
                CellValue::Int(res.rows_affected() as i64),
            ]],
            total_rows: Some(1),
            affected_rows: res.rows_affected(),
            duration_ms: start.elapsed().as_millis() as u64,
            notice: Some(format!("{} rows affected", res.rows_affected())),
        })
    }

    async fn apply_staged_mutations(
        &self,
        mutations: &[StagedMutation],
    ) -> AppResult<MutationReport> {
        let start = Instant::now();
        let previews = preview_mutations(mutations, EngineKind::Mysql);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        for m in mutations {
            match m {
                StagedMutation::Insert { item, values } => {
                    let table = validate_identifier(item).map_err(AppError::BadRequest)?;
                    let cols: Vec<String> = values
                        .keys()
                        .map(|c| quote_ident(c, EngineKind::Mysql))
                        .collect();
                    let ph: Vec<&str> = vec!["?"; cols.len()];
                    let sql = format!(
                        "INSERT INTO {} ({}) VALUES ({})",
                        quote_ident(&table, EngineKind::Mysql),
                        cols.join(", "),
                        ph.join(", ")
                    );
                    let mut q = sqlx::query(AssertSqlSafe(sql.as_str()));
                    for v in values.values() {
                        q = bind_mysql(q, v);
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
                    let sets: Vec<String> = changes
                        .keys()
                        .map(|c| format!("{} = ?", quote_ident(c, EngineKind::Mysql)))
                        .collect();
                    let wheres: Vec<String> = pk_match
                        .iter()
                        .map(|(c, v)| {
                            if matches!(v, CellValue::Null) {
                                format!("{} IS NULL", quote_ident(c, EngineKind::Mysql))
                            } else {
                                format!("{} = ?", quote_ident(c, EngineKind::Mysql))
                            }
                        })
                        .collect();
                    let sql = format!(
                        "UPDATE {} SET {} WHERE {}",
                        quote_ident(&table, EngineKind::Mysql),
                        sets.join(", "),
                        wheres.join(" AND ")
                    );
                    let mut q = sqlx::query(AssertSqlSafe(sql.as_str()));
                    for v in changes.values() {
                        q = bind_mysql(q, v);
                    }
                    for v in pk_match.values() {
                        if !matches!(v, CellValue::Null) {
                            q = bind_mysql(q, v);
                        }
                    }
                    let affected = q
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?
                        .rows_affected();
                    if affected != 1 {
                        return Err(AppError::BadRequest(
                            "Baris telah berubah atau kunci tidak unik. Muat ulang tabel.".into(),
                        ));
                    }
                }
                StagedMutation::Delete { item, pk_match } => {
                    let table = validate_identifier(item).map_err(AppError::BadRequest)?;
                    let wheres: Vec<String> = pk_match
                        .iter()
                        .map(|(c, v)| {
                            if matches!(v, CellValue::Null) {
                                format!("{} IS NULL", quote_ident(c, EngineKind::Mysql))
                            } else {
                                format!("{} = ?", quote_ident(c, EngineKind::Mysql))
                            }
                        })
                        .collect();
                    let sql = format!(
                        "DELETE FROM {} WHERE {}",
                        quote_ident(&table, EngineKind::Mysql),
                        wheres.join(" AND ")
                    );
                    let mut q = sqlx::query(AssertSqlSafe(sql.as_str()));
                    for v in pk_match.values() {
                        if !matches!(v, CellValue::Null) {
                            q = bind_mysql(q, v);
                        }
                    }
                    let affected = q
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?
                        .rows_affected();
                    if affected != 1 {
                        return Err(AppError::BadRequest(
                            "Baris telah berubah atau kunci tidak unik. Muat ulang tabel.".into(),
                        ));
                    }
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
            DdlAction::CopyTable {
                source_table,
                table_name,
            } => {
                let engine = EngineKind::Mysql;
                let source = super::ddl_ident(&source_table, engine)?;
                let target = super::ddl_ident(&table_name, engine)?;
                let structure = self.inspect_structure(&source_table).await?;
                let primary = structure
                    .columns
                    .iter()
                    .filter(|c| c.is_pk)
                    .map(|c| quote_ident(&c.name, engine))
                    .collect::<Vec<_>>()
                    .join(", ");
                let definition = if primary.is_empty() {
                    String::new()
                } else {
                    format!(" (PRIMARY KEY ({primary}))")
                };
                format!("CREATE TABLE {target}{definition} ENGINE=InnoDB AS SELECT * FROM {source}")
            }
            DdlAction::CreateTable {
                table_name,
                columns,
            } => {
                if columns.is_empty() {
                    return Err(AppError::BadRequest("Tabel memerlukan kolom.".into()));
                }
                let definitions = columns
                    .iter()
                    .map(|c| super::column_definition(c, EngineKind::Mysql))
                    .collect::<AppResult<Vec<_>>>()?;
                format!(
                    "CREATE TABLE {} ({})",
                    ddl_ident(&table_name, EngineKind::Mysql)?,
                    definitions.join(", ")
                )
            }
            DdlAction::RenameColumn {
                table_name,
                column_name,
                new_name,
            } => format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {}",
                ddl_ident(&table_name, EngineKind::Mysql)?,
                ddl_ident(&column_name, EngineKind::Mysql)?,
                ddl_ident(&new_name, EngineKind::Mysql)?
            ),
            DdlAction::DropColumn {
                table_name,
                column_name,
            } => format!(
                "ALTER TABLE {} DROP COLUMN {}",
                ddl_ident(&table_name, EngineKind::Mysql)?,
                ddl_ident(&column_name, EngineKind::Mysql)?
            ),
            DdlAction::RenameTable {
                table_name,
                new_name,
            } => format!(
                "ALTER TABLE {} RENAME TO {}",
                ddl_ident(&table_name, EngineKind::Mysql)?,
                ddl_ident(&new_name, EngineKind::Mysql)?
            ),
            DdlAction::AddColumn { table_name, column } => format!(
                "ALTER TABLE {} ADD COLUMN {}",
                ddl_ident(&table_name, EngineKind::Mysql)?,
                super::column_definition(&column, EngineKind::Mysql)?
            ),
            DdlAction::CreateIndex {
                table_name,
                index_name,
                columns,
                unique,
            } => {
                let vt = validate_identifier(&table_name).map_err(AppError::BadRequest)?;
                let vi = validate_identifier(&index_name).map_err(AppError::BadRequest)?;
                let cols: Vec<String> = columns
                    .into_iter()
                    .map(|c| quote_ident(&c, EngineKind::Mysql))
                    .collect();
                let u = if unique { "UNIQUE " } else { "" };
                format!(
                    "CREATE {u}INDEX {} ON {} ({});",
                    quote_ident(&vi, EngineKind::Mysql),
                    quote_ident(&vt, EngineKind::Mysql),
                    cols.join(", ")
                )
            }
            DdlAction::TruncateItem { item_name } => {
                let vt = validate_identifier(&item_name).map_err(AppError::BadRequest)?;
                format!("TRUNCATE TABLE {};", quote_ident(&vt, EngineKind::Mysql))
            }
            DdlAction::DropItem { item_name } => {
                let vt = validate_identifier(&item_name).map_err(AppError::BadRequest)?;
                format!("DROP TABLE {};", quote_ident(&vt, EngineKind::Mysql))
            }
        };
        sqlx::raw_sql(AssertSqlSafe(sql.as_str()))
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(DdlReport {
            executed_sql: sql,
            message: "MySQL DDL executed".to_string(),
        })
    }

    async fn build_erd(&self, items: &[ItemInfo]) -> AppResult<ErdGraph> {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for item in items.iter().filter(|i| i.item_type == "TABLE") {
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
