use crate::error::{AppError, AppResult};
use crate::state::AppState;
use anyhow::Context;
use dashmap::DashMap;
use guidb_shared::{
    ConnectionProfile, CreateConnectionRequest, CreateSavedQueryRequest, EngineKind,
    EnvironmentKind, ProtectionMode, QueryHistoryEntry, SavedQueryEntry,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

pub async fn open_sqlite_pool(path: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating database dir {}", parent.display()))?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .with_context(|| format!("Failed connecting to SQLite {}", path.display()))
}

pub fn resolve_sqlite_path(data_dir: &Path, target: &str) -> PathBuf {
    let stripped = target.strip_prefix("sqlite://").unwrap_or(target);
    let p = PathBuf::from(stripped);
    if p.is_absolute() {
        p
    } else {
        data_dir.join(p)
    }
}

pub async fn init_app_state(data_dir: PathBuf) -> anyhow::Result<AppState> {
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("Failed creating data directory {}", data_dir.display()))?;

    let catalog_path = data_dir.join("guidb_catalog.sqlite");
    let catalog_pool = open_sqlite_pool(&catalog_path).await?;
    ensure_catalog_schema(&catalog_pool).await?;

    let state = AppState {
        data_dir: data_dir.clone(),
        catalog_pool,
        sqlite_pools: Arc::new(DashMap::new()),
        pg_pools: Arc::new(DashMap::new()),
        mysql_pools: Arc::new(DashMap::new()),
    };

    seed_builtin_databases(&state).await?;
    Ok(state)
}

