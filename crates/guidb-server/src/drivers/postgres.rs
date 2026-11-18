use crate::drivers::{DataQueryOptions, DatabaseAdapter};
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use guidb_shared::sql_gen::{preview_mutations, quote_ident, validate_identifier};
use guidb_shared::{
    CellValue, ColumnDef, DdlAction, DdlReport, EngineKind, ErdEdge, ErdGraph, ErdNode,
    ForeignKeyDef, IndexDef, ItemInfo, MutationReport, QueryResult, StagedMutation, StructureDef,
    TestConnectionResponse,
};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::collections::HashSet;
use std::time::Instant;

pub struct PostgresAdapter {
    pool: PgPool,
}

impl PostgresAdapter {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn decode_pg_row(row: &PgRow) -> Vec<CellValue> {
    (0..row.len())
        .map(|idx| {
            let Ok(raw) = row.try_get_raw(idx) else {
                return CellValue::Null;
            };
            if raw.is_null() {
                return CellValue::Null;
            }
            let ty = raw.type_info().name().to_uppercase();
            if ty == "BOOL" {
                if let Ok(v) = row.try_get::<bool, _>(idx) {
                    return CellValue::Bool(v);
                }
            }
            if matches!(ty.as_str(), "INT2" | "INT4" | "INT8") {
                if let Ok(v) = row.try_get::<i64, _>(idx) {
                    return CellValue::Int(v);
                }
                if let Ok(v) = row.try_get::<i32, _>(idx) {
                    return CellValue::Int(v as i64);
                }
            }
            if matches!(ty.as_str(), "FLOAT4" | "FLOAT8" | "NUMERIC") {
                if let Ok(v) = row.try_get::<f64, _>(idx) {
                    return CellValue::Float(v);
                }
            }
            if ty == "JSON" || ty == "JSONB" {
                if let Ok(v) = row.try_get::<serde_json::Value, _>(idx) {
                    return CellValue::Json(v);
                }
            }
            if let Ok(s) = row.try_get::<String, _>(idx) {
                return CellValue::Text(s);
            }
            if let Ok(u) = row.try_get::<uuid::Uuid, _>(idx) {
                return CellValue::Text(u.to_string());
            }
            if let Ok(dt) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
                return CellValue::Text(dt.to_string());
            }
            CellValue::Bytes(format!("<{ty}>"))
        })
        .collect()
}

fn bind_pg<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    val: &'q CellValue,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match val {
        CellValue::Null => q.bind(Option::<String>::None),
        CellValue::Bool(b) => q.bind(*b),
        CellValue::Int(i) => q.bind(*i),
        CellValue::Float(f) => q.bind(*f),
        CellValue::Text(s) => q.bind(s.as_str()),
        CellValue::Json(v) => q.bind(v.clone()),
        CellValue::Bytes(b) => q.bind(b.as_str()),
    }
}

#[async_trait]
impl DatabaseAdapter for PostgresAdapter {
    async fn test_connection(&self) -> AppResult<TestConnectionResponse> {
        let start = Instant::now();
        let ver: String = sqlx::query_scalar("SELECT version()")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(TestConnectionResponse {
            reachable: true,
            latency_ms: start.elapsed().as_millis() as u64,
            server_version: ver.split(',').next().unwrap_or(&ver).to_string(),
            details: "PostgreSQL pool connected".to_string(),
        })
    }

