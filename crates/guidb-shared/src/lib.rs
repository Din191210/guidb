pub mod sql_gen;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Logical data model supported by a database connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataModel {
    Relational,
    Document,
    KeyValue,
}

/// Concrete database engine driver kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Sqlite,
    Postgres,
    Mysql,
    MongoDb,
    Redis,
}

impl EngineKind {
    pub fn model(self) -> DataModel {
        match self {
            EngineKind::Sqlite | EngineKind::Postgres | EngineKind::Mysql => DataModel::Relational,
            EngineKind::MongoDb => DataModel::Document,
            EngineKind::Redis => DataModel::KeyValue,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EngineKind::Sqlite => "SQLite",
            EngineKind::Postgres => "PostgreSQL",
            EngineKind::Mysql => "MySQL / MariaDB",
            EngineKind::MongoDb => "MongoDB (Document)",
            EngineKind::Redis => "Redis (Key-Value)",
        }
    }
}

/// Environment tag for visual safety badges on connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    Local,
    Staging,
    Production,
}

impl EnvironmentKind {
    pub fn label(self) -> &'static str {
        match self {
            EnvironmentKind::Local => "LOCAL",
            EnvironmentKind::Staging => "STAGING",
            EnvironmentKind::Production => "PROD",
        }
    }
}

/// Protection policy preventing accidental database or table destruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionMode {
    ReadOnly,
    #[default]
    SafeGuard,
    Unrestricted,
}

impl ProtectionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ProtectionMode::ReadOnly => "read_only",
            ProtectionMode::SafeGuard => "safe_guard",
            ProtectionMode::Unrestricted => "unrestricted",
        }
    }

    pub fn from_slug(raw: &str) -> Self {
        match raw {
            "read_only" => ProtectionMode::ReadOnly,
            "unrestricted" => ProtectionMode::Unrestricted,
            _ => ProtectionMode::SafeGuard,
        }
    }
}

/// Saved connection profile stored in the persistent catalog database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub engine: EngineKind,
    pub environment: EnvironmentKind,
    pub target: String,
    pub protection_mode: ProtectionMode,
    pub is_builtin: bool,
    pub created_at: String,
}

/// Request payload to create a new saved connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateConnectionRequest {
    pub name: String,
    pub engine: EngineKind,
    pub environment: EnvironmentKind,
    pub target: String,
    #[serde(default)]
    pub protection_mode: Option<ProtectionMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateProtectionRequest {
    pub protection_mode: ProtectionMode,
}

/// Request payload to test connectivity before saving or connecting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestConnectionRequest {
    pub engine: EngineKind,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestConnectionResponse {
    pub reachable: bool,
    pub latency_ms: u64,
    pub server_version: String,
    pub details: String,
}

/// Uniform typed cell representation across SQL, Document, and Key-Value grids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CellValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Json(serde_json::Value),
    Bytes(String),
}

impl CellValue {
    pub fn display_text(&self) -> String {
        match self {
            CellValue::Null => "NULL".to_string(),
            CellValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            CellValue::Int(i) => i.to_string(),
            CellValue::Float(f) => format!("{f}"),
            CellValue::Text(s) => s.clone(),
            CellValue::Json(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()),
            CellValue::Bytes(b) => b.clone(),
        }
    }

    pub fn from_input(raw: &str, declared_type: Option<&str>) -> Self {
        let trimmed = raw.trim();
        if trimmed.eq_ignore_ascii_case("NULL") {
            return CellValue::Null;
        }
        let upper = declared_type.unwrap_or("").to_uppercase();
        if upper.contains("INT") {
            if let Ok(i) = trimmed.parse::<i64>() {
                return CellValue::Int(i);
            }
        }
        if upper.contains("REAL") || upper.contains("FLOAT") || upper.contains("DOUBLE") || upper.contains("NUMERIC") || upper.contains("DECIMAL") {
            if let Ok(f) = trimmed.parse::<f64>() {
                return CellValue::Float(f);
            }
        }
        if upper.contains("BOOL") {
            if trimmed.eq_ignore_ascii_case("true") || trimmed == "1" {
                return CellValue::Bool(true);
            }
            if trimmed.eq_ignore_ascii_case("false") || trimmed == "0" {
                return CellValue::Bool(false);
            }
        }
        if upper.contains("JSON") || trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                return CellValue::Json(v);
            }
        }
        CellValue::Text(raw.to_string())
    }
}

