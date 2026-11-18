# GuiDB Studio

Full-stack Rust database management studio (`Axum + Tokio + Leptos SSR + Pure-Rust WASM + SQLx`) built as a unified visual client across Relational SQL (`SQLite`, `PostgreSQL`, `MySQL / MariaDB`), Document (`MongoDB`), and Key-Value (`Redis`) workflows.

## Architecture

- [`crates/guidb-shared`](crates/guidb-shared): Shared domain models (`ProtectionMode`, `BackupRecord`, `BackupSchedule`, `ConnectionProfile`) and SQL/NoSQL generator + formatter helpers.
- [`crates/guidb-wasm`](crates/guidb-wasm): Pure-Rust WebAssembly engine (`static/guidb_wasm.wasm`) handling SQL formatting and staged mutation SQL preview generation in the browser.
- [`crates/guidb-server`](crates/guidb-server): Axum + Tokio HTTP API, Leptos SSR shell, SQLite persistent catalog (`catalog.rs`), multi-engine query/mutation execution, host database discovery (`discovery.rs`), layered safety guardrails (`safety.rs`), and backup scheduler engine (`backup.rs`).
- [`static`](static): Compiled `guidb_wasm.wasm`, CSS design tokens and layout (`app.css`), and interactive studio client (`app.js`).

## Key Capabilities

1. **Unified Multi-Model Browser**: Switch seamlessly between SQLite, PostgreSQL, MySQL/MariaDB tables, MongoDB document collections (with aggregation pipeline support), and Redis key-value namespaces.
2. **Staged Cell Editing & WASM Preview**: Edit cells directly in the Data Grid, stage batch `INSERT` / `UPDATE` / `DELETE` changes, inspect generated SQL via Rust WASM, and commit atomically.
3. **Structure, DDL, SVG ERD, & Schema Sync**: Inspect table DDL, browse interactive SVG entity-relationship diagrams, compare schemas between source and target connections, and apply synchronized migrations.
4. **Deteksi DB (Host Server Auto-Discovery)**: Scan active host database engines, open TCP ports, configuration parameters, and existing schemas/databases without modifying live services (`POST /api/discover`).
5. **Anti-Accidental-Delete Protection (`safety.rs`)**:
   - Permanent global block on `DROP DATABASE`, `DROP SCHEMA`, `SHUTDOWN`, Redis `FLUSHALL`, and `FLUSHDB`.
   - Per-connection persistent policy (`PUT /api/connections/{id}/protection`): `SafeGuard` (default: blocks `DROP TABLE`, `DROP VIEW`, `TRUNCATE`, un-scoped `DELETE` / `UPDATE`, and Redis `DEL`), `ReadOnly`, and `Unrestricted`.
6. **Manual & Scheduled Backups (`backup.rs`)**:
   - Automatic `PreMigrationAuto` snapshot before applying Schema Sync migrations.
   - Manual backups (`FullSchemaAndData` or `SchemaOnly`) with `SHA-256` checksum tracking.
   - Tokio background scheduler (`spawn_backup_scheduler`) with `interval_minutes` and `retention_count` pruning.
   - Checksum verification (`POST /api/backups/{id}/verify`), file download (`GET /api/backups/{id}/download`), and isolated non-destructive restore into a new connection (`POST /api/backups/{id}/restore`).

## Quick Start

```bash
# Run workspace checks & tests
cargo clippy --workspace --offline -- -D warnings
cargo test --workspace --offline

# Start server (default bind: 127.0.0.1:8787)
cargo run -p guidb-server
```

Environment variables (optional):
- `GUIDB_ADDR`: Bind address (default `127.0.0.1:8787`)
- `GUIDB_DATA_DIR`: Catalog, starter SQLite databases, and `backups/` folder (default `./data`)
- `GUIDB_STATIC_DIR`: Static assets directory (default `./static`)