async fn ensure_catalog_schema(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::raw_sql(
        r#"
        CREATE TABLE IF NOT EXISTS connections (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            engine TEXT NOT NULL,
            environment TEXT NOT NULL,
            target TEXT NOT NULL,
            protection_mode TEXT NOT NULL DEFAULT 'safe_guard',
            is_builtin INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS query_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            connection_id TEXT NOT NULL,
            connection_name TEXT NOT NULL,
            statement TEXT NOT NULL,
            status TEXT NOT NULL,
            affected_rows INTEGER NOT NULL DEFAULT 0,
            duration_ms INTEGER NOT NULL DEFAULT 0,
            executed_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS saved_queries (
            id TEXT PRIMARY KEY,
            connection_id TEXT NOT NULL,
            title TEXT NOT NULL,
            statement TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS backup_records (
            id TEXT PRIMARY KEY,
            connection_id TEXT NOT NULL,
            connection_name TEXT NOT NULL,
            engine TEXT NOT NULL,
            kind TEXT NOT NULL,
            scope TEXT NOT NULL,
            label TEXT NOT NULL,
            file_name TEXT NOT NULL,
            file_size_bytes INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            object_count INTEGER NOT NULL,
            row_count INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS backup_schedules (
            id TEXT PRIMARY KEY,
            connection_id TEXT NOT NULL,
            connection_name TEXT NOT NULL,
            interval_minutes INTEGER NOT NULL,
            scope TEXT NOT NULL,
            retention_count INTEGER NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            last_run_at TEXT,
            next_run_at TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await
    .context("Failed creating catalog tables")?;

    // Migrate pre-existing connections table cleanly if protection_mode column is absent
    let cols = sqlx::query("PRAGMA table_info(connections)")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let has_protection = cols.iter().any(|r| {
        r.try_get::<String, _>("name")
            .is_ok_and(|n| n == "protection_mode")
    });
    if !has_protection {
        sqlx::query(
            "ALTER TABLE connections ADD COLUMN protection_mode TEXT NOT NULL DEFAULT 'safe_guard'",
        )
        .execute(pool)
        .await
        .context("Failed migrating connections.protection_mode")?;
    }

    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let migrated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pragma_table_info('backup_records') WHERE name='schedule_id'",
    )
    .fetch_one(&mut *tx)
    .await?;
    if migrated == 0 {
        sqlx::query("ALTER TABLE backup_records ADD COLUMN schedule_id TEXT")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE backup_records SET schedule_id = (SELECT min(s.id) FROM backup_schedules s WHERE s.connection_id = backup_records.connection_id HAVING count(*) = 1) WHERE kind = 'scheduled'").execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn seed_builtin_databases(state: &AppState) -> anyhow::Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM connections")
        .fetch_one(&state.catalog_pool)
        .await
        .context("Failed counting connections")?;

    let now = chrono::Utc::now().to_rfc3339();
    if count == 0 {
        let builtins = [
            (
                "conn-commerce-core",
                "Commerce Core (SQLite)",
                "sqlite",
                "local",
                "commerce_core.sqlite",
            ),
            (
                "conn-commerce-staging",
                "Commerce Staging (Sync Target)",
                "sqlite",
                "staging",
                "commerce_staging.sqlite",
            ),
            (
                "conn-telemetry-docs",
                "Telemetry Docs (MongoDB)",
                "mongo_db",
                "local",
                "telemetry_docs.sqlite",
            ),
            (
                "conn-edge-kv",
                "Edge Cache (Redis)",
                "redis",
                "local",
                "edge_kv.sqlite",
            ),
        ];
        for (id, name, engine, env, target) in builtins {
            sqlx::query(
                "INSERT INTO connections (id, name, engine, environment, target, is_builtin, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)",
            )
            .bind(id)
            .bind(name)
            .bind(engine)
            .bind(env)
            .bind(target)
            .bind(&now)
            .execute(&state.catalog_pool)
            .await
            .context("Failed seeding builtin connection")?;
        }

        let saved = [
            (
                "sq-revenue",
                "conn-commerce-core",
                "Top Customer Revenue (JOIN)",
                "SELECT c.id, c.company_name, c.city, COUNT(o.id) AS total_orders, COALESCE(SUM(o.total_amount), 0) AS revenue_idr\nFROM customers c\nLEFT JOIN orders o ON o.customer_id = c.id\nGROUP BY c.id, c.company_name, c.city\nORDER BY revenue_idr DESC\nLIMIT 20;",
            ),
            (
                "sq-explain",
                "conn-commerce-core",
                "Explain Orders Index Usage",
                "EXPLAIN QUERY PLAN\nSELECT id, order_code, status, total_amount\nFROM orders\nWHERE customer_id = 1 AND status = 'PAID';",
            ),
            (
                "sq-mongo-agg",
                "conn-telemetry-docs",
                "Aggregate Errors by Service",
                "[\n  {\"$match\": {\"severity\": \"error\"}},\n  {\"$group\": {\"_id\": \"$service\", \"events\": {\"$sum\": 1}, \"avg_latency\": {\"$avg\": \"$latency_ms\"}}},\n  {\"$sort\": {\"events\": -1}}\n]",
            ),
            (
                "sq-redis-info",
                "conn-edge-kv",
                "Inspect Session Hash & Info",
                "HGETALL session:usr_901",
            ),
        ];
        for (id, cid, title, stmt) in saved {
            sqlx::query(
                "INSERT INTO saved_queries (id, connection_id, title, statement, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(id)
            .bind(cid)
            .bind(title)
            .bind(stmt)
            .bind(&now)
            .execute(&state.catalog_pool)
            .await
            .context("Failed seeding saved queries")?;
        }
    }

    seed_commerce_core(&state.data_dir.join("commerce_core.sqlite")).await?;
    seed_commerce_staging(&state.data_dir.join("commerce_staging.sqlite")).await?;
    seed_telemetry_docs(&state.data_dir.join("telemetry_docs.sqlite")).await?;
    seed_edge_kv(&state.data_dir.join("edge_kv.sqlite")).await?;

    let sched_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backup_schedules")
        .fetch_one(&state.catalog_pool)
        .await
        .unwrap_or(0);
    if sched_count == 0 {
        let next_run = (chrono::Utc::now() + chrono::Duration::minutes(60)).to_rfc3339();
        let _ = sqlx::query(
            "INSERT INTO backup_schedules (id, connection_id, connection_name, interval_minutes, scope, retention_count, enabled, last_run_at, next_run_at, created_at)
             VALUES ('sched-commerce-hourly', 'conn-commerce-core', 'Commerce Core (SQLite)', 60, 'full_schema_and_data', 7, 1, NULL, ?1, ?2)",
        )
        .bind(&next_run)
        .bind(&now)
        .execute(&state.catalog_pool)
        .await;
    }

    Ok(())
}

async fn seed_commerce_core(path: &Path) -> anyhow::Result<()> {
    let pool = open_sqlite_pool(path).await?;
    let existing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='customers'",
    )
    .fetch_one(&pool)
    .await?;
    if existing > 0 {
        pool.close().await;
        return Ok(());
    }

    let ddl = r#"
    CREATE TABLE customers (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        company_name TEXT NOT NULL,
        contact_email TEXT NOT NULL UNIQUE,
        city TEXT NOT NULL,
        tier TEXT NOT NULL DEFAULT 'STANDARD',
        credit_limit INTEGER NOT NULL DEFAULT 25000000,
        created_at TEXT NOT NULL
    );
    CREATE TABLE products (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        sku TEXT NOT NULL UNIQUE,
        product_name TEXT NOT NULL,
        category TEXT NOT NULL,
        unit_price INTEGER NOT NULL,
        stock_qty INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE orders (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        order_code TEXT NOT NULL UNIQUE,
        customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
        status TEXT NOT NULL,
        total_amount INTEGER NOT NULL,
        placed_at TEXT NOT NULL
    );
    CREATE TABLE order_items (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        order_id INTEGER NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
        product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
        qty INTEGER NOT NULL,
        line_total INTEGER NOT NULL
    );
    CREATE TABLE shipments (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        order_id INTEGER NOT NULL UNIQUE REFERENCES orders(id) ON DELETE CASCADE,
        carrier TEXT NOT NULL,
        tracking_no TEXT NOT NULL,
        shipped_at TEXT NOT NULL
    );
    CREATE INDEX idx_orders_customer_status ON orders(customer_id, status);
    CREATE INDEX idx_orders_placed_at ON orders(placed_at);
    CREATE INDEX idx_order_items_order ON order_items(order_id);
    CREATE VIEW v_customer_revenue AS
    SELECT c.id AS customer_id, c.company_name, c.tier, COUNT(o.id) AS order_count, COALESCE(SUM(o.total_amount), 0) AS total_revenue
    FROM customers c
    LEFT JOIN orders o ON o.customer_id = c.id
    GROUP BY c.id, c.company_name, c.tier;
    "#;
    sqlx::raw_sql(ddl).execute(&pool).await?;

    let seed = r#"
    INSERT INTO customers (company_name, contact_email, city, tier, credit_limit, created_at) VALUES
        ('PT Nusantara Logistik', 'ops@nusantara-logistik.co.id', 'Jakarta', 'ENTERPRISE', 150000000, '2026-11-01T08:30:00Z'),
        ('CV Sinar Presisi', 'finance@sinarpresisi.id', 'Surabaya', 'GROWTH', 60000000, '2026-11-03T10:15:00Z'),
        ('PT Karya Semesta', 'procurement@karyasemesta.co.id', 'Bandung', 'ENTERPRISE', 120000000, '2026-11-05T14:00:00Z'),
        ('UD Mandiri Teknik', 'admin@mandiriteknik.com', 'Semarang', 'STANDARD', 25000000, '2026-11-07T09:45:00Z'),
        ('PT Delta Energi', 'billing@deltaenergi.id', 'Medan', 'GROWTH', 85000000, '2026-11-10T11:20:00Z');

    INSERT INTO products (sku, product_name, category, unit_price, stock_qty) VALUES
        ('SKU-SRV-101', 'Rack Server 2U NVMe Node', 'Infrastructure', 28500000, 14),
        ('SKU-NET-204', 'Managed 24-Port SFP+ Switch', 'Networking', 12400000, 29),
        ('SKU-STR-309', 'Enterprise SAS Storage Array', 'Storage', 42000000, 7),
        ('SKU-UPS-412', 'Online Double-Conversion UPS 3kVA', 'Power', 7800000, 19);

    INSERT INTO orders (order_code, customer_id, status, total_amount, placed_at) VALUES
        ('ORD-2026-1001', 1, 'PAID', 57000000, '2026-11-12T09:00:00Z'),
        ('ORD-2026-1002', 2, 'SHIPPED', 24800000, '2026-11-13T13:20:00Z'),
        ('ORD-2026-1003', 3, 'PAID', 49800000, '2026-11-14T16:10:00Z'),
        ('ORD-2026-1004', 1, 'PENDING', 15600000, '2026-11-15T11:05:00Z'),
        ('ORD-2026-1005', 4, 'PAID', 12400000, '2026-11-16T08:40:00Z');

    INSERT INTO order_items (order_id, product_id, qty, line_total) VALUES
        (1, 1, 2, 57000000),
        (2, 2, 2, 24800000),
        (3, 3, 1, 42000000),
        (3, 4, 1, 7800000),
        (4, 4, 2, 15600000),
        (5, 2, 1, 12400000);

    INSERT INTO shipments (order_id, carrier, tracking_no, shipped_at) VALUES
        (1, 'JNE Cargo', 'JNEC-99812031', '2026-11-12T17:30:00Z'),
        (2, 'SiCepat Kargo', 'SCK-77412904', '2026-11-14T08:15:00Z'),
        (3, 'DHL Express ID', 'DHL-55201944', '2026-11-15T10:00:00Z');
    "#;
    sqlx::raw_sql(seed).execute(&pool).await?;
    pool.close().await;
    Ok(())
}

async fn seed_commerce_staging(path: &Path) -> anyhow::Result<()> {
    let pool = open_sqlite_pool(path).await?;
    let existing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='customers'",
    )
    .fetch_one(&pool)
    .await?;
    if existing > 0 {
        pool.close().await;
        return Ok(());
    }

    // Intentionally omits credit_limit column on customers, idx_orders_placed_at index, and shipments table
    // so SQLyog-style Schema Sync detects real structural drift and generates executable DDL migrations.
    let ddl = r#"
    CREATE TABLE customers (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        company_name TEXT NOT NULL,
        contact_email TEXT NOT NULL UNIQUE,
        city TEXT NOT NULL,
        tier TEXT NOT NULL DEFAULT 'STANDARD',
        created_at TEXT NOT NULL
    );
    CREATE TABLE products (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        sku TEXT NOT NULL UNIQUE,
        product_name TEXT NOT NULL,
        category TEXT NOT NULL,
        unit_price INTEGER NOT NULL,
        stock_qty INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE orders (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        order_code TEXT NOT NULL UNIQUE,
        customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
        status TEXT NOT NULL,
        total_amount INTEGER NOT NULL,
        placed_at TEXT NOT NULL
    );
    CREATE TABLE order_items (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        order_id INTEGER NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
        product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
        qty INTEGER NOT NULL,
        line_total INTEGER NOT NULL
    );
    CREATE INDEX idx_orders_customer_status ON orders(customer_id, status);
    CREATE INDEX idx_order_items_order ON order_items(order_id);
    INSERT INTO customers (company_name, contact_email, city, tier, created_at) VALUES
        ('Staging Test Account', 'qa@staging.local', 'Jakarta', 'STANDARD', '2026-11-01T00:00:00Z');
    "#;
    sqlx::raw_sql(ddl).execute(&pool).await?;
    pool.close().await;
    Ok(())
}

async fn seed_telemetry_docs(path: &Path) -> anyhow::Result<()> {
    let pool = open_sqlite_pool(path).await?;
    let existing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_guidb_collections'",
    )
    .fetch_one(&pool)
    .await?;
    if existing > 0 {
        pool.close().await;
        return Ok(());
    }

    let ddl = r#"
    CREATE TABLE _guidb_collections (
        name TEXT PRIMARY KEY,
        created_at TEXT NOT NULL
    );
    CREATE TABLE _guidb_documents (
        collection TEXT NOT NULL,
        doc_id TEXT NOT NULL,
        doc_json TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (collection, doc_id)
    );
    "#;
    sqlx::raw_sql(ddl).execute(&pool).await?;

    let collections = ["service_events", "audit_logs", "feature_flags"];
    for col in collections {
        sqlx::query("INSERT INTO _guidb_collections (name, created_at) VALUES (?1, ?2)")
            .bind(col)
            .bind("2026-11-18T00:00:00Z")
            .execute(&pool)
            .await?;
    }

    let docs = [
        (
            "service_events",
            "673b1001a9c1000000000001",
            r#"{"_id":"673b1001a9c1000000000001","service":"checkout-api","severity":"error","latency_ms":412,"status_code":502,"region":"ap-southeast-3","message":"Upstream payment gateway timeout"}"#,
        ),
        (
            "service_events",
            "673b1001a9c1000000000002",
            r#"{"_id":"673b1001a9c1000000000002","service":"auth-gateway","severity":"info","latency_ms":18,"status_code":200,"region":"ap-southeast-3","message":"JWT token refreshed"}"#,
        ),
        (
            "service_events",
            "673b1001a9c1000000000003",
            r#"{"_id":"673b1001a9c1000000000003","service":"checkout-api","severity":"error","latency_ms":530,"status_code":504,"region":"ap-southeast-1","message":"Idempotency lock contention"}"#,
        ),
        (
            "service_events",
            "673b1001a9c1000000000004",
            r#"{"_id":"673b1001a9c1000000000004","service":"inventory-worker","severity":"warn","latency_ms":145,"status_code":200,"region":"ap-southeast-3","message":"Stock sync lag exceeded 100ms"}"#,
        ),
        (
            "audit_logs",
            "673b2001a9c1000000000001",
            r#"{"_id":"673b2001a9c1000000000001","actor":"dba@nusantara.id","action":"ALTER_INDEX","target":"orders.idx_orders_placed_at","approved":true}"#,
        ),
        (
            "audit_logs",
            "673b2001a9c1000000000002",
            r#"{"_id":"673b2001a9c1000000000002","actor":"deploy-bot","action":"ROLLOUT","target":"v2.14.0","approved":true}"#,
        ),
        (
            "feature_flags",
            "673b3001a9c1000000000001",
            r#"{"_id":"673b3001a9c1000000000001","flag":"instant_qris_settlement","enabled":true,"rollout_pct":100}"#,
        ),
        (
            "feature_flags",
            "673b3001a9c1000000000002",
            r#"{"_id":"673b3001a9c1000000000002","flag":"multi_warehouse_routing","enabled":false,"rollout_pct":15}"#,
        ),
    ];

    for (col, id, json) in docs {
        sqlx::query(
            "INSERT INTO _guidb_documents (collection, doc_id, doc_json, updated_at) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(col)
        .bind(id)
        .bind(json)
        .bind("2026-11-18T07:00:00Z")
        .execute(&pool)
        .await?;
    }

    pool.close().await;
    Ok(())
}

async fn seed_edge_kv(path: &Path) -> anyhow::Result<()> {
    let pool = open_sqlite_pool(path).await?;
    let existing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_guidb_kv'",
    )
    .fetch_one(&pool)
    .await?;
    if existing > 0 {
        pool.close().await;
        return Ok(());
    }

    let ddl = r#"
    CREATE TABLE _guidb_kv (
        key TEXT PRIMARY KEY,
        namespace TEXT NOT NULL,
        data_type TEXT NOT NULL,
        value_text TEXT NOT NULL,
        ttl_seconds INTEGER NOT NULL DEFAULT -1,
        byte_size INTEGER NOT NULL DEFAULT 0,
        updated_at TEXT NOT NULL
    );
    "#;
    sqlx::raw_sql(ddl).execute(&pool).await?;

    let keys = [
        (
            "session:usr_901",
            "session:*",
            "HASH",
            r#"{"user_id":"usr_901","role":"dba_lead","ip":"10.20.4.18","mfa":"verified"}"#,
            3540_i64,
        ),
        (
            "session:usr_412",
            "session:*",
            "HASH",
            r#"{"user_id":"usr_412","role":"backend_dev","ip":"10.20.4.29","mfa":"verified"}"#,
            1820_i64,
        ),
        (
            "ratelimit:api:10.0.0.8",
            "ratelimit:*",
            "STRING",
            "842",
            58_i64,
        ),
        (
            "config:feature_rollout",
            "config:*",
            "STRING",
            r#"{"read_replica":"enabled","max_pool":32}"#,
            -1_i64,
        ),
        (
            "queue:webhook_jobs",
            "queue:*",
            "LIST",
            r#"["job_9011","job_9012","job_9013"]"#,
            -1_i64,
        ),
        (
            "leaderboard:query_ms",
            "leaderboard:*",
            "ZSET",
            r#"[{"member":"orders_by_cust","score":2.4},{"member":"customer_rev","score":8.1}]"#,
            86400_i64,
        ),
    ];

    for (key, ns, dt, val, ttl) in keys {
        sqlx::query(
            "INSERT INTO _guidb_kv (key, namespace, data_type, value_text, ttl_seconds, byte_size, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(key)
        .bind(ns)
        .bind(dt)
        .bind(val)
        .bind(ttl)
        .bind(val.len() as i64)
        .bind("2026-11-18T07:30:00Z")
        .execute(&pool)
        .await?;
    }

    pool.close().await;
    Ok(())
}

pub fn parse_engine(raw: &str) -> EngineKind {
    match raw {
        "sqlite" => EngineKind::Sqlite,
        "postgres" => EngineKind::Postgres,
        "mysql" => EngineKind::Mysql,
        "mongo_db" => EngineKind::MongoDb,
        "redis" => EngineKind::Redis,
        _ => EngineKind::Sqlite,
    }
}

pub fn engine_to_str(engine: EngineKind) -> &'static str {
    match engine {
        EngineKind::Sqlite => "sqlite",
        EngineKind::Postgres => "postgres",
        EngineKind::Mysql => "mysql",
        EngineKind::MongoDb => "mongo_db",
        EngineKind::Redis => "redis",
    }
}

pub fn parse_env(raw: &str) -> EnvironmentKind {
    match raw {
        "staging" => EnvironmentKind::Staging,
        "production" => EnvironmentKind::Production,
        _ => EnvironmentKind::Local,
    }
}

pub fn env_to_str(env: EnvironmentKind) -> &'static str {
    match env {
        EnvironmentKind::Local => "local",
        EnvironmentKind::Staging => "staging",
        EnvironmentKind::Production => "production",
    }
}

pub async fn list_connections(pool: &SqlitePool) -> AppResult<Vec<ConnectionProfile>> {
    let rows = sqlx::query(
        "SELECT id, name, engine, environment, target, protection_mode, is_builtin, created_at
         FROM connections ORDER BY is_builtin DESC, created_at ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|r| ConnectionProfile {
            id: r.get::<String, _>("id"),
            name: r.get::<String, _>("name"),
            engine: parse_engine(&r.get::<String, _>("engine")),
            environment: parse_env(&r.get::<String, _>("environment")),
            target: r.get::<String, _>("target"),
            protection_mode: ProtectionMode::from_slug(
                r.try_get::<String, _>("protection_mode")
                    .as_deref()
                    .unwrap_or("safe_guard"),
            ),
            is_builtin: r.get::<i64, _>("is_builtin") != 0,
            created_at: r.get::<String, _>("created_at"),
        })
        .collect())
}

pub async fn get_connection(pool: &SqlitePool, id: &str) -> AppResult<ConnectionProfile> {
    let row = sqlx::query(
        "SELECT id, name, engine, environment, target, protection_mode, is_builtin, created_at
         FROM connections WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?
    .ok_or_else(|| AppError::NotFound(format!("Connection '{id}' not found")))?;

    Ok(ConnectionProfile {
        id: row.get::<String, _>("id"),
        name: row.get::<String, _>("name"),
        engine: parse_engine(&row.get::<String, _>("engine")),
        environment: parse_env(&row.get::<String, _>("environment")),
        target: row.get::<String, _>("target"),
        protection_mode: ProtectionMode::from_slug(
            row.try_get::<String, _>("protection_mode")
                .as_deref()
                .unwrap_or("safe_guard"),
        ),
        is_builtin: row.get::<i64, _>("is_builtin") != 0,
        created_at: row.get::<String, _>("created_at"),
    })
}

pub async fn update_connection_protection(
    pool: &SqlitePool,
    id: &str,
    mode: ProtectionMode,
) -> AppResult<ConnectionProfile> {
    let _ = get_connection(pool, id).await?;
    sqlx::query("UPDATE connections SET protection_mode = ?1 WHERE id = ?2")
        .bind(mode.as_str())
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    get_connection(pool, id).await
}

pub async fn ensure_store_schema(pool: &SqlitePool, engine: EngineKind) -> AppResult<()> {
    if engine == EngineKind::MongoDb {
        sqlx::raw_sql(
                "CREATE TABLE IF NOT EXISTS _guidb_collections (name TEXT PRIMARY KEY, created_at TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS _guidb_documents (collection TEXT NOT NULL, doc_id TEXT NOT NULL, doc_json TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (collection, doc_id));",
            )
            .execute(pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
    } else if engine == EngineKind::Redis {
        sqlx::raw_sql(
                "CREATE TABLE IF NOT EXISTS _guidb_kv (key TEXT PRIMARY KEY, namespace TEXT NOT NULL, data_type TEXT NOT NULL, value_text TEXT NOT NULL, ttl_seconds INTEGER NOT NULL DEFAULT -1, byte_size INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL);",
            )
            .execute(pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
    }
    Ok(())
}

pub async fn create_connection(
    state: &AppState,
    req: CreateConnectionRequest,
) -> AppResult<ConnectionProfile> {
    let name = req.name.trim().to_string();
    let target = req.target.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest(
            "Connection name cannot be empty".into(),
        ));
    }
    if target.is_empty() {
        return Err(AppError::BadRequest(
            "Connection target / URI cannot be empty".into(),
        ));
    }

    // Initialize local storage for local SQLite / Document / KV files if target is a local file path
    if matches!(
        req.engine,
        EngineKind::Sqlite | EngineKind::MongoDb | EngineKind::Redis
    ) && !target.starts_with("mongodb://")
        && !target.starts_with("redis://")
    {
        let resolved = resolve_sqlite_path(&state.data_dir, &target);
        let file_pool = open_sqlite_pool(&resolved)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        ensure_store_schema(&file_pool, req.engine).await?;
        file_pool.close().await;
    }

    let id = format!("conn-{}", Uuid::new_v4().simple());
    let now = chrono::Utc::now().to_rfc3339();
    let mode = req.protection_mode.unwrap_or(ProtectionMode::SafeGuard);
    sqlx::query(
        "INSERT INTO connections (id, name, engine, environment, target, protection_mode, is_builtin, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
    )
    .bind(&id)
    .bind(&name)
    .bind(engine_to_str(req.engine))
    .bind(env_to_str(req.environment))
    .bind(&target)
    .bind(mode.as_str())
    .bind(&now)
    .execute(&state.catalog_pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(ConnectionProfile {
        id,
        name,
        engine: req.engine,
        environment: req.environment,
        target,
        protection_mode: mode,
        is_builtin: false,
        created_at: now,
    })
}

pub async fn delete_connection(state: &AppState, id: &str) -> AppResult<()> {
    let conn = get_connection(&state.catalog_pool, id).await?;
    if conn.is_builtin {
        return Err(AppError::BadRequest(
            "Built-in starter connections cannot be deleted".into(),
        ));
    }
    sqlx::query("DELETE FROM connections WHERE id = ?1")
        .bind(id)
        .execute(&state.catalog_pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    state.sqlite_pools.remove(id);
    state.pg_pools.remove(id);
    state.mysql_pools.remove(id);
    Ok(())
}

pub async fn record_history(
    pool: &SqlitePool,
    connection_id: &str,
    connection_name: &str,
    statement: &str,
    status: &str,
    affected_rows: i64,
    duration_ms: i64,
) {
    let now = chrono::Utc::now().to_rfc3339();
    if let Err(err) = sqlx::query(
        "INSERT INTO query_history (connection_id, connection_name, statement, status, affected_rows, duration_ms, executed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(connection_id)
    .bind(connection_name)
    .bind(statement)
    .bind(status)
    .bind(affected_rows)
    .bind(duration_ms)
    .bind(now)
    .execute(pool)
    .await
    {
        tracing::warn!(error = ?err, "Failed recording query history");
    }
}

pub async fn list_history(pool: &SqlitePool, limit: i64) -> AppResult<Vec<QueryHistoryEntry>> {
    let rows = sqlx::query(
        "SELECT id, connection_id, connection_name, statement, status, affected_rows, duration_ms, executed_at
         FROM query_history ORDER BY id DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|r| QueryHistoryEntry {
            id: r.get("id"),
            connection_id: r.get("connection_id"),
            connection_name: r.get("connection_name"),
            statement: r.get("statement"),
            status: r.get("status"),
            affected_rows: r.get("affected_rows"),
            duration_ms: r.get("duration_ms"),
            executed_at: r.get("executed_at"),
        })
        .collect())
}

pub async fn list_saved_queries(pool: &SqlitePool) -> AppResult<Vec<SavedQueryEntry>> {
    let rows = sqlx::query(
        "SELECT id, connection_id, title, statement, created_at
         FROM saved_queries ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|r| SavedQueryEntry {
            id: r.get("id"),
            connection_id: r.get("connection_id"),
            title: r.get("title"),
            statement: r.get("statement"),
            created_at: r.get("created_at"),
        })
        .collect())
}

pub async fn create_saved_query(
    pool: &SqlitePool,
    req: CreateSavedQueryRequest,
) -> AppResult<SavedQueryEntry> {
    let title = req.title.trim().to_string();
    let statement = req.statement.trim().to_string();
    if title.is_empty() || statement.is_empty() {
        return Err(AppError::BadRequest(
            "Title and query statement are required".into(),
        ));
    }
    let id = format!("sq-{}", Uuid::new_v4().simple());
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO saved_queries (id, connection_id, title, statement, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(&id)
    .bind(&req.connection_id)
    .bind(&title)
    .bind(&statement)
    .bind(&now)
    .execute(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(SavedQueryEntry {
        id,
        connection_id: req.connection_id,
        title,
        statement,
        created_at: now,
    })
}

pub async fn delete_saved_query(pool: &SqlitePool, id: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM saved_queries WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// Helper for parsing integer strings cleanly when validating limits/offsets.
pub fn parse_positive_i64(raw: Option<&str>, default: i64, max: i64) -> i64 {
    raw.and_then(|s| i64::from_str(s).ok())
        .map(|v| v.clamp(0, max))
        .unwrap_or(default)
}