    async fn list_items(&self) -> AppResult<Vec<ItemInfo>> {
        let rows = sqlx::query(
            "SELECT table_name, table_type FROM information_schema.tables
             WHERE table_schema = 'public' ORDER BY table_type ASC, table_name ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let name: String = r.get("table_name");
            let tt: String = r.get("table_type");
            let q = quote_ident(&name, EngineKind::Postgres);
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
        let pk_rows: Vec<String> = sqlx::query_scalar(
            "SELECT kcu.column_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage kcu
               ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema
             WHERE tc.constraint_type = 'PRIMARY KEY' AND tc.table_schema = 'public' AND tc.table_name = $1",
        )
        .bind(&valid)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();
        let pk_set: HashSet<String> = pk_rows.into_iter().collect();

        let col_rows = sqlx::query(
            "SELECT column_name, data_type, is_nullable, column_default
             FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = $1
             ORDER BY ordinal_position ASC",
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
            .map(|r| {
                let cname: String = r.get("column_name");
                let nullable = r.get::<String, _>("is_nullable") == "YES";
                ColumnDef {
                    is_pk: pk_set.contains(&cname),
                    name: cname,
                    data_type: r.get::<String, _>("data_type").to_uppercase(),
                    nullable,
                    default_value: r.get("column_default"),
                }
            })
            .collect();

        let fk_rows = sqlx::query(
            "SELECT tc.constraint_name, kcu.column_name, ccu.table_name AS to_table, ccu.column_name AS to_column
             FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage kcu
               ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema
             JOIN information_schema.constraint_column_usage ccu
               ON ccu.constraint_name = tc.constraint_name AND ccu.table_schema = tc.table_schema
             WHERE tc.constraint_type = 'FOREIGN KEY' AND tc.table_schema = 'public' AND tc.table_name = $1",
        )
        .bind(&valid)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let foreign_keys: Vec<ForeignKeyDef> = fk_rows
            .into_iter()
            .map(|r| ForeignKeyDef {
                name: r.get("constraint_name"),
                from_column: r.get("column_name"),
                to_table: r.get("to_table"),
                to_column: r.get("to_column"),
                on_delete: "NO ACTION".to_string(),
            })
            .collect();

        let idx_rows = sqlx::query(
            "SELECT indexname, indexdef FROM pg_indexes WHERE schemaname = 'public' AND tablename = $1",
        )
        .bind(&valid)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();
        let indexes: Vec<IndexDef> = idx_rows
            .into_iter()
            .map(|r| {
                let def: String = r.get("indexdef");
                IndexDef {
                    name: r.get("indexname"),
                    columns: Vec::new(),
                    unique: def.contains("UNIQUE"),
                }
            })
            .collect();

        let ddl_cols: Vec<String> = columns
            .iter()
            .map(|c| {
                let nn = if !c.nullable { " NOT NULL" } else { "" };
                let pk = if c.is_pk { " PRIMARY KEY" } else { "" };
                format!(
                    "  {} {}{}{}",
                    quote_ident(&c.name, EngineKind::Postgres),
                    c.data_type,
                    pk,
                    nn
                )
            })
            .collect();
        let ddl = format!(
            "CREATE TABLE {} (\n{}\n);",
            quote_ident(&valid, EngineKind::Postgres),
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
        let qt = quote_ident(&st.item_name, EngineKind::Postgres);
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
                .map(|c| {
                    format!(
                        "CAST({} AS TEXT) ILIKE $1",
                        quote_ident(c, EngineKind::Postgres)
                    )
                })
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
                format!(" ORDER BY {} {dir}", quote_ident(c, EngineKind::Postgres))
            }
            _ => String::new(),
        };

        let limit = opts.limit.clamp(1, 500);
        let offset = opts.offset.max(0);
        let count_sql = format!("SELECT COUNT(*) FROM {qt}{where_clause}");
        let total: i64 = if let Some(ref pat) = filter_pat {
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
            format!("SELECT * FROM {qt}{where_clause}{order_clause} LIMIT {limit} OFFSET {offset}");
        let rows_raw = if let Some(ref pat) = filter_pat {
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

        let rows = rows_raw.iter().map(decode_pg_row).collect();
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
        if upper.starts_with("SELECT")
            || upper.starts_with("WITH")
            || upper.starts_with("EXPLAIN")
            || upper.starts_with("SHOW")
        {
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
            let rows: Vec<Vec<CellValue>> = rows_raw.iter().map(decode_pg_row).collect();
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
        let previews = preview_mutations(mutations, EngineKind::Postgres);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        for m in mutations {
            match m {
                StagedMutation::Insert { item, values } => {
                    let table = validate_identifier(item).map_err(AppError::BadRequest)?;
                    let mut cols = Vec::new();
                    let mut ph = Vec::new();
                    for (i, c) in values.keys().enumerate() {
                        let vc = validate_identifier(c).map_err(AppError::BadRequest)?;
                        cols.push(quote_ident(&vc, EngineKind::Postgres));
                        ph.push(format!("${}", i + 1));
                    }
                    let sql = format!(
                        "INSERT INTO {} ({}) VALUES ({})",
                        quote_ident(&table, EngineKind::Postgres),
                        cols.join(", "),
                        ph.join(", ")
                    );
                    let mut q = sqlx::query(&sql);
                    for v in values.values() {
                        q = bind_pg(q, v);
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
                    let mut idx = 1_usize;
                    let mut sets = Vec::new();
                    for c in changes.keys() {
                        let vc = validate_identifier(c).map_err(AppError::BadRequest)?;
                        sets.push(format!(
                            "{} = ${idx}",
                            quote_ident(&vc, EngineKind::Postgres)
                        ));
                        idx += 1;
                    }
                    let mut wheres = Vec::new();
                    for (c, v) in pk_match {
                        let vc = validate_identifier(c).map_err(AppError::BadRequest)?;
                        if matches!(v, CellValue::Null) {
                            wheres.push(format!(
                                "{} IS NULL",
                                quote_ident(&vc, EngineKind::Postgres)
                            ));
                        } else {
                            wheres.push(format!(
                                "{} = ${idx}",
                                quote_ident(&vc, EngineKind::Postgres)
                            ));
                            idx += 1;
                        }
                    }
                    let sql = format!(
                        "UPDATE {} SET {} WHERE {}",
                        quote_ident(&table, EngineKind::Postgres),
                        sets.join(", "),
                        wheres.join(" AND ")
                    );
                    let mut q = sqlx::query(&sql);
                    for v in changes.values() {
                        q = bind_pg(q, v);
                    }
                    for v in pk_match.values() {
                        if !matches!(v, CellValue::Null) {
                            q = bind_pg(q, v);
                        }
                    }
                    q.execute(&mut *tx)
                        .await
                        .map_err(|e| AppError::Database(e.to_string()))?;
                }
                StagedMutation::Delete { item, pk_match } => {
                    let table = validate_identifier(item).map_err(AppError::BadRequest)?;
                    let mut idx = 1_usize;
                    let mut wheres = Vec::new();
                    for (c, v) in pk_match {
                        let vc = validate_identifier(c).map_err(AppError::BadRequest)?;
                        if matches!(v, CellValue::Null) {
                            wheres.push(format!(
                                "{} IS NULL",
                                quote_ident(&vc, EngineKind::Postgres)
                            ));
                        } else {
                            wheres.push(format!(
                                "{} = ${idx}",
                                quote_ident(&vc, EngineKind::Postgres)
                            ));
                            idx += 1;
                        }
                    }
                    let sql = format!(
                        "DELETE FROM {} WHERE {}",
                        quote_ident(&table, EngineKind::Postgres),
                        wheres.join(" AND ")
                    );
                    let mut q = sqlx::query(&sql);
                    for v in pk_match.values() {
                        if !matches!(v, CellValue::Null) {
                            q = bind_pg(q, v);
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
                    let mut line =
                        format!("{} {}", quote_ident(&vc, EngineKind::Postgres), c.data_type);
                    if c.is_pk {
                        line.push_str(" PRIMARY KEY");
                    } else if !c.nullable {
                        line.push_str(" NOT NULL");
                    }
                    defs.push(line);
                }
                format!(
                    "CREATE TABLE {} ({});",
                    quote_ident(&vt, EngineKind::Postgres),
                    defs.join(", ")
                )
            }
            DdlAction::AddColumn { table_name, column } => {
                let vt = validate_identifier(&table_name).map_err(AppError::BadRequest)?;
                let vc = validate_identifier(&column.name).map_err(AppError::BadRequest)?;
                format!(
                    "ALTER TABLE {} ADD COLUMN {} {};",
                    quote_ident(&vt, EngineKind::Postgres),
                    quote_ident(&vc, EngineKind::Postgres),
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
                    .map(|c| quote_ident(&c, EngineKind::Postgres))
                    .collect();
                let u = if unique { "UNIQUE " } else { "" };
                format!(
                    "CREATE {u}INDEX {} ON {} ({});",
                    quote_ident(&vi, EngineKind::Postgres),
                    quote_ident(&vt, EngineKind::Postgres),
                    cols.join(", ")
                )
            }
            DdlAction::TruncateItem { item_name } => {
                let vt = validate_identifier(&item_name).map_err(AppError::BadRequest)?;
                format!("TRUNCATE TABLE {};", quote_ident(&vt, EngineKind::Postgres))
            }
            DdlAction::DropItem { item_name } => {
                let vt = validate_identifier(&item_name).map_err(AppError::BadRequest)?;
                format!("DROP TABLE {};", quote_ident(&vt, EngineKind::Postgres))
            }
        };
        sqlx::raw_sql(&sql)
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(DdlReport {
            executed_sql: sql,
            message: "PostgreSQL DDL executed".to_string(),
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
