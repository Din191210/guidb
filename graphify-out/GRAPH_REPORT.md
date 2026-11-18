# Graph Report - guidb  (2026-11-18)

## Corpus Check
- 28 files · ~33,416 words
- Verdict: corpus is large enough that graph structure adds value.
- Unclassified: 7 file(s) not represented in the graph (top: .sqlite 5, .css 1, .wasm 1)

## Summary
- 531 nodes · 1696 edges · 28 communities (17 shown, 11 thin omitted)
- Extraction: 96% EXTRACTED · 4% INFERRED · 0% AMBIGUOUS · INFERRED: 60 edges (avg confidence: 0.85)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- AppResult
- sqlite.rs
- CellValue
- api_integration.rs
- guidb-shared/src/lib.rs
- app.js
- quote_ident
- catalog.rs
- discovery.rs
- DESIGN.md - GuiDB Studio
- guidb-shared
- verify_all.sh
- guidb-wasm/src/lib.rs
- sync.rs
- SqliteAdapter
- StagedMutation
- guidb-server/src/lib.rs
- ProtectionMode
- create_connection
- PostgresAdapter
- BackupKind
- bind_mysql

## God Nodes (most connected - your core abstractions)
1. `AppResult` - 133 edges
2. `AppState` - 81 edges
3. `EngineKind` - 35 edges
4. `CellValue` - 28 edges
5. `ConnectionProfile` - 27 edges
6. `QueryResult` - 26 edges
7. `bindEvents()` - 26 edges
8. `StagedMutation` - 25 edges
9. `DdlAction` - 23 edges
10. `quote_ident()` - 22 edges

## Surprising Connections (you probably didn't know these)
- `load_workspace_for()` --calls--> `resolve_adapter()`  [INFERRED]
  crates/guidb-server/src/api.rs → crates/guidb-server/src/drivers/mod.rs
- `execute_query()` --calls--> `resolve_adapter()`  [INFERRED]
  crates/guidb-server/src/api.rs → crates/guidb-server/src/drivers/mod.rs
- `mutate_staged()` --calls--> `resolve_adapter()`  [INFERRED]
  crates/guidb-server/src/api.rs → crates/guidb-server/src/drivers/mod.rs
- `mutate_staged()` --calls--> `preview_mutations()`  [INFERRED]
  crates/guidb-server/src/api.rs → crates/guidb-shared/src/sql_gen.rs
- `execute_ddl()` --calls--> `resolve_adapter()`  [INFERRED]
  crates/guidb-server/src/api.rs → crates/guidb-server/src/drivers/mod.rs

## Import Cycles
- None detected.

## Communities (28 total, 11 thin omitted)

### Community 0 - "AppResult"
Cohesion: 0.07
Nodes (89): create_backup(), create_connection(), create_saved_query(), create_schedule(), delete_connection(), delete_saved_query(), delete_schedule(), discovery_provision() (+81 more)

### Community 1 - "sqlite.rs"
Cohesion: 0.06
Nodes (29): cell_to_val(), docs_to_query_result(), DocumentAdapter, matches_filter(), run_group_stage(), derive_namespace(), KvAdapter, read_resp_value() (+21 more)

### Community 2 - "CellValue"
Cohesion: 0.13
Nodes (17): CellValue, Bool, Bytes, Float, Int, Null, Text, cell_to_json() (+9 more)

### Community 3 - "api_integration.rs"
Cohesion: 0.14
Nodes (7): spawn_test_app(), test_bootstrap_and_ssr_page(), test_mongodb_aggregation_and_redis_commands(), test_protection_guardrails_and_backup_restore_schedule(), test_schema_sync_and_ddl_and_connection_crud(), test_server_discovery_scan_reports_engines_and_params(), test_staged_mutations_commit_and_persist()

### Community 4 - "guidb-shared/src/lib.rs"
Cohesion: 0.09
Nodes (28): DatabaseAdapter, get_or_open_mysql(), get_or_open_postgres(), get_or_open_sqlite(), resolve_adapter(), ColumnDef, ConnectionProfile, DataModel (+20 more)

### Community 5 - "app.js"
Cohesion: 0.17
Nodes (42): apiJson(), applySchemaSync(), bindEvents(), boot(), callWasmStringFn(), cellToDisplay(), clearBanner(), clearStagedChanges() (+34 more)