/// Object explorer item (table, view, collection, or keyspace prefix).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemInfo {
    pub name: String,
    pub item_type: String,
    pub row_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub is_pk: bool,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyDef {
    pub name: String,
    pub from_column: String,
    pub to_table: String,
    pub to_column: String,
    pub on_delete: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructureDef {
    pub item_name: String,
    pub item_type: String,
    pub columns: Vec<ColumnDef>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    pub ddl: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub column_types: Vec<String>,
    pub pk_columns: Vec<String>,
    pub rows: Vec<Vec<CellValue>>,
    pub total_rows: Option<i64>,
    pub affected_rows: u64,
    pub duration_ms: u64,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErdNode {
    pub table_name: String,
    pub row_count: i64,
    pub columns: Vec<ColumnDef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErdEdge {
    pub from_table: String,
    pub from_column: String,
    pub to_table: String,
    pub to_column: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErdGraph {
    pub nodes: Vec<ErdNode>,
    pub edges: Vec<ErdEdge>,
}

/// TablePlus-style staged mutation built in the UI grid before transactional commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StagedMutation {
    Insert {
        item: String,
        values: BTreeMap<String, CellValue>,
    },
    Update {
        item: String,
        pk_match: BTreeMap<String, CellValue>,
        changes: BTreeMap<String, CellValue>,
    },
    Delete {
        item: String,
        pk_match: BTreeMap<String, CellValue>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutateRequest {
    pub mutations: Vec<StagedMutation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationReport {
    pub committed_count: usize,
    pub executed_statements: Vec<String>,
    pub duration_ms: u64,
}

/// Request payload for executing a query, aggregation pipeline, or Redis command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteQueryRequest {
    pub item: Option<String>,
    pub query: String,
}

/// SQLyog-style DDL / Admin actions triggered from right-click / toolbar controls.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DdlAction {
    CreateTable {
        table_name: String,
        columns: Vec<NewColumnSpec>,
    },
    AddColumn {
        table_name: String,
        column: NewColumnSpec,
    },
    CreateIndex {
        table_name: String,
        index_name: String,
        columns: Vec<String>,
        unique: bool,
    },
    TruncateItem {
        item_name: String,
    },
    DropItem {
        item_name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewColumnSpec {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub is_pk: bool,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DdlReport {
    pub executed_sql: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaDiffKind {
    MissingTable,
    MissingColumn,
    MissingIndex,
    TypeMismatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaDiffItem {
    pub kind: SchemaDiffKind,
    pub object_name: String,
    pub detail: String,
    pub migration_sql: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSyncDiffRequest {
    pub source_conn_id: String,
    pub target_conn_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSyncDiffResponse {
    pub source_name: String,
    pub target_name: String,
    pub differences: Vec<SchemaDiffItem>,
    pub full_migration_script: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSyncApplyRequest {
    pub source_conn_id: String,
    pub target_conn_id: String,
    pub statements: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSyncApplyResponse {
    pub applied_statements: usize,
    pub remaining_differences: usize,
    pub report: SchemaSyncDiffResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryHistoryEntry {
    pub id: i64,
    pub connection_id: String,
    pub connection_name: String,
    pub statement: String,
    pub status: String,
    pub affected_rows: i64,
    pub duration_ms: i64,
    pub executed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedQueryEntry {
    pub id: String,
    pub connection_id: String,
    pub title: String,
    pub statement: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSavedQueryRequest {
    pub connection_id: String,
    pub title: String,
    pub statement: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceResponse {
    pub connection: ConnectionProfile,
    pub model: DataModel,
    pub items: Vec<ItemInfo>,
    pub active_item: Option<String>,
    pub structure: Option<StructureDef>,
    pub data: Option<QueryResult>,
    pub erd: ErdGraph,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapResponse {
    pub connections: Vec<ConnectionProfile>,
    pub workspace: WorkspaceResponse,
    pub history: Vec<QueryHistoryEntry>,
    pub saved_queries: Vec<SavedQueryEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineStatus {
    Running,
    InstalledStopped,
    NotInstalled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDatabase {
    pub name: String,
    pub size_label: String,
    pub object_count: i64,
    pub engine: EngineKind,
    pub connection_target: String,
    pub already_registered: bool,
    pub existing_connection_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredEngine {
    pub engine: EngineKind,
    pub engine_label: String,
    pub status: EngineStatus,
    pub version: String,
    pub binary_path: Option<String>,
    pub endpoint: Option<String>,
    pub latency_ms: Option<u64>,
    pub parameters: BTreeMap<String, String>,
    pub databases: Vec<DiscoveredDatabase>,
    pub can_provision_local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerDiscoveryResponse {
    pub hostname: String,
    pub os_info: String,
    pub scanned_at: String,
    pub total_engines_detected: usize,
    pub running_engines_count: usize,
    pub total_databases_found: usize,
    pub engines: Vec<DiscoveredEngine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionEngineRequest {
    pub engine: EngineKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionEngineResponse {
    pub connection: ConnectionProfile,
    pub message: String,
    pub discovery: ServerDiscoveryResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupKind {
    Manual,
    Scheduled,
    PreMigrationAuto,
}

impl BackupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackupKind::Manual => "manual",
            BackupKind::Scheduled => "scheduled",
            BackupKind::PreMigrationAuto => "pre_migration_auto",
        }
    }

    pub fn from_slug(raw: &str) -> Self {
        match raw {
            "scheduled" => BackupKind::Scheduled,
            "pre_migration_auto" => BackupKind::PreMigrationAuto,
            _ => BackupKind::Manual,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackupScope {
    #[default]
    FullSchemaAndData,
    SchemaOnly,
}

impl BackupScope {
    pub fn as_str(self) -> &'static str {
        match self {
            BackupScope::FullSchemaAndData => "full_schema_and_data",
            BackupScope::SchemaOnly => "schema_only",
        }
    }

    pub fn from_slug(raw: &str) -> Self {
        match raw {
            "schema_only" => BackupScope::SchemaOnly,
            _ => BackupScope::FullSchemaAndData,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    pub id: String,
    pub connection_id: String,
    pub connection_name: String,
    pub engine: EngineKind,
    pub kind: BackupKind,
    pub scope: BackupScope,
    pub label: String,
    pub file_name: String,
    pub file_size_bytes: u64,
    pub sha256: String,
    pub object_count: usize,
    pub row_count: usize,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBackupRequest {
    pub connection_id: String,
    #[serde(default)]
    pub scope: BackupScope,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyBackupResponse {
    pub backup_id: String,
    pub valid: bool,
    pub expected_sha256: String,
    pub actual_sha256: String,
    pub file_size_bytes: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreBackupRequest {
    pub new_connection_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreBackupResponse {
    pub connection: ConnectionProfile,
    pub restored_statements: usize,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSchedule {
    pub id: String,
    pub connection_id: String,
    pub connection_name: String,
    pub interval_minutes: i64,
    pub scope: BackupScope,
    pub retention_count: i64,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    pub next_run_at: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateScheduleRequest {
    pub connection_id: String,
    pub interval_minutes: i64,
    #[serde(default)]
    pub scope: BackupScope,
    pub retention_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToggleScheduleRequest {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupDashboardResponse {
    pub backups: Vec<BackupRecord>,
    pub schedules: Vec<BackupSchedule>,
    pub backup_directory: String,
}
