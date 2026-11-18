use crate::catalog;
use crate::drivers::resolve_adapter;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use guidb_shared::sql_gen::{quote_ident, validate_identifier};
use guidb_shared::{
    DataModel, EngineKind, SchemaDiffItem, SchemaDiffKind, SchemaSyncApplyResponse,
    SchemaSyncDiffResponse,
};
use std::collections::BTreeMap;

pub async fn compute_schema_diff(
    state: &AppState,
    source_conn_id: &str,
    target_conn_id: &str,
) -> AppResult<SchemaSyncDiffResponse> {
    let source_conn = catalog::get_connection(&state.catalog_pool, source_conn_id).await?;
    let target_conn = catalog::get_connection(&state.catalog_pool, target_conn_id).await?;

    if source_conn.engine.model() != DataModel::Relational
        || target_conn.engine.model() != DataModel::Relational
    {
        return Err(AppError::BadRequest(
            "Schema Sync compares relational SQL databases (SQLite, PostgreSQL, MySQL)".into(),
        ));
    }

    let source_adapter = resolve_adapter(state, &source_conn).await?;
    let target_adapter = resolve_adapter(state, &target_conn).await?;

    let source_items = source_adapter.list_items().await?;
    let target_items = target_adapter.list_items().await?;

    let mut target_map = BTreeMap::new();
    for item in target_items
        .into_iter()
        .filter(|i| i.item_type.contains("TABLE"))
    {
        let st = target_adapter.inspect_structure(&item.name).await?;
        target_map.insert(item.name, st);
    }

    let mut differences = Vec::new();
    let target_engine = target_conn.engine;

    for item in source_items
        .into_iter()
        .filter(|i| i.item_type.contains("TABLE"))
    {
        let src_st = source_adapter.inspect_structure(&item.name).await?;
        let Some(tgt_st) = target_map.get(&item.name) else {
            let ddl = if src_st.ddl.trim().ends_with(';') {
                src_st.ddl.clone()
            } else if src_st.ddl.trim().is_empty() {
                build_fallback_create_table(&src_st, target_engine)
            } else {
                format!("{};", src_st.ddl)
            };
            differences.push(SchemaDiffItem {
                kind: SchemaDiffKind::MissingTable,
                object_name: item.name.clone(),
                detail: format!(
                    "Table '{}' ({} columns) exists in {} but is missing from {}",
                    item.name,
                    src_st.columns.len(),
                    source_conn.name,
                    target_conn.name
                ),
                migration_sql: ddl,
            });
            for idx in &src_st.indexes {
                let cols: Vec<String> = idx
                    .columns
                    .iter()
                    .map(|c| quote_ident(c, target_engine))
                    .collect();
                let u = if idx.unique { "UNIQUE " } else { "" };
                differences.push(SchemaDiffItem {
                    kind: SchemaDiffKind::MissingIndex,
                    object_name: format!("{}.{}", item.name, idx.name),
                    detail: format!("Index '{}' missing on new table '{}'", idx.name, item.name),
                    migration_sql: format!(
                        "CREATE {u}INDEX {} ON {} ({});",
                        quote_ident(&idx.name, target_engine),
                        quote_ident(&item.name, target_engine),
                        cols.join(", ")
                    ),
                });
            }
            continue;
        };

        let tgt_cols: BTreeMap<&str, &guidb_shared::ColumnDef> = tgt_st
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c))
            .collect();

        for col in &src_st.columns {
            match tgt_cols.get(col.name.as_str()) {
                None => {
                    let mut stmt = format!(
                        "ALTER TABLE {} ADD COLUMN {} {}",
                        quote_ident(&item.name, target_engine),
                        quote_ident(&col.name, target_engine),
                        col.data_type
                    );
                    if !col.nullable {
                        let dflt = col
                            .default_value
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .unwrap_or(if col.data_type.to_uppercase().contains("INT") {
                                "0"
                            } else {
                                "''"
                            });
                        stmt.push_str(&format!(" NOT NULL DEFAULT {dflt}"));
                    } else if let Some(ref d) = col.default_value {
                        stmt.push_str(&format!(" DEFAULT {d}"));
                    }
                    stmt.push(';');

                    differences.push(SchemaDiffItem {
                        kind: SchemaDiffKind::MissingColumn,
                        object_name: format!("{}.{}", item.name, col.name),
                        detail: format!(
                            "Column '{}.{}' ({}) missing in {}",
                            item.name, col.name, col.data_type, target_conn.name
                        ),
                        migration_sql: stmt,
                    });
                }
                Some(tcol) => {
                    if normalize_type(&col.data_type) != normalize_type(&tcol.data_type) {
                        differences.push(SchemaDiffItem {
                            kind: SchemaDiffKind::TypeMismatch,
                            object_name: format!("{}.{}", item.name, col.name),
                            detail: format!(
                                "Type differs: source={}, target={}",
                                col.data_type, tcol.data_type
                            ),
                            migration_sql: format!(
                                "-- Manual review suggested: ALTER TABLE {} MODIFY COLUMN {} {};",
                                quote_ident(&item.name, target_engine),
                                quote_ident(&col.name, target_engine),
                                col.data_type
                            ),
                        });
                    }
                }
            }
        }

        let tgt_idx_names: std::collections::HashSet<&str> =
            tgt_st.indexes.iter().map(|i| i.name.as_str()).collect();
        for idx in &src_st.indexes {
            if !tgt_idx_names.contains(idx.name.as_str()) {
                let cols: Vec<String> = idx
                    .columns
                    .iter()
                    .map(|c| quote_ident(c, target_engine))
                    .collect();
                let u = if idx.unique { "UNIQUE " } else { "" };
                differences.push(SchemaDiffItem {
                    kind: SchemaDiffKind::MissingIndex,
                    object_name: format!("{}.{}", item.name, idx.name),
                    detail: format!(
                        "Index '{}' ({}) missing in {}",
                        idx.name,
                        idx.columns.join(", "),
                        target_conn.name
                    ),
                    migration_sql: format!(
                        "CREATE {u}INDEX {} ON {} ({});",
                        quote_ident(&idx.name, target_engine),
                        quote_ident(&item.name, target_engine),
                        cols.join(", ")
                    ),
                });
            }
        }
    }

    let full_migration_script = differences
        .iter()
        .map(|d| d.migration_sql.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    Ok(SchemaSyncDiffResponse {
        source_name: source_conn.name,
        target_name: target_conn.name,
        differences,
        full_migration_script,
    })
}

pub async fn apply_schema_sync(
    state: &AppState,
    source_conn_id: &str,
    target_conn_id: &str,
    statements: &[String],
) -> AppResult<SchemaSyncApplyResponse> {
    let target_conn = catalog::get_connection(&state.catalog_pool, target_conn_id).await?;
    let target_adapter = resolve_adapter(state, &target_conn).await?;

    let mut applied = 0_usize;
    for stmt in statements {
        let trimmed = stmt.trim();
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }
        target_adapter.execute_query(None, trimmed).await?;
        catalog::record_history(
            &state.catalog_pool,
            &target_conn.id,
            &target_conn.name,
            trimmed,
            "OK (SYNC)",
            0,
            1,
        )
        .await;
        applied += 1;
    }

    let report = compute_schema_diff(state, source_conn_id, target_conn_id).await?;
    let remaining = report.differences.len();
    Ok(SchemaSyncApplyResponse {
        applied_statements: applied,
        remaining_differences: remaining,
        report,
    })
}

fn normalize_type(raw: &str) -> String {
    raw.trim().to_uppercase().replace("INT8", "BIGINT")
}

fn build_fallback_create_table(st: &guidb_shared::StructureDef, engine: EngineKind) -> String {
    let valid_t = validate_identifier(&st.item_name).unwrap_or_else(|_| st.item_name.clone());
    let cols: Vec<String> = st
        .columns
        .iter()
        .map(|c| {
            let mut s = format!("{} {}", quote_ident(&c.name, engine), c.data_type);
            if c.is_pk {
                s.push_str(" PRIMARY KEY");
            } else if !c.nullable {
                s.push_str(" NOT NULL");
            }
            s
        })
        .collect();
    format!(
        "CREATE TABLE {} ({});",
        quote_ident(&valid_t, engine),
        cols.join(", ")
    )
}
