use crate::backup;
use crate::catalog;
use crate::discovery;
use crate::drivers::sync;
use crate::drivers::{resolve_adapter, test_connection_direct, DataQueryOptions};
use crate::error::{AppError, AppResult};
use crate::safety;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use guidb_shared::sql_gen::{format_sql_literal, preview_mutations, quote_ident};
use guidb_shared::{
    BackupDashboardResponse, BackupKind, BackupRecord, BackupSchedule, BootstrapResponse,
    ConnectionProfile, CreateBackupRequest, CreateConnectionRequest, CreateSavedQueryRequest,
    CreateScheduleRequest, DdlAction, DdlReport, ExecuteQueryRequest, MutateRequest,
    MutationReport, ProtectionMode, QueryHistoryEntry, QueryResult, RestoreBackupRequest,
    RestoreBackupResponse, SavedQueryEntry, SchemaSyncApplyRequest, SchemaSyncApplyResponse,
    SchemaSyncDiffRequest, SchemaSyncDiffResponse, TestConnectionRequest, TestConnectionResponse,
    ToggleScheduleRequest, UpdateProtectionRequest, VerifyBackupResponse, WorkspaceResponse,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct WorkspaceQuery {
    pub item: Option<String>,
    pub filter: Option<String>,
    pub sort_col: Option<String>,
    pub sort_dir: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub struct ExportQuery {
    pub item: String,
    pub format: Option<String>,
}

pub async fn get_bootstrap(State(state): State<AppState>) -> AppResult<Json<BootstrapResponse>> {
    let connections = catalog::list_connections(&state.catalog_pool).await?;
    let first = connections
        .first()
        .cloned()
        .ok_or_else(|| AppError::NotFound("No database connections registered".into()))?;
    let workspace = load_workspace_for(
        &state,
        &first,
        WorkspaceQuery {
            item: None,
            filter: None,
            sort_col: None,
            sort_dir: None,
            limit: Some(100),
            offset: Some(0),
        },
    )
    .await?;
    let history = catalog::list_history(&state.catalog_pool, 50).await?;
    let saved_queries = catalog::list_saved_queries(&state.catalog_pool).await?;
    Ok(Json(BootstrapResponse {
        connections,
        workspace,
        history,
        saved_queries,
    }))
}

pub async fn discovery_scan(
    State(state): State<AppState>,
) -> AppResult<Json<guidb_shared::ServerDiscoveryResponse>> {
    let report = discovery::scan_server_databases(&state).await?;
    Ok(Json(report))
}

pub async fn discovery_provision(
    State(state): State<AppState>,
    Json(req): Json<guidb_shared::ProvisionEngineRequest>,
) -> AppResult<Json<guidb_shared::ProvisionEngineResponse>> {
    let res = discovery::provision_local_engine(&state, req.engine).await?;
    Ok(Json(res))
}

pub async fn list_connections(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<ConnectionProfile>>> {
    Ok(Json(catalog::list_connections(&state.catalog_pool).await?))
}

pub async fn create_connection(
    State(state): State<AppState>,
    Json(req): Json<CreateConnectionRequest>,
) -> AppResult<(StatusCode, Json<ConnectionProfile>)> {
    let created = catalog::create_connection(&state, req).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn delete_connection(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    catalog::delete_connection(&state, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn test_connection(
    State(state): State<AppState>,
    Json(req): Json<TestConnectionRequest>,
) -> AppResult<Json<TestConnectionResponse>> {
    let res = test_connection_direct(&state, req.engine, &req.target).await?;
    Ok(Json(res))
}

pub async fn get_workspace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<WorkspaceQuery>,
) -> AppResult<Json<WorkspaceResponse>> {
    let conn = catalog::get_connection(&state.catalog_pool, &id).await?;
    let ws = load_workspace_for(&state, &conn, q).await?;
    Ok(Json(ws))
}

pub async fn load_workspace_for(
    state: &AppState,
    conn: &ConnectionProfile,
    q: WorkspaceQuery,
) -> AppResult<WorkspaceResponse> {
    let adapter = resolve_adapter(state, conn).await?;
    let items = adapter.list_items().await?;
    let active_item = q
        .item
        .filter(|s| !s.trim().is_empty())
        .or_else(|| items.first().map(|i| i.name.clone()));

    let (structure, data) = if let Some(ref item_name) = active_item {
        let st = adapter.inspect_structure(item_name).await.ok();
        let data = adapter
            .fetch_item_data(
                item_name,
                DataQueryOptions {
                    filter: q.filter,
                    sort_col: q.sort_col,
                    sort_dir: q.sort_dir,
                    limit: q.limit.unwrap_or(100),
                    offset: q.offset.unwrap_or(0),
                },
            )
            .await
            .ok();
        (st, data)
    } else {
        (None, None)
    };

    let erd = adapter
        .build_erd(&items)
        .await
        .unwrap_or(guidb_shared::ErdGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
        });

    Ok(WorkspaceResponse {
        connection: conn.clone(),
        model: conn.engine.model(),
        items,
        active_item,
        structure,
        data,
        erd,
    })
}

pub async fn execute_query(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ExecuteQueryRequest>,
) -> AppResult<Json<QueryResult>> {
    let conn = catalog::get_connection(&state.catalog_pool, &id).await?;
    if let Err(err) = safety::check_query_safety(conn.engine, conn.protection_mode, &req.query) {
        catalog::record_history(
            &state.catalog_pool,
            &conn.id,
            &conn.name,
            req.query.trim(),
            &format!("BLOCKED: {err}"),
            0,
            0,
        )
        .await;
        return Err(err);
    }
    let adapter = resolve_adapter(&state, &conn).await?;
    match adapter.execute_query(req.item.as_deref(), &req.query).await {
        Ok(res) => {
            let rows_metric = if res.affected_rows > 0 {
                res.affected_rows as i64
            } else {
                res.rows.len() as i64
            };
            catalog::record_history(
                &state.catalog_pool,
                &conn.id,
                &conn.name,
                req.query.trim(),
                "OK",
                rows_metric,
                res.duration_ms as i64,
            )
            .await;
            Ok(Json(res))
        }
        Err(err) => {
            catalog::record_history(
                &state.catalog_pool,
                &conn.id,
                &conn.name,
                req.query.trim(),
                &format!("ERR: {err}"),
                0,
                0,
            )
            .await;
            Err(err)
        }
    }
}

pub async fn mutate_staged(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<MutateRequest>,
) -> AppResult<Json<MutationReport>> {
    let conn = catalog::get_connection(&state.catalog_pool, &id).await?;
    if let Err(err) = safety::check_mutations_safety(conn.protection_mode, &req.mutations) {
        catalog::record_history(
            &state.catalog_pool,
            &conn.id,
            &conn.name,
            "STAGED MUTATION BATCH",
            &format!("BLOCKED: {err}"),
            0,
            0,
        )
        .await;
        return Err(err);
    }
    let adapter = resolve_adapter(&state, &conn).await?;
    if conn.engine.model() == guidb_shared::DataModel::Relational {
        let mut keys = std::collections::HashMap::new();
        for mutation in &req.mutations {
            use guidb_shared::StagedMutation;
            let (item, selected) = match mutation {
                StagedMutation::Update { item, pk_match, .. }
                | StagedMutation::Delete { item, pk_match } => (item, pk_match),
                StagedMutation::Insert { .. } => continue,
            };
            if !keys.contains_key(item) {
                let primary: Vec<_> = adapter
                    .inspect_structure(item)
                    .await?
                    .columns
                    .into_iter()
                    .filter(|c| c.is_pk)
                    .map(|c| c.name)
                    .collect();
                keys.insert(item.clone(), primary);
            }
            let primary = &keys[item];
            if primary.is_empty()
                || primary.len() != selected.len()
                || primary.iter().any(|c| !selected.contains_key(c))
            {
                return Err(AppError::BadRequest(
                    "Perubahan baris memerlukan primary key lengkap.".into(),
                ));
            }
        }
    }
    let previews = preview_mutations(&req.mutations, conn.engine);
    let report = adapter.apply_staged_mutations(&req.mutations).await?;
    catalog::record_history(
        &state.catalog_pool,
        &conn.id,
        &conn.name,
        &previews.join("\n"),
        "OK (STAGED COMMIT)",
        report.committed_count as i64,
        report.duration_ms as i64,
    )
    .await;
    Ok(Json(report))
}

pub async fn execute_ddl(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(action): Json<DdlAction>,
) -> AppResult<Json<DdlReport>> {
    let conn = catalog::get_connection(&state.catalog_pool, &id).await?;
    if let Err(err) = safety::check_ddl_safety(conn.protection_mode, &action) {
        catalog::record_history(
            &state.catalog_pool,
            &conn.id,
            &conn.name,
            "DDL ACTION",
            &format!("BLOCKED: {err}"),
            0,
            0,
        )
        .await;
        return Err(err);
    }
    let adapter = resolve_adapter(&state, &conn).await?;
    let report = adapter.execute_ddl(action).await?;
    catalog::record_history(
        &state.catalog_pool,
        &conn.id,
        &conn.name,
        &report.executed_sql,
        "OK (DDL)",
        0,
        1,
    )
    .await;
    Ok(Json(report))
}

pub async fn schema_sync_diff(
    State(state): State<AppState>,
    Json(req): Json<SchemaSyncDiffRequest>,
) -> AppResult<Json<SchemaSyncDiffResponse>> {
    let res = sync::compute_schema_diff(&state, &req.source_conn_id, &req.target_conn_id).await?;
    Ok(Json(res))
}

pub async fn schema_sync_apply(
    State(state): State<AppState>,
    Json(req): Json<SchemaSyncApplyRequest>,
) -> AppResult<Json<SchemaSyncApplyResponse>> {
    let target_conn = catalog::get_connection(&state.catalog_pool, &req.target_conn_id).await?;
    if target_conn.protection_mode == ProtectionMode::ReadOnly {
        return Err(AppError::BadRequest(
            "DIBLOKIR (Mode READ-ONLY): Target Schema Sync terkunci hanya-baca.".into(),
        ));
    }
    let plan = sync::compute_schema_diff(&state, &req.source_conn_id, &req.target_conn_id).await?;
    for statement in &req.statements {
        if !plan
            .differences
            .iter()
            .any(|diff| diff.migration_sql == *statement)
        {
            return Err(AppError::BadRequest(
                "Skema telah berubah. Hitung ulang perbedaan sebelum menerapkan migrasi.".into(),
            ));
        }
        if !statement.trim_start().starts_with("--") {
            safety::check_query_safety(target_conn.engine, target_conn.protection_mode, statement)?;
        }
    }
    backup::create_backup(
        &state,
        &req.target_conn_id,
        BackupKind::PreMigrationAuto,
        guidb_shared::BackupScope::FullSchemaAndData,
        Some("Auto Pre-Schema-Sync Backup".to_string()),
    )
    .await?;

    let res = sync::apply_schema_sync(
        &state,
        &req.source_conn_id,
        &req.target_conn_id,
        &req.statements,
    )
    .await?;
    Ok(Json(res))
}

pub async fn update_protection(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateProtectionRequest>,
) -> AppResult<Json<ConnectionProfile>> {
    let updated =
        catalog::update_connection_protection(&state.catalog_pool, &id, req.protection_mode)
            .await?;
    Ok(Json(updated))
}

pub async fn get_backups(
    State(state): State<AppState>,
) -> AppResult<Json<BackupDashboardResponse>> {
    Ok(Json(backup::get_backup_dashboard(&state).await?))
}

pub async fn create_backup(
    State(state): State<AppState>,
    Json(req): Json<CreateBackupRequest>,
) -> AppResult<(StatusCode, Json<BackupRecord>)> {
    let rec = backup::create_backup(
        &state,
        &req.connection_id,
        BackupKind::Manual,
        req.scope,
        req.label,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(rec)))
}

pub async fn verify_backup(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<VerifyBackupResponse>> {
    Ok(Json(backup::verify_backup(&state, &id).await?))
}

pub async fn restore_backup(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RestoreBackupRequest>,
) -> AppResult<(StatusCode, Json<RestoreBackupResponse>)> {
    let res = backup::restore_backup_isolated(&state, &id, req).await?;
    Ok((StatusCode::CREATED, Json(res)))
}

pub async fn download_backup(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (file_name, bytes) = backup::get_backup_file_content(&state, &id).await?;
    let mut headers = HeaderMap::new();
    let content_type = if file_name.ends_with(".json") {
        "application/json"
    } else if file_name.ends_with(".sqlite") {
        "application/vnd.sqlite3"
    } else {
        "application/sql"
    };
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    let disp = header::HeaderValue::from_str(&format!("attachment; filename=\"{file_name}\""))
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    headers.insert(header::CONTENT_DISPOSITION, disp);
    Ok((headers, bytes))
}

pub async fn create_schedule(
    State(state): State<AppState>,
    Json(req): Json<CreateScheduleRequest>,
) -> AppResult<(StatusCode, Json<BackupSchedule>)> {
    let sched = backup::create_schedule(&state, req).await?;
    Ok((StatusCode::CREATED, Json(sched)))
}

pub async fn toggle_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ToggleScheduleRequest>,
) -> AppResult<Json<BackupSchedule>> {
    Ok(Json(
        backup::toggle_schedule(&state, &id, req.enabled).await?,
    ))
}

pub async fn run_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<BackupRecord>> {
    Ok(Json(backup::run_schedule_now(&state, &id).await?))
}

pub async fn delete_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    backup::delete_schedule(&state, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_history(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<QueryHistoryEntry>>> {
    Ok(Json(catalog::list_history(&state.catalog_pool, 50).await?))
}

pub async fn list_saved_queries(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<SavedQueryEntry>>> {
    Ok(Json(
        catalog::list_saved_queries(&state.catalog_pool).await?,
    ))
}

pub async fn create_saved_query(
    State(state): State<AppState>,
    Json(req): Json<CreateSavedQueryRequest>,
) -> AppResult<(StatusCode, Json<SavedQueryEntry>)> {
    let entry = catalog::create_saved_query(&state.catalog_pool, req).await?;
    Ok((StatusCode::CREATED, Json(entry)))
}

pub async fn delete_saved_query(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    catalog::delete_saved_query(&state.catalog_pool, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn export_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ExportQuery>,
) -> AppResult<impl IntoResponse> {
    let conn = catalog::get_connection(&state.catalog_pool, &id).await?;
    let adapter = resolve_adapter(&state, &conn).await?;
    let mut data = adapter
        .fetch_item_data(
            &q.item,
            DataQueryOptions {
                filter: None,
                sort_col: None,
                sort_dir: None,
                limit: 500,
                offset: 0,
            },
        )
        .await?;

    if conn.engine.model() == guidb_shared::DataModel::Relational {
        let expected = data.total_rows.unwrap_or(data.rows.len() as i64);
        while (data.rows.len() as i64) < expected {
            let page = adapter
                .fetch_item_data(
                    &q.item,
                    DataQueryOptions {
                        filter: None,
                        sort_col: None,
                        sort_dir: None,
                        limit: 500,
                        offset: data.rows.len() as i64,
                    },
                )
                .await?;
            if page.columns != data.columns
                || page.rows.is_empty()
                || page.total_rows != Some(expected)
            {
                return Err(AppError::BadRequest(
                    "Tabel berubah selama ekspor. Ulangi ekspor.".into(),
                ));
            }
            data.rows.extend(page.rows);
        }
    }

    let safe_filename: String = q
        .item
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .collect();
    let base_name = if safe_filename.is_empty() {
        "export"
    } else {
        safe_filename.as_str()
    };

    let fmt = q.format.as_deref().unwrap_or("csv").to_ascii_lowercase();
    let mut headers = HeaderMap::new();
    match fmt.as_str() {
        "json" => {
            let records: Vec<serde_json::Map<String, serde_json::Value>> = data
                .rows
                .iter()
                .map(|row| {
                    data.columns
                        .iter()
                        .zip(row.iter())
                        .map(|(col, cell)| {
                            let v = match cell {
                                guidb_shared::CellValue::Null => serde_json::Value::Null,
                                guidb_shared::CellValue::Bool(b) => serde_json::Value::Bool(*b),
                                guidb_shared::CellValue::Int(i) => serde_json::json!(i),
                                guidb_shared::CellValue::Float(f) => serde_json::json!(f),
                                guidb_shared::CellValue::Text(s) => {
                                    serde_json::Value::String(s.clone())
                                }
                                guidb_shared::CellValue::Json(j) => j.clone(),
                                guidb_shared::CellValue::Bytes(b) => {
                                    serde_json::Value::String(b.clone())
                                }
                            };
                            (col.clone(), v)
                        })
                        .collect()
                })
                .collect();
            let body = serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".to_string());
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            let disp = header::HeaderValue::from_str(&format!(
                "attachment; filename=\"{base_name}.json\""
            ))
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
            headers.insert(header::CONTENT_DISPOSITION, disp);
            Ok((headers, body))
        }
        "sql" => {
            let qt = quote_ident(&q.item, conn.engine);
            let cols: Vec<String> = data
                .columns
                .iter()
                .map(|c| quote_ident(c, conn.engine))
                .collect();
            let mut body = String::new();
            for row in &data.rows {
                let vals: Vec<String> = row.iter().map(format_sql_literal).collect();
                body.push_str(&format!(
                    "INSERT INTO {qt} ({}) VALUES ({});\n",
                    cols.join(", "),
                    vals.join(", ")
                ));
            }
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/sql"),
            );
            let disp =
                header::HeaderValue::from_str(&format!("attachment; filename=\"{base_name}.sql\""))
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
            headers.insert(header::CONTENT_DISPOSITION, disp);
            Ok((headers, body))
        }
        _ => {
            let mut body = String::new();
            body.push_str(
                &data
                    .columns
                    .iter()
                    .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            body.push('\n');
            for row in &data.rows {
                let cells: Vec<String> = row
                    .iter()
                    .map(|c| {
                        let txt = c.display_text();
                        if txt.contains([',', '"', '\n']) {
                            format!("\"{}\"", txt.replace('"', "\"\""))
                        } else {
                            txt
                        }
                    })
                    .collect();
                body.push_str(&cells.join(","));
                body.push('\n');
            }
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/csv; charset=utf-8"),
            );
            let disp =
                header::HeaderValue::from_str(&format!("attachment; filename=\"{base_name}.csv\""))
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
            headers.insert(header::CONTENT_DISPOSITION, disp);
            Ok((headers, body))
        }
    }
}
