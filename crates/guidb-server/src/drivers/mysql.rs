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
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::collections::HashSet;
use std::time::Instant;

pub struct MysqlAdapter {
    pool: MySqlPool,
}

impl MysqlAdapter {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
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
            let cnt: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {q}"))
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
                format!("  {} {}{}{}", quote_ident(&c.name, EngineKind::Mysql), c.data_type, pk, nn)
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
            _ => String::new(),
        };

        let limit = opts.limit.clamp(1, 500);
        let offset = opts.offset.max(0);
        let count_sql = format!("SELECT COUNT(*) FROM {qt}{where_clause}");
        let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
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
        let mut dq = sqlx::query(&data_sql);
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
        if upper.starts_with("SELECT") || upper.starts_with("SHOW") || upper.starts_with("EXPLAIN") || upper.starts_with("DESCRIBE") {
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
        let res = sqlx::raw_sql(trimmed)
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
                    let mut q = sqlx::query(&sql);
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
                    let mut q = sqlx::query(&sql);
                    for v in changes.values() {
                        q = bind_mysql(q, v);
                    }
                    for v in pk_match.values() {
                        if !matches!(v, CellValue::Null) {
                            q = bind_mysql(q, v);
                        }
                    }
                    q.execute(&mut *tx)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?;
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
                    let mut q = sqlx::query(&sql);
                    for v in pk_match.values() {
                        if !matches!(v, CellValue::Null) {
                            q = bind_mysql(q, v);
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
                let vt = validate_identifier(&table_name).map_err(AppError::BadRequest)?;
                let mut defs = Vec::new();
                for c in columns {
                    let vc = validate_identifier(&c.name).map_err(AppError::BadRequest)?;
                    let mut line = format!("{} {}", quote_ident(&vc, EngineKind::Mysql), c.data_type);
                    if c.is_pk {
                        line.push_str(" PRIMARY KEY");
                    } else if !c.nullable {
                        line.push_str(" NOT NULL");
                    }
                    defs.push(line);
                }
                format!(
                    "CREATE TABLE {} ({});",
                    quote_ident(&vt, EngineKind::Mysql),
                    defs.join(", ")
                )
            }
            DdlAction::AddColumn { table_name, column } => {
                let vt = validate_identifier(&table_name).map_err(AppError::BadRequest)?;
                let vc = validate_identifier(&column.name).map_err(AppError::BadRequest)?;
                format!(
                    "ALTER TABLE {} ADD COLUMN {} {};",
                    quote_ident(&vt, EngineKind::Mysql),
                    quote_ident(&vc, EngineKind::Mysql),
                    column.data_type
                )
            }
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
        sqlx::raw_sql(&sql)
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(DdlReport {
            executed_sql: sql,
            message: "MySQL DDL executed".to_string(),
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
