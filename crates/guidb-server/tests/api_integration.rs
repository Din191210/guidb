use guidb_server::{build_router, catalog};
use guidb_shared::{
    BackupDashboardResponse, BackupRecord, BackupSchedule, BackupScope, BootstrapResponse,
    CellValue, DdlAction, DdlReport, EngineKind, EnvironmentKind, MutationReport, NewColumnSpec,
    ProtectionMode, QueryResult, RestoreBackupResponse, SchemaSyncApplyResponse,
    SchemaSyncDiffResponse, ServerDiscoveryResponse, StagedMutation, VerifyBackupResponse,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::net::TcpListener;

async fn spawn_test_app() -> (String, PathBuf) {
    let temp_dir =
        std::env::temp_dir().join(format!("guidb_test_{}", uuid::Uuid::new_v4().simple()));
    let state = catalog::init_app_state(temp_dir.clone())
        .await
        .expect("init test state");
    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../static");
    let router = build_router(state, static_dir);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve axum");
    });
    (format!("http://127.0.0.1:{port}"), temp_dir)
}

#[tokio::test]
async fn test_bootstrap_and_ssr_page() {
    let (base, dir) = spawn_test_app().await;
    let client = reqwest::Client::new();

    let html = client
        .get(format!("{base}/"))
        .send()
        .await
        .expect("GET /")
        .text()
        .await
        .expect("html text");
    assert!(html.contains("GuiDB Studio"));
    assert!(html.contains("guidb-bootstrap-data"));

    let boot: BootstrapResponse = client
        .get(format!("{base}/api/bootstrap"))
        .send()
        .await
        .expect("GET /api/bootstrap")
        .json()
        .await
        .expect("parse bootstrap");

    assert_eq!(boot.connections.len(), 4);
    assert_eq!(boot.workspace.connection.id, "conn-commerce-core");
    assert!(!boot.workspace.erd.nodes.is_empty());
    assert!(!boot.workspace.erd.edges.is_empty());

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_staged_mutations_commit_and_persist() {
    let (base, dir) = spawn_test_app().await;
    let client = reqwest::Client::new();

    let mut insert_vals = BTreeMap::new();
    insert_vals.insert(
        "company_name".to_string(),
        CellValue::Text("PT Integrasi Rust".to_string()),
    );
    insert_vals.insert(
        "contact_email".to_string(),
        CellValue::Text("qa@integrasirust.id".to_string()),
    );
    insert_vals.insert(
        "city".to_string(),
        CellValue::Text("Yogyakarta".to_string()),
    );
    insert_vals.insert(
        "tier".to_string(),
        CellValue::Text("ENTERPRISE".to_string()),
    );
    insert_vals.insert("credit_limit".to_string(), CellValue::Int(95000000));
    insert_vals.insert(
        "created_at".to_string(),
        CellValue::Text("2026-11-18T10:00:00Z".to_string()),
    );

    let mut pk_match = BTreeMap::new();
    pk_match.insert("id".to_string(), CellValue::Int(1));
    let mut update_changes = BTreeMap::new();
    update_changes.insert(
        "city".to_string(),
        CellValue::Text("Jakarta Selatan".to_string()),
    );

    let mut del_pk = BTreeMap::new();
    del_pk.insert("id".to_string(), CellValue::Int(5));

    let mutations = vec![
        StagedMutation::Insert {
            item: "customers".to_string(),
            values: insert_vals,
        },
        StagedMutation::Update {
            item: "customers".to_string(),
            pk_match,
            changes: update_changes,
        },
    ];

    let report: MutationReport = client
        .post(format!("{base}/api/connections/conn-commerce-core/mutate"))
        .json(&serde_json::json!({ "mutations": mutations }))
        .send()
        .await
        .expect("POST mutate")
        .json()
        .await
        .expect("parse report");
    assert_eq!(report.committed_count, 2);

    // SafeGuard blocks row deletion until mode is switched to Unrestricted
    let blocked_del = client
        .post(format!("{base}/api/connections/conn-commerce-core/mutate"))
        .json(&serde_json::json!({
            "mutations": [StagedMutation::Delete {
                item: "customers".to_string(),
                pk_match: del_pk.clone(),
            }]
        }))
        .send()
        .await
        .expect("blocked delete");
    assert_eq!(blocked_del.status().as_u16(), 400);

    let prot_resp = client
        .put(format!(
            "{base}/api/connections/conn-commerce-core/protection"
        ))
        .json(&serde_json::json!({ "protection_mode": ProtectionMode::Unrestricted }))
        .send()
        .await
        .expect("set unrestricted");
    assert_eq!(prot_resp.status().as_u16(), 200);

    let del_ok = client
        .post(format!("{base}/api/connections/conn-commerce-core/mutate"))
        .json(&serde_json::json!({
            "mutations": [StagedMutation::Delete {
                item: "customers".to_string(),
                pk_match: del_pk,
            }]
        }))
        .send()
        .await
        .expect("unrestricted delete");
    assert_eq!(del_ok.status().as_u16(), 200);

    let check: QueryResult = client
        .post(format!("{base}/api/connections/conn-commerce-core/query"))
        .json(&serde_json::json!({
            "query": "SELECT company_name, city FROM customers WHERE contact_email = 'qa@integrasirust.id';"
        }))
        .send()
        .await
        .expect("query inserted row")
        .json()
        .await
        .expect("parse query result");
    assert_eq!(check.rows.len(), 1);
    assert_eq!(
        check.rows[0][0],
        CellValue::Text("PT Integrasi Rust".to_string())
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_mongodb_aggregation_and_redis_commands() {
    let (base, dir) = spawn_test_app().await;
    let client = reqwest::Client::new();

    let agg: QueryResult = client
        .post(format!("{base}/api/connections/conn-telemetry-docs/query"))
        .json(&serde_json::json!({
            "item": "service_events",
            "query": r#"[{"$match":{"severity":"error"}},{"$group":{"_id":"$service","events":{"$sum":1}}}]"#
        }))
        .send()
        .await
        .expect("mongo agg")
        .json()
        .await
        .expect("parse agg");
    assert_eq!(agg.rows.len(), 1);
    assert_eq!(agg.rows[0][0], CellValue::Text("checkout-api".to_string()));
    assert_eq!(agg.rows[0][1], CellValue::Int(2));

    let set_res: QueryResult = client
        .post(format!("{base}/api/connections/conn-edge-kv/query"))
        .json(&serde_json::json!({ "query": "SET api:health ok EX 120" }))
        .send()
        .await
        .expect("redis SET")
        .json()
        .await
        .expect("parse SET");
    assert_eq!(set_res.rows[0][0], CellValue::Text("OK".to_string()));

    let get_res: QueryResult = client
        .post(format!("{base}/api/connections/conn-edge-kv/query"))
        .json(&serde_json::json!({ "query": "GET api:health" }))
        .send()
        .await
        .expect("redis GET")
        .json()
        .await
        .expect("parse GET");
    assert_eq!(get_res.rows[0][2], CellValue::Text("ok".to_string()));
    assert_eq!(get_res.rows[0][3], CellValue::Int(120));

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_schema_sync_and_ddl_and_connection_crud() {
    let (base, dir) = spawn_test_app().await;
    let client = reqwest::Client::new();

    let diff: SchemaSyncDiffResponse = client
        .post(format!("{base}/api/schema-sync/diff"))
        .json(&serde_json::json!({
            "source_conn_id": "conn-commerce-core",
            "target_conn_id": "conn-commerce-staging"
        }))
        .send()
        .await
        .expect("diff")
        .json()
        .await
        .expect("parse diff");
    assert!(
        diff.differences.len() >= 3,
        "expected >=3 diffs, got {}",
        diff.differences.len()
    );

    let statements: Vec<String> = diff
        .differences
        .iter()
        .map(|d| d.migration_sql.clone())
        .collect();
    let applied: SchemaSyncApplyResponse = client
        .post(format!("{base}/api/schema-sync/apply"))
        .json(&serde_json::json!({
            "source_conn_id": "conn-commerce-core",
            "target_conn_id": "conn-commerce-staging",
            "statements": statements
        }))
        .send()
        .await
        .expect("apply sync")
        .json()
        .await
        .expect("parse apply");
    assert_eq!(applied.remaining_differences, 0);

    let ddl_rep: DdlReport = client
        .post(format!("{base}/api/connections/conn-commerce-core/ddl"))
        .json(&DdlAction::CreateTable {
            table_name: "audit_notes".to_string(),
            columns: vec![
                NewColumnSpec {
                    name: "id".to_string(),
                    data_type: "INTEGER".to_string(),
                    nullable: false,
                    is_pk: true,
                    default_value: None,
                },
                NewColumnSpec {
                    name: "body".to_string(),
                    data_type: "TEXT".to_string(),
                    nullable: false,
                    is_pk: false,
                    default_value: None,
                },
            ],
        })
        .send()
        .await
        .expect("ddl create table")
        .json()
        .await
        .expect("parse ddl");
    assert!(ddl_rep.executed_sql.contains("CREATE TABLE"));

    // Create & test new SQLite connection
    let create_resp = client
        .post(format!("{base}/api/connections"))
        .json(&serde_json::json!({
            "name": "Custom Local Test DB",
            "engine": EngineKind::Sqlite,
            "environment": EnvironmentKind::Local,
            "target": "custom_test.sqlite"
        }))
        .send()
        .await
        .expect("create conn");
    assert_eq!(create_resp.status().as_u16(), 201);

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_server_discovery_scan_reports_engines_and_params() {
    let (base, dir) = spawn_test_app().await;
    let client = reqwest::Client::new();

    let report: ServerDiscoveryResponse = client
        .get(format!("{base}/api/discovery/scan"))
        .send()
        .await
        .expect("GET /api/discovery/scan")
        .json()
        .await
        .expect("parse discovery response");

    assert_eq!(report.engines.len(), 5);
    assert!(report.total_engines_detected >= 3);
    assert!(report.total_databases_found >= 4);

    let sqlite_eng = report
        .engines
        .iter()
        .find(|e| e.engine == EngineKind::Sqlite)
        .expect("sqlite engine entry");
    assert!(sqlite_eng
        .databases
        .iter()
        .any(|d| d.name == "commerce_core.sqlite" && d.already_registered));
    assert!(sqlite_eng
        .parameters
        .contains_key("commerce_core.sqlite.journal_mode"));

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_protection_guardrails_and_backup_restore_schedule() {
    let (base, dir) = spawn_test_app().await;
    let client = reqwest::Client::new();

    // 1. Global hard block forbids DROP DATABASE regardless of protection mode
    let drop_db = client
        .post(format!("{base}/api/connections/conn-commerce-core/query"))
        .json(&serde_json::json!({ "query": "DROP DATABASE commerce_core;" }))
        .send()
        .await
        .expect("drop db request");
    assert_eq!(drop_db.status().as_u16(), 400);

    // 2. SafeGuard blocks DROP TABLE and DELETE without WHERE
    let drop_tbl = client
        .post(format!("{base}/api/connections/conn-commerce-core/query"))
        .json(&serde_json::json!({ "query": "DROP TABLE customers;" }))
        .send()
        .await
        .expect("drop table");
    assert_eq!(drop_tbl.status().as_u16(), 400);

    let del_all = client
        .post(format!("{base}/api/connections/conn-commerce-core/query"))
        .json(&serde_json::json!({ "query": "DELETE FROM customers;" }))
        .send()
        .await
        .expect("delete without where");
    assert_eq!(del_all.status().as_u16(), 400);

    // 3. Manual backup, SHA-256 verify, and isolated restore into a brand new DB
    let bkp: BackupRecord = client
        .post(format!("{base}/api/backups/create"))
        .json(&serde_json::json!({
            "connection_id": "conn-commerce-core",
            "scope": BackupScope::FullSchemaAndData,
            "label": "Test Snapshot"
        }))
        .send()
        .await
        .expect("create backup")
        .json()
        .await
        .expect("parse backup record");
    assert!(bkp.file_size_bytes > 100);
    assert!(bkp.row_count >= 10);

    let verify: VerifyBackupResponse = client
        .post(format!("{base}/api/backups/{}/verify", bkp.id))
        .send()
        .await
        .expect("verify backup")
        .json()
        .await
        .expect("parse verify");
    assert!(verify.valid);

    let restored: RestoreBackupResponse = client
        .post(format!("{base}/api/backups/{}/restore", bkp.id))
        .json(&serde_json::json!({ "new_connection_name": "Isolated Clone DB" }))
        .send()
        .await
        .expect("restore backup")
        .json()
        .await
        .expect("parse restore");
    assert_eq!(restored.connection.name, "Isolated Clone DB");
    assert_ne!(restored.connection.id, "conn-commerce-core");

    // 4. Create & run scheduled backup
    let sched: BackupSchedule = client
        .post(format!("{base}/api/backups/schedules"))
        .json(&serde_json::json!({
            "connection_id": "conn-telemetry-docs",
            "interval_minutes": 30,
            "scope": BackupScope::FullSchemaAndData,
            "retention_count": 3
        }))
        .send()
        .await
        .expect("create schedule")
        .json()
        .await
        .expect("parse schedule");

    let run_rec: BackupRecord = client
        .post(format!("{base}/api/backups/schedules/{}/run", sched.id))
        .send()
        .await
        .expect("run schedule")
        .json()
        .await
        .expect("parse run record");
    assert!(run_rec.file_name.ends_with(".json"));

    let dash: BackupDashboardResponse = client
        .get(format!("{base}/api/backups"))
        .send()
        .await
        .expect("get dashboard")
        .json()
        .await
        .expect("parse dashboard");
    assert!(dash.backups.len() >= 2);
    assert!(dash.schedules.len() >= 2);

    let _ = std::fs::remove_dir_all(dir);
}
