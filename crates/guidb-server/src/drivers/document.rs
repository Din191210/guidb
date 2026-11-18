use crate::drivers::{DataQueryOptions, DatabaseAdapter};
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use guidb_shared::sql_gen::preview_mutations;
use guidb_shared::{
    CellValue, ColumnDef, DdlAction, DdlReport, EngineKind, ErdGraph, ItemInfo, MutationReport,
    QueryResult, StagedMutation, StructureDef, TestConnectionResponse,
};
use serde_json::{Map, Value};
use sqlx::{Row, SqlitePool};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;
use uuid::Uuid;

pub struct DocumentAdapter {
    pool: SqlitePool,
}

impl DocumentAdapter {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DatabaseAdapter for DocumentAdapter {
    async fn test_connection(&self) -> AppResult<TestConnectionResponse> {
        let start = Instant::now();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _guidb_collections")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(TestConnectionResponse {
            reachable: true,
            latency_ms: start.elapsed().as_millis() as u64,
            server_version: "GuiDB Document Engine (MongoDB BSON/JSON)".to_string(),
            details: format!("{count} collections available"),
        })
    }

    async fn list_items(&self) -> AppResult<Vec<ItemInfo>> {
        let rows = sqlx::query("SELECT name FROM _guidb_collections ORDER BY name ASC")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let name: String = r.get("name");
            let cnt: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM _guidb_documents WHERE collection = ?1")
                    .bind(&name)
                    .fetch_one(&self.pool)
                    .await
                    .unwrap_or(0);
            out.push(ItemInfo {
                name,
                item_type: "COLLECTION".to_string(),
                row_count: cnt,
            });
        }
        Ok(out)
    }

    async fn inspect_structure(&self, item: &str) -> AppResult<StructureDef> {
        let rows =
            sqlx::query("SELECT doc_json FROM _guidb_documents WHERE collection = ?1 LIMIT 100")
                .bind(item)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;

        let mut field_types: BTreeMap<String, String> = BTreeMap::new();
        field_types.insert("_id".to_string(), "ObjectId / String".to_string());
        for r in rows {
            let raw: String = r.get("doc_json");
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) {
                for (k, v) in map {
                    let ty = match v {
                        Value::Null => "Null",
                        Value::Bool(_) => "Boolean",
                        Value::Number(n) if n.is_i64() => "Int64",
                        Value::Number(_) => "Double",
                        Value::String(_) => "String",
                        Value::Array(_) => "Array",
                        Value::Object(_) => "Document",
                    };
                    field_types.entry(k).or_insert_with(|| ty.to_string());
                }
            }
        }

        let columns = field_types
            .into_iter()
            .map(|(name, data_type)| ColumnDef {
                is_pk: name == "_id",
                nullable: name != "_id",
                name,
                data_type,
                default_value: None,
            })
            .collect();

        Ok(StructureDef {
            item_name: item.to_string(),
            item_type: "COLLECTION".to_string(),
            columns,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            ddl: format!("db.createCollection(\"{item}\");"),
        })
    }

    async fn fetch_item_data(&self, item: &str, opts: DataQueryOptions) -> AppResult<QueryResult> {
        let start = Instant::now();
        let rows = sqlx::query(
            "SELECT doc_id, doc_json FROM _guidb_documents WHERE collection = ?1 ORDER BY doc_id ASC",
        )
        .bind(item)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

        let mut parsed_docs: Vec<Map<String, Value>> = Vec::with_capacity(rows.len());
        let filter_obj = opts
            .filter
            .as_deref()
            .map(str::trim)
            .filter(|s| s.starts_with('{'))
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|v| v.as_object().cloned());
        let text_filter = opts
            .filter
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('{'))
            .map(|s| s.to_lowercase());

        for r in rows {
            let doc_id: String = r.get("doc_id");
            let raw: String = r.get("doc_json");
            let mut obj = match serde_json::from_str::<Value>(&raw) {
                Ok(Value::Object(m)) => m,
                _ => Map::new(),
            };
            obj.entry("_id".to_string())
                .or_insert_with(|| Value::String(doc_id));
            if let Some(ref fobj) = filter_obj {
                if !matches_filter(&obj, fobj) {
                    continue;
                }
            } else if let Some(ref needle) = text_filter {
                if !raw.to_lowercase().contains(needle) {
                    continue;
                }
            }
            parsed_docs.push(obj);
        }

        let total = parsed_docs.len() as i64;
        let start_idx = (opts.offset.max(0) as usize).min(parsed_docs.len());
        let end_idx = (start_idx + opts.limit.clamp(1, 500) as usize).min(parsed_docs.len());
        let page = &parsed_docs[start_idx..end_idx];

        Ok(docs_to_query_result(
            page,
            Some(total),
            start.elapsed().as_millis() as u64,
            None,
        ))
    }

    async fn execute_query(&self, item: Option<&str>, query: &str) -> AppResult<QueryResult> {
        let start = Instant::now();
        let trimmed = query.trim();
        let col = item.unwrap_or("service_events");

        if trimmed.is_empty() || trimmed == "{}" {
            return self
                .fetch_item_data(
                    col,
                    DataQueryOptions {
                        filter: None,
                        sort_col: None,
                        sort_dir: None,
                        limit: 100,
                        offset: 0,
                    },
                )
                .await;
        }

        let parsed: Value = serde_json::from_str(trimmed).map_err(|e| {
            AppError::BadRequest(format!(
                "MongoDB query must be a JSON filter object {{...}} or aggregation pipeline [...]: {e}"
            ))
        })?;

        let rows = sqlx::query(
            "SELECT doc_id, doc_json FROM _guidb_documents WHERE collection = ?1 ORDER BY doc_id ASC",
        )
        .bind(col)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

        let mut docs: Vec<Map<String, Value>> = rows
            .into_iter()
            .filter_map(|r| {
                let id: String = r.get("doc_id");
                let raw: String = r.get("doc_json");
                if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(&raw) {
                    m.entry("_id".to_string()).or_insert(Value::String(id));
                    Some(m)
                } else {
                    None
                }
            })
            .collect();

        if let Value::Object(filter_map) = parsed {
            docs.retain(|d| matches_filter(d, &filter_map));
            let total = docs.len() as i64;
            return Ok(docs_to_query_result(
                &docs,
                Some(total),
                start.elapsed().as_millis() as u64,
                None,
            ));
        }

        if let Value::Array(stages) = parsed {
            for stage in stages {
                let Some(stage_obj) = stage.as_object() else {
                    continue;
                };
                if let Some(Value::Object(match_filter)) = stage_obj.get("$match") {
                    docs.retain(|d| matches_filter(d, match_filter));
                } else if let Some(Value::Object(group_spec)) = stage_obj.get("$group") {
                    docs = run_group_stage(&docs, group_spec);
                } else if let Some(Value::Object(sort_spec)) = stage_obj.get("$sort") {
                    if let Some((field, dir_val)) = sort_spec.iter().next() {
                        let desc = dir_val.as_i64().unwrap_or(1) < 0;
                        docs.sort_by(|a, b| {
                            let va = a.get(field).map(|v| v.to_string()).unwrap_or_default();
                            let vb = b.get(field).map(|v| v.to_string()).unwrap_or_default();
                            let na = a.get(field).and_then(|v| v.as_f64());
                            let nb = b.get(field).and_then(|v| v.as_f64());
                            let ord = match (na, nb) {
                                (Some(x), Some(y)) => {
                                    x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
                                }
                                _ => va.cmp(&vb),
                            };
                            if desc {
                                ord.reverse()
                            } else {
                                ord
                            }
                        });
                    }
                } else if let Some(limit_val) = stage_obj.get("$limit").and_then(|v| v.as_u64()) {
                    docs.truncate(limit_val as usize);
                }
            }
            let total = docs.len() as i64;
            return Ok(docs_to_query_result(
                &docs,
                Some(total),
                start.elapsed().as_millis() as u64,
                Some("Aggregation pipeline executed".to_string()),
            ));
        }

        Err(AppError::BadRequest(
            "Expected JSON object filter or JSON array aggregation pipeline".into(),
        ))
    }

    async fn apply_staged_mutations(
        &self,
        mutations: &[StagedMutation],
    ) -> AppResult<MutationReport> {
        let start = Instant::now();
        let previews = preview_mutations(mutations, EngineKind::MongoDb);
        let now = chrono::Utc::now().to_rfc3339();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        for m in mutations {
            match m {
                StagedMutation::Insert { item, values } => {
                    let mut doc_map = Map::new();
                    for (k, v) in values {
                        doc_map.insert(k.clone(), cell_to_val(v));
                    }
                    let doc_id = doc_map
                        .get("_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
                    doc_map.insert("_id".to_string(), Value::String(doc_id.clone()));
                    let json_str = serde_json::to_string(&Value::Object(doc_map))
                        .unwrap_or_else(|_| "{}".to_string());
                    sqlx::query(
                        "INSERT INTO _guidb_documents (collection, doc_id, doc_json, updated_at)
                         VALUES (?1, ?2, ?3, ?4)",
                    )
                    .bind(item)
                    .bind(doc_id)
                    .bind(json_str)
                    .bind(&now)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                }
                StagedMutation::Update {
                    item,
                    pk_match,
                    changes,
                } => {
                    let doc_id = pk_match
                        .get("_id")
                        .map(CellValue::display_text)
                        .ok_or_else(|| {
                            AppError::BadRequest("Missing _id for document update".into())
                        })?;
                    let raw: String = sqlx::query_scalar(
                        "SELECT doc_json FROM _guidb_documents WHERE collection = ?1 AND doc_id = ?2",
                    )
                    .bind(item)
                    .bind(&doc_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?
                    .ok_or_else(|| AppError::NotFound(format!("Document '{doc_id}' not found")))?;

                    let mut obj = match serde_json::from_str::<Value>(&raw) {
                        Ok(Value::Object(m)) => m,
                        _ => Map::new(),
                    };
                    for (k, v) in changes {
                        obj.insert(k.clone(), cell_to_val(v));
                    }
                    let updated_json = serde_json::to_string(&Value::Object(obj))
                        .unwrap_or_else(|_| "{}".to_string());
                    sqlx::query(
                        "UPDATE _guidb_documents SET doc_json = ?1, updated_at = ?2 WHERE collection = ?3 AND doc_id = ?4",
                    )
                    .bind(updated_json)
                    .bind(&now)
                    .bind(item)
                    .bind(&doc_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                }
                StagedMutation::Delete { item, pk_match } => {
                    let doc_id = pk_match
                        .get("_id")
                        .map(CellValue::display_text)
                        .ok_or_else(|| {
                            AppError::BadRequest("Missing _id for document delete".into())
                        })?;
                    sqlx::query(
                        "DELETE FROM _guidb_documents WHERE collection = ?1 AND doc_id = ?2",
                    )
                    .bind(item)
                    .bind(&doc_id)
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
        let now = chrono::Utc::now().to_rfc3339();
        match action {
            DdlAction::CreateTable { table_name, .. } => {
                let col = table_name.trim().to_string();
                if col.is_empty() {
                    return Err(AppError::BadRequest("Collection name required".into()));
                }
                sqlx::query(
                    "INSERT OR IGNORE INTO _guidb_collections (name, created_at) VALUES (?1, ?2)",
                )
                .bind(&col)
                .bind(&now)
                .execute(&self.pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
                Ok(DdlReport {
                    executed_sql: format!("db.createCollection(\"{col}\");"),
                    message: format!("Collection '{col}' created"),
                })
            }
            DdlAction::TruncateItem { item_name } => {
                sqlx::query("DELETE FROM _guidb_documents WHERE collection = ?1")
                    .bind(&item_name)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                Ok(DdlReport {
                    executed_sql: format!("db.{item_name}.deleteMany({{}});"),
                    message: format!("Collection '{item_name}' emptied"),
                })
            }
            DdlAction::DropItem { item_name } => {
                sqlx::query("DELETE FROM _guidb_documents WHERE collection = ?1")
                    .bind(&item_name)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                sqlx::query("DELETE FROM _guidb_collections WHERE name = ?1")
                    .bind(&item_name)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                Ok(DdlReport {
                    executed_sql: format!("db.{item_name}.drop();"),
                    message: format!("Collection '{item_name}' dropped"),
                })
            }
            _ => Err(AppError::BadRequest(
                "Collection structure is schema-less; use document editor or query bar".into(),
            )),
        }
    }

    async fn build_erd(&self) -> AppResult<ErdGraph> {
        Ok(ErdGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
        })
    }
}

fn matches_filter(doc: &Map<String, Value>, filter: &Map<String, Value>) -> bool {
    filter
        .iter()
        .all(|(k, expected)| doc.get(k).is_some_and(|actual| actual == expected))
}

fn run_group_stage(
    docs: &[Map<String, Value>],
    group_spec: &Map<String, Value>,
) -> Vec<Map<String, Value>> {
    let key_expr = group_spec
        .get("_id")
        .and_then(|v| v.as_str())
        .unwrap_or("all");
    let group_field = key_expr.strip_prefix('$');
    let mut buckets: BTreeMap<String, Vec<&Map<String, Value>>> = BTreeMap::new();
    for doc in docs {
        let key = match group_field {
            Some(f) => doc
                .get(f)
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "null".to_string()),
            None => "all".to_string(),
        };
        buckets.entry(key).or_default().push(doc);
    }

    let mut out = Vec::with_capacity(buckets.len());
    for (group_key, items) in buckets {
        let mut row = Map::new();
        row.insert("_id".to_string(), Value::String(group_key));
        for (alias, expr) in group_spec {
            if alias == "_id" {
                continue;
            }
            if let Some(op_map) = expr.as_object() {
                if op_map.contains_key("$sum") {
                    let sum_val = if op_map.get("$sum").and_then(|v| v.as_i64()) == Some(1) {
                        Value::from(items.len() as i64)
                    } else if let Some(field) = op_map
                        .get("$sum")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.strip_prefix('$'))
                    {
                        let total: f64 = items
                            .iter()
                            .filter_map(|d| d.get(field).and_then(|v| v.as_f64()))
                            .sum();
                        Value::from(total)
                    } else {
                        Value::from(items.len() as i64)
                    };
                    row.insert(alias.clone(), sum_val);
                } else if let Some(field) = op_map
                    .get("$avg")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.strip_prefix('$'))
                {
                    let vals: Vec<f64> = items
                        .iter()
                        .filter_map(|d| d.get(field).and_then(|v| v.as_f64()))
                        .collect();
                    let avg = if vals.is_empty() {
                        0.0
                    } else {
                        (vals.iter().sum::<f64>() / vals.len() as f64 * 100.0).round() / 100.0
                    };
                    row.insert(alias.clone(), Value::from(avg));
                }
            }
        }
        out.push(row);
    }
    out
}

fn docs_to_query_result(
    docs: &[Map<String, Value>],
    total_rows: Option<i64>,
    duration_ms: u64,
    notice: Option<String>,
) -> QueryResult {
    let mut keys = BTreeSet::new();
    for d in docs {
        for k in d.keys() {
            keys.insert(k.clone());
        }
    }
    let mut columns: Vec<String> = Vec::with_capacity(keys.len());
    if keys.remove("_id") {
        columns.push("_id".to_string());
    }
    columns.extend(keys);
    let column_types = vec!["JSON".to_string(); columns.len()];
    let pk_columns = if columns.first().is_some_and(|c| c == "_id") {
        vec!["_id".to_string()]
    } else {
        Vec::new()
    };

    let rows = docs
        .iter()
        .map(|doc| {
            columns
                .iter()
                .map(|c| match doc.get(c) {
                    None | Some(Value::Null) => CellValue::Null,
                    Some(Value::Bool(b)) => CellValue::Bool(*b),
                    Some(Value::Number(n)) => {
                        if let Some(i) = n.as_i64() {
                            CellValue::Int(i)
                        } else if let Some(f) = n.as_f64() {
                            CellValue::Float(f)
                        } else {
                            CellValue::Text(n.to_string())
                        }
                    }
                    Some(Value::String(s)) => CellValue::Text(s.clone()),
                    Some(other) => CellValue::Json(other.clone()),
                })
                .collect()
        })
        .collect();

    QueryResult {
        columns,
        column_types,
        pk_columns,
        rows,
        total_rows,
        affected_rows: 0,
        duration_ms,
        notice,
    }
}

fn cell_to_val(c: &CellValue) -> Value {
    match c {
        CellValue::Null => Value::Null,
        CellValue::Bool(b) => Value::Bool(*b),
        CellValue::Int(i) => Value::from(*i),
        CellValue::Float(f) => Value::from(*f),
        CellValue::Text(s) => Value::String(s.clone()),
        CellValue::Json(v) => v.clone(),
        CellValue::Bytes(b) => Value::String(b.clone()),
    }
}