### Community 6 - "quote_ident"
Cohesion: 0.27
Nodes (4): MysqlAdapter, build_fallback_create_table(), quote_ident(), validate_identifier()

### Community 7 - "catalog.rs"
Cohesion: 0.23
Nodes (10): delete_saved_query(), ensure_catalog_schema(), init_app_state(), open_sqlite_pool(), seed_builtin_databases(), seed_commerce_core(), seed_commerce_staging(), seed_edge_kv() (+2 more)

### Community 8 - "discovery.rs"
Cohesion: 0.09
Nodes (32): NosqlBackupBundle, ensure_or_create_connection(), find_binary(), find_registered(), format_bytes(), inspect_mongodb(), inspect_mysql_mariadb(), inspect_postgres() (+24 more)

### Community 9 - "DESIGN.md - GuiDB Studio"
Cohesion: 0.40
Nodes (4): DESIGN.md - GuiDB Studio, Design Read, Dial Rationale & Visual Decisions (R-31), Responsive Breakpoints

### Community 10 - "guidb-shared"
Cohesion: 0.67
Nodes (3): guidb-server, guidb-shared, guidb-wasm

### Community 12 - "guidb-wasm/src/lib.rs"
Cohesion: 0.21
Nodes (4): pack_output(), wasm_alloc(), wasm_format_sql(), wasm_preview_mutations()

### Community 15 - "sync.rs"
Cohesion: 0.12
Nodes (8): normalize_type(), AppError, BadRequest, Database, Internal, NotFound, ErrorBody, Json

### Community 17 - "StagedMutation"
Cohesion: 0.18
Nodes (6): bind_pg(), StagedMutation, Delete, Insert, Update, PreviewInput

### Community 19 - "ProtectionMode"
Cohesion: 0.25
Nodes (8): check_ddl_safety(), check_mutations_safety(), check_query_safety(), strip_comments_and_collapse(), ProtectionMode, ReadOnly, SafeGuard, Unrestricted

### Community 20 - "create_connection"
Cohesion: 0.22
Nodes (9): create_connection(), engine_to_str(), env_to_str(), resolve_sqlite_path(), CreateConnectionRequest, EnvironmentKind, Local, Production (+1 more)

### Community 24 - "BackupKind"
Cohesion: 0.33
Nodes (4): BackupKind, Manual, PreMigrationAuto, Scheduled

## Knowledge Gaps
- **45 isolated node(s):** `guidb-server`, `Relational`, `Document`, `KeyValue`, `Sqlite` (+40 more)
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 127 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)
- **11 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `AppResult` connect `AppResult` to `sqlite.rs`, `guidb-shared/src/lib.rs`, `quote_ident`, `catalog.rs`, `discovery.rs`, `sync.rs`, `SqliteAdapter`, `StagedMutation`, `ProtectionMode`, `create_connection`, `PostgresAdapter`?**
  _High betweenness centrality (0.214) - this node is a cross-community bridge._
- **What connects `guidb-server`, `Relational`, `Document` to the rest of the system?**
  _45 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `AppResult` be split into smaller, more focused modules?**
  _Cohesion score 0.06617526617526617 - nodes in this community are weakly interconnected._
- **Why does `AppState` connect `AppResult` to `guidb-shared/src/lib.rs`, `catalog.rs`, `discovery.rs`, `sync.rs`, `guidb-server/src/lib.rs`, `create_connection`, `state.rs`?**
  _High betweenness centrality (0.070) - this node is a cross-community bridge._
- **Should `sqlite.rs` be split into smaller, more focused modules?**
  _Cohesion score 0.059097127222982215 - nodes in this community are weakly interconnected._
- **Why does `EngineKind` connect `discovery.rs` to `AppResult`, `CellValue`, `guidb-shared/src/lib.rs`, `quote_ident`, `catalog.rs`, `guidb-wasm/src/lib.rs`, `sync.rs`, `StagedMutation`, `ProtectionMode`, `create_connection`?**
  _High betweenness centrality (0.065) - this node is a cross-community bridge._
- **Should `CellValue` be split into smaller, more focused modules?**
  _Cohesion score 0.13438735177865613 - nodes in this community are weakly interconnected._