use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use guidb_shared::{
    ConnectionProfile, CreateConnectionRequest, DiscoveredDatabase, DiscoveredEngine, EngineKind,
    EngineStatus, EnvironmentKind, ProvisionEngineResponse, ServerDiscoveryResponse,
};
use sqlx::mysql::MySqlPoolOptions;
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

pub async fn scan_server_databases(state: &AppState) -> AppResult<ServerDiscoveryResponse> {
    let existing = catalog::list_connections(&state.catalog_pool).await?;
    let mut target_to_id: HashMap<(String, String), String> = HashMap::new();
    for c in &existing {
        target_to_id.insert(
            (catalog::engine_to_str(c.engine).to_string(), c.target.clone()),
            c.id.clone(),
        );
    }

    let hostname = read_hostname();
    let os_info = read_os_pretty_name();

    let (mysql_eng, pg_eng, sqlite_eng, mongo_eng, redis_eng) = tokio::join!(
        inspect_mysql_mariadb(state, &existing, &target_to_id),
        inspect_postgres(state, &existing, &target_to_id),
        inspect_sqlite(state, &target_to_id),
        inspect_mongodb(state, &target_to_id),
        inspect_redis(state, &target_to_id),
    );

    let engines = vec![mysql_eng, pg_eng, sqlite_eng, mongo_eng, redis_eng];
    let total_engines_detected = engines
        .iter()
        .filter(|e| e.status != EngineStatus::NotInstalled)
        .count();
    let running_engines_count = engines
        .iter()
        .filter(|e| e.status == EngineStatus::Running)
        .count();
    let total_databases_found = engines.iter().map(|e| e.databases.len()).sum();

    Ok(ServerDiscoveryResponse {
        hostname,
        os_info,
        scanned_at: chrono::Utc::now().to_rfc3339(),
        total_engines_detected,
        running_engines_count,
        total_databases_found,
        engines,
    })
}

fn read_hostname() -> String {
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string())
}

fn read_os_pretty_name() -> String {
    if let Ok(raw) = std::fs::read_to_string("/etc/os-release") {
        for line in raw.lines() {
            if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                return rest.trim_matches('"').to_string();
            }
        }
    }
    std::env::consts::OS.to_string()
}

async fn find_binary(candidates: &[&str]) -> Option<String> {
    for c in candidates {
        if Path::new(c).exists() {
            return Some((*c).to_string());
        }
    }
    for c in candidates {
        let name = Path::new(c).file_name().and_then(|s| s.to_str()).unwrap_or(c);
        if let Ok(out) = Command::new("which").arg(name).output().await {
            if out.status.success() {
                let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !p.is_empty() {
                    return Some(p);
                }
            }
        }
    }
    None
}

async fn run_version_cmd(bin: &str, arg: &str) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new(bin).arg(arg).output(),
    )
    .await
    .ok()?
    .ok()?;
    let combined = format!(
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    combined
        .lines()
        .find(|l| !l.trim().is_empty() && !l.contains("[Warning]"))
        .map(|l| l.trim().to_string())
}

async fn tcp_probe(addr: &str) -> Option<u64> {
    let start = Instant::now();
    let res = tokio::time::timeout(Duration::from_millis(600), TcpStream::connect(addr)).await;
    match res {
        Ok(Ok(_)) => Some(start.elapsed().as_millis() as u64),
        _ => None,
    }
}

/// Read the MySQL/MariaDB v10 server handshake greeting packet without credentials.
async fn read_mysql_greeting(addr: &str) -> Option<(u64, String)> {
    let start = Instant::now();
    let mut stream = tokio::time::timeout(Duration::from_millis(600), TcpStream::connect(addr))
        .await
        .ok()?
        .ok()?;
    let latency = start.elapsed().as_millis() as u64;
    let mut buf = [0_u8; 256];
    let n = tokio::time::timeout(Duration::from_millis(600), stream.read(&mut buf))
        .await
        .ok()?
        .ok()?;
    // Packet layout: [3-byte len][1-byte seq][1-byte proto=0x0a][NUL-terminated server version]
    if n > 6 && buf[4] == 0x0a {
        let slice = &buf[5..n];
        let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        let ver = String::from_utf8_lossy(&slice[..end]).trim().to_string();
        if !ver.is_empty() {
            return Some((latency, ver));
        }
    }
    Some((latency, "MySQL/MariaDB wire active".to_string()))
}

fn find_registered(
    map: &HashMap<(String, String), String>,
    engine: EngineKind,
    target: &str,
) -> (bool, Option<String>) {
    let key = (catalog::engine_to_str(engine).to_string(), target.to_string());
    match map.get(&key) {
        Some(id) => (true, Some(id.clone())),
        None => (false, None),
    }
}

fn parse_ini_params(path: &Path, params: &mut BTreeMap<String, String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let clean = line.trim();
        if clean.is_empty()
            || clean.starts_with('#')
            || clean.starts_with(';')
            || clean.starts_with('[')
            || clean.starts_with('!')
        {
            continue;
        }
        if let Some((k, v)) = clean.split_once('=') {
            let key = k.trim();
            let val = v.trim();
            if matches!(
                key,
                "datadir"
                    | "socket"
                    | "port"
                    | "log-error"
                    | "pid-file"
                    | "bind-address"
                    | "max_connections"
                    | "innodb_buffer_pool_size"
            ) {
                params.insert(format!("cfg.{key}"), val.to_string());
            }
        }
    }
}

async fn inspect_mysql_mariadb(
    state: &AppState,
    existing: &[ConnectionProfile],
    registered: &HashMap<(String, String), String>,
) -> DiscoveredEngine {
    let bin = find_binary(&[
        "/usr/bin/mariadbd",
        "/usr/bin/mysqld",
        "/usr/bin/mariadb",
        "/usr/bin/mysql",
    ])
    .await;
    let mut params: BTreeMap<String, String> = BTreeMap::new();
    let mut version = if let Some(ref b) = bin {
        run_version_cmd(b, "--version")
            .await
            .unwrap_or_else(|| "Installed".to_string())
    } else {
        "Not installed".to_string()
    };

    for cfg in [
        Path::new("/etc/my.cnf"),
        Path::new("/etc/my.cnf.d/mariadb-server.cnf"),
    ] {
        if cfg.exists() {
            params
                .entry("config_files".to_string())
                .and_modify(|s: &mut String| {
                    s.push_str(", ");
                    s.push_str(&cfg.display().to_string());
                })
                .or_insert_with(|| cfg.display().to_string());
            parse_ini_params(cfg, &mut params);
        }
    }
    let local_cnf = state.data_dir.join("local_mariadb/my.cnf");
    if local_cnf.exists() {
        parse_ini_params(&local_cnf, &mut params);
    }

    let mut open_ports: Vec<(String, u64)> = Vec::new();
    for addr in ["127.0.0.1:3306", "127.0.0.1:3307", "127.0.0.1:33307"] {
        if let Some((lat, greeting_ver)) = read_mysql_greeting(addr).await {
            params.insert(format!("handshake.{addr}"), greeting_ver.clone());
            if !greeting_ver.contains("wire active") {
                version = format!("{greeting_ver} (Live on {addr})");
            }
            open_ports.push((addr.to_string(), lat));
        }
    }

    if !open_ports.is_empty() {
        let port_list: Vec<&str> = open_ports.iter().map(|(a, _)| a.as_str()).collect();
        params.insert("listening_ports".to_string(), port_list.join(", "));
    }

    let mut databases = Vec::new();
    let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
    let saved_mysql_dsns: Vec<String> = existing
        .iter()
        .filter(|c| c.engine == EngineKind::Mysql)
        .map(|c| c.target.clone())
        .collect();

    let mut any_authenticated = false;
    for (addr, _) in &open_ports {
        let mut candidates: Vec<String> = saved_mysql_dsns
            .iter()
            .filter(|d| d.contains(addr))
            .cloned()
            .collect();
        candidates.push(format!("mysql://root@{addr}/information_schema"));
        candidates.push(format!("mysql://{user}@{addr}/information_schema"));

        let mut port_authed = false;
        for dsn in candidates {
            if let Ok(pool) = MySqlPoolOptions::new()
                .max_connections(2)
                .acquire_timeout(Duration::from_secs(2))
                .connect(&dsn)
                .await
            {
                port_authed = true;
                any_authenticated = true;
                let base_dsn = dsn
                    .rsplit_once('/')
                    .map(|(b, _)| b.to_string())
                    .unwrap_or_else(|| format!("mysql://root@{addr}"));

                if let Ok(row) = sqlx::query(
                    "SELECT @@version AS v, @@port AS p, @@socket AS s, @@datadir AS dd,
                            @@max_connections AS mc, @@character_set_server AS cs,
                            @@collation_server AS col, CAST(@@innodb_buffer_pool_size AS CHAR) AS bp",
                )
                .fetch_one(&pool)
                .await
                {
                    params.insert("live.version".into(), row.try_get::<String, _>("v").unwrap_or_default());
                    params.insert("live.port".into(), row.try_get::<u32, _>("p").map(|v| v.to_string()).unwrap_or_default());
                    params.insert("live.socket".into(), row.try_get::<String, _>("s").unwrap_or_default());
                    params.insert("live.datadir".into(), row.try_get::<String, _>("dd").unwrap_or_default());
                    params.insert("live.max_connections".into(), row.try_get::<u32, _>("mc").map(|v| v.to_string()).unwrap_or_default());
                    params.insert("live.charset".into(), row.try_get::<String, _>("cs").unwrap_or_default());
                    params.insert("live.collation".into(), row.try_get::<String, _>("col").unwrap_or_default());
                    params.insert("live.innodb_buffer_pool".into(), row.try_get::<String, _>("bp").unwrap_or_default());
                }

                if let Ok(db_rows) = sqlx::query(
                    "SELECT s.SCHEMA_NAME AS db_name,
                            COUNT(t.TABLE_NAME) AS tbl_count,
                            COALESCE(SUM(t.DATA_LENGTH + t.INDEX_LENGTH), 0) AS bytes_used
                     FROM information_schema.SCHEMATA s
                     LEFT JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = s.SCHEMA_NAME
                     GROUP BY s.SCHEMA_NAME
                     ORDER BY s.SCHEMA_NAME ASC",
                )
                .fetch_all(&pool)
                .await
                {
                    for r in db_rows {
                        let db_name: String = r.try_get("db_name").unwrap_or_default();
                        let tbl_count: i64 = r.try_get("tbl_count").unwrap_or(0);
                        let bytes_used: i64 = r.try_get("bytes_used").unwrap_or(0);
                        let target = format!("{base_dsn}/{db_name}");
                        let (already_registered, existing_connection_id) =
                            find_registered(registered, EngineKind::Mysql, &target);
                        databases.push(DiscoveredDatabase {
                            name: db_name,
                            size_label: format_bytes(bytes_used.max(0) as u64),
                            object_count: tbl_count,
                            engine: EngineKind::Mysql,
                            connection_target: target,
                            already_registered,
                            existing_connection_id,
                        });
                    }
                }
                pool.close().await;
                break;
            }
        }
        if !port_authed {
            params.insert(
                format!("auth.{addr}"),
                "Aktif - memerlukan username/password untuk membaca skema".to_string(),
            );
        }
    }

    // If no port authenticated, supplement with on-disk datadir database directories if readable
    if !any_authenticated {
        if let Ok(entries) = std::fs::read_dir("/var/lib/mysql") {
            for ent in entries.flatten() {
                if ent.path().is_dir() {
                    let name = ent.file_name().to_string_lossy().to_string();
                    let target = format!("mysql://root@127.0.0.1:3306/{name}");
                    let (already_registered, existing_connection_id) =
                        find_registered(registered, EngineKind::Mysql, &target);
                    databases.push(DiscoveredDatabase {
                        name,
                        size_label: "On-disk datadir".to_string(),
                        object_count: 0,
                        engine: EngineKind::Mysql,
                        connection_target: target,
                        already_registered,
                        existing_connection_id,
                    });
                }
            }
        }
    }

    let status = if !open_ports.is_empty() {
        EngineStatus::Running
    } else if bin.is_some() {
        params.insert(
            "service_state".into(),
            "Terpasang (daemon tidak aktif di port 3306)".into(),
        );
        EngineStatus::InstalledStopped
    } else {
        EngineStatus::NotInstalled
    };

    let endpoint = open_ports
        .first()
        .map(|(a, _)| a.clone())
        .or_else(|| Some("127.0.0.1:3306".to_string()));
    let latency_ms = open_ports.first().map(|(_, l)| *l);
    let can_provision_local = bin.is_some() && !open_ports.iter().any(|(a, _)| a.ends_with(":33307"));

    DiscoveredEngine {
        engine: EngineKind::Mysql,
        engine_label: "MySQL / MariaDB".to_string(),
        status,
        version,
        binary_path: bin,
        endpoint,
        latency_ms,
        parameters: params,
        databases,
        can_provision_local,
    }
}

async fn inspect_postgres(
    state: &AppState,
    existing: &[ConnectionProfile],
    registered: &HashMap<(String, String), String>,
) -> DiscoveredEngine {
    let bin = find_binary(&["/usr/bin/postgres", "/usr/bin/psql"]).await;
    let mut params: BTreeMap<String, String> = BTreeMap::new();
    let version = if let Some(ref b) = bin {
        run_version_cmd(b, "--version")
            .await
            .unwrap_or_else(|| "Installed".to_string())
    } else {
        "Not installed".to_string()
    };

    if Path::new("/usr/bin/initdb").exists() {
        params.insert("initdb_bin".into(), "/usr/bin/initdb".into());
    }
    if Path::new("/usr/bin/pg_ctl").exists() {
        params.insert("pg_ctl_bin".into(), "/usr/bin/pg_ctl".into());
    }
    let pg_local_conf = state.data_dir.join("local_postgres/postgresql.conf");
    if pg_local_conf.exists() {
        params.insert("local_config".into(), pg_local_conf.display().to_string());
    }

    let mut open_ports: Vec<(String, u64)> = Vec::new();
    for addr in [
        "127.0.0.1:5432",
        "127.0.0.1:5433",
        "127.0.0.1:5434",
        "127.0.0.1:55432",
        "127.0.0.1:55433",
    ] {
        if let Some(lat) = tcp_probe(addr).await {
            open_ports.push((addr.to_string(), lat));
        }
    }

    if !open_ports.is_empty() {
        let port_list: Vec<&str> = open_ports.iter().map(|(a, _)| a.as_str()).collect();
        params.insert("listening_ports".to_string(), port_list.join(", "));
    }

    let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_string());
    let saved_pg_dsns: Vec<String> = existing
        .iter()
        .filter(|c| c.engine == EngineKind::Postgres)
        .map(|c| c.target.clone())
        .collect();

    let mut databases = Vec::new();
    for (addr, _) in &open_ports {
        let mut candidates: Vec<String> = saved_pg_dsns
            .iter()
            .filter(|d| d.contains(addr))
            .cloned()
            .collect();
        candidates.push(format!("postgres://postgres@{addr}/postgres"));
        candidates.push(format!("postgres://{user}@{addr}/postgres"));
        candidates.push(format!("postgres://postgres:postgres@{addr}/postgres"));

        let mut port_authed = false;
        for dsn in candidates {
            if let Ok(pool) = PgPoolOptions::new()
                .max_connections(2)
                .acquire_timeout(Duration::from_secs(2))
                .connect(&dsn)
                .await
            {
                port_authed = true;
                let base_dsn = dsn
                    .rsplit_once('/')
                    .map(|(b, _)| b.to_string())
                    .unwrap_or_else(|| format!("postgres://postgres@{addr}"));

                if let Ok(setting_rows) = sqlx::query(
                    "SELECT name, setting FROM pg_settings
                     WHERE name IN ('server_version', 'port', 'listen_addresses', 'max_connections', 'shared_buffers', 'data_directory', 'wal_level', 'server_encoding', 'TimeZone')
                     ORDER BY name ASC",
                )
                .fetch_all(&pool)
                .await
                {
                    for r in setting_rows {
                        let k: String = r.try_get("name").unwrap_or_default();
                        let v: String = r.try_get("setting").unwrap_or_default();
                        params.insert(format!("live({addr}).{k}"), v);
                    }
                }

                if let Ok(db_rows) = sqlx::query(
                    "SELECT datname, pg_size_pretty(pg_database_size(datname)) AS size_txt
                     FROM pg_database WHERE datistemplate = false ORDER BY datname ASC",
                )
                .fetch_all(&pool)
                .await
                {
                    for r in db_rows {
                        let db_name: String = r.try_get("datname").unwrap_or_default();
                        let size_txt: String = r.try_get("size_txt").unwrap_or_default();
                        let target = format!("{base_dsn}/{db_name}");
                        let (already_registered, existing_connection_id) =
                            find_registered(registered, EngineKind::Postgres, &target);
                        databases.push(DiscoveredDatabase {
                            name: format!("{db_name} (@{addr})"),
                            size_label: size_txt,
                            object_count: 0,
                            engine: EngineKind::Postgres,
                            connection_target: target,
                            already_registered,
                            existing_connection_id,
                        });
                    }
                }
                pool.close().await;
                break;
            }
        }
        if !port_authed {
            params.insert(
                format!("auth.{addr}"),
                "Aktif - memerlukan username/password untuk membaca daftar database".to_string(),
            );
        }
    }

    let status = if !open_ports.is_empty() {
        EngineStatus::Running
    } else if bin.is_some() {
        params.insert(
            "service_state".into(),
            "Terpasang (daemon tidak aktif di port 5432)".into(),
        );
        EngineStatus::InstalledStopped
    } else {
        EngineStatus::NotInstalled
    };

    let endpoint = open_ports
        .first()
        .map(|(a, _)| a.clone())
        .or_else(|| Some("127.0.0.1:5432".to_string()));
    let latency_ms = open_ports.first().map(|(_, l)| *l);
    let can_provision_local = bin.is_some()
        && Path::new("/usr/bin/initdb").exists()
        && !state.data_dir.join("local_postgres/PG_VERSION").exists();

    DiscoveredEngine {
        engine: EngineKind::Postgres,
        engine_label: "PostgreSQL".to_string(),
        status,
        version,
        binary_path: bin,
        endpoint,
        latency_ms,
        parameters: params,
        databases,
        can_provision_local,
    }
}

async fn inspect_sqlite(
    state: &AppState,
    registered: &HashMap<(String, String), String>,
) -> DiscoveredEngine {
    let bin = find_binary(&[
        "/usr/bin/sqlite3",
        "/home/server1/Android/Sdk/platform-tools/sqlite3",
    ])
    .await;
    let mut params: BTreeMap<String, String> = BTreeMap::new();
    let version = if let Some(ref b) = bin {
        run_version_cmd(b, "--version")
            .await
            .unwrap_or_else(|| "Embedded SQLite".to_string())
    } else {
        "Embedded SQLx SQLite".to_string()
    };
    params.insert("data_directory".into(), state.data_dir.display().to_string());

    let mut databases = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&state.data_dir) {
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e == "sqlite" || e == "db")
            })
            .collect();
        files.sort();

        for file in files {
            let fname = file
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            if fname == "guidb_catalog.sqlite" {
                continue;
            }
            let size_bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            if let Ok(pool) = catalog::open_sqlite_pool(&file).await {
                let is_internal_nosql: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('_guidb_collections', '_guidb_kv')",
                )
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
                if is_internal_nosql > 0 {
                    pool.close().await;
                    continue;
                }

                let tbl_cnt: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'",
                )
                .fetch_one(&pool)
                .await
                .unwrap_or(0);

                let jm: String = sqlx::query_scalar("PRAGMA journal_mode;")
                    .fetch_one(&pool)
                    .await
                    .unwrap_or_else(|_| "unknown".into());
                let ps: i64 = sqlx::query_scalar("PRAGMA page_size;")
                    .fetch_one(&pool)
                    .await
                    .unwrap_or(4096);
                let enc: String = sqlx::query_scalar("PRAGMA encoding;")
                    .fetch_one(&pool)
                    .await
                    .unwrap_or_else(|_| "UTF-8".into());
                params.insert(format!("{fname}.journal_mode"), jm);
                params.insert(format!("{fname}.page_size"), ps.to_string());
                params.insert(format!("{fname}.encoding"), enc);
                pool.close().await;

                let (already_registered, existing_connection_id) =
                    find_registered(registered, EngineKind::Sqlite, &fname);
                databases.push(DiscoveredDatabase {
                    name: fname.clone(),
                    size_label: format_bytes(size_bytes),
                    object_count: tbl_cnt,
                    engine: EngineKind::Sqlite,
                    connection_target: fname,
                    already_registered,
                    existing_connection_id,
                });
            }
        }
    }

    DiscoveredEngine {
        engine: EngineKind::Sqlite,
        engine_label: "SQLite (Embedded & File DB)".to_string(),
        status: EngineStatus::Running,
        version,
        binary_path: bin,
        endpoint: Some(state.data_dir.display().to_string()),
        latency_ms: Some(1),
        parameters: params,
        databases,
        can_provision_local: false,
    }
}

async fn inspect_mongodb(
    state: &AppState,
    registered: &HashMap<(String, String), String>,
) -> DiscoveredEngine {
    let bin = find_binary(&["/usr/bin/mongod", "/usr/bin/mongosh"]).await;
    let tcp_lat = tcp_probe("127.0.0.1:27017").await;
    let mut params: BTreeMap<String, String> = BTreeMap::new();
    params.insert("default_port".into(), "27017".into());

    let mut databases = Vec::new();
    let local_doc = state.data_dir.join("telemetry_docs.sqlite");
    if local_doc.exists() {
        let size = std::fs::metadata(&local_doc).map(|m| m.len()).unwrap_or(0);
        if let Ok(pool) = catalog::open_sqlite_pool(&local_doc).await {
            let cols: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _guidb_collections")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            let docs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _guidb_documents")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            pool.close().await;
            params.insert("embedded_store".into(), "telemetry_docs.sqlite".into());
            params.insert("embedded_documents".into(), docs.to_string());

            let (already_registered, existing_connection_id) =
                find_registered(registered, EngineKind::MongoDb, "telemetry_docs.sqlite");
            databases.push(DiscoveredDatabase {
                name: "telemetry_docs (Embedded Mongo)".to_string(),
                size_label: format_bytes(size),
                object_count: cols,
                engine: EngineKind::MongoDb,
                connection_target: "telemetry_docs.sqlite".to_string(),
                already_registered,
                existing_connection_id,
            });
        }
    }

    if let Some(lat) = tcp_lat {
        params.insert("tcp_endpoint".into(), "127.0.0.1:27017".into());
        let target = "mongodb://127.0.0.1:27017".to_string();
        let (already_registered, existing_connection_id) =
            find_registered(registered, EngineKind::MongoDb, &target);
        databases.push(DiscoveredDatabase {
            name: "MongoDB TCP (127.0.0.1:27017)".to_string(),
            size_label: format!("{lat} ms"),
            object_count: 0,
            engine: EngineKind::MongoDb,
            connection_target: target,
            already_registered,
            existing_connection_id,
        });
    }

    let status = if tcp_lat.is_some() || !databases.is_empty() {
        EngineStatus::Running
    } else if bin.is_some() {
        EngineStatus::InstalledStopped
    } else {
        EngineStatus::NotInstalled
    };

    DiscoveredEngine {
        engine: EngineKind::MongoDb,
        engine_label: "MongoDB (Document / BSON)".to_string(),
        status,
        version: if bin.is_some() {
            "MongoDB CLI + Embedded Document Engine".to_string()
        } else {
            "Embedded Document Engine (mongod tidak terpasang di host)".to_string()
        },
        binary_path: bin,
        endpoint: Some(if tcp_lat.is_some() {
            "127.0.0.1:27017".into()
        } else {
            "Embedded Store".into()
        }),
        latency_ms: tcp_lat.or(Some(1)),
        parameters: params,
        databases,
        can_provision_local: false,
    }
}

async fn inspect_redis(
    state: &AppState,
    registered: &HashMap<(String, String), String>,
) -> DiscoveredEngine {
    let bin = find_binary(&[
        "/usr/bin/redis-server",
        "/usr/bin/redis-cli",
        "/usr/bin/valkey-server",
    ])
    .await;
    let tcp_lat = tcp_probe("127.0.0.1:6379").await;
    let mut params: BTreeMap<String, String> = BTreeMap::new();
    params.insert("default_port".into(), "6379".into());

    let mut databases = Vec::new();
    let local_kv = state.data_dir.join("edge_kv.sqlite");
    if local_kv.exists() {
        let size = std::fs::metadata(&local_kv).map(|m| m.len()).unwrap_or(0);
        if let Ok(pool) = catalog::open_sqlite_pool(&local_kv).await {
            let keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _guidb_kv")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            let bytes: i64 =
                sqlx::query_scalar("SELECT COALESCE(SUM(byte_size), 0) FROM _guidb_kv")
                    .fetch_one(&pool)
                    .await
                    .unwrap_or(0);
            pool.close().await;
            params.insert("embedded_store".into(), "edge_kv.sqlite".into());
            params.insert("embedded_payload_bytes".into(), bytes.to_string());

            let (already_registered, existing_connection_id) =
                find_registered(registered, EngineKind::Redis, "edge_kv.sqlite");
            databases.push(DiscoveredDatabase {
                name: "edge_kv (Embedded KV)".to_string(),
                size_label: format_bytes(size),
                object_count: keys,
                engine: EngineKind::Redis,
                connection_target: "edge_kv.sqlite".to_string(),
                already_registered,
                existing_connection_id,
            });
        }
    }

    if let Some(lat) = tcp_lat {
        if let Ok(info) = query_redis_info("127.0.0.1:6379").await {
            for (k, v) in info {
                params.insert(k, v);
            }
        }
        let target = "redis://127.0.0.1:6379".to_string();
        let (already_registered, existing_connection_id) =
            find_registered(registered, EngineKind::Redis, &target);
        databases.push(DiscoveredDatabase {
            name: "Redis Live Server (127.0.0.1:6379)".to_string(),
            size_label: format!("{lat} ms"),
            object_count: 0,
            engine: EngineKind::Redis,
            connection_target: target,
            already_registered,
            existing_connection_id,
        });
    }

    let status = if tcp_lat.is_some() || !databases.is_empty() {
        EngineStatus::Running
    } else if bin.is_some() {
        EngineStatus::InstalledStopped
    } else {
        EngineStatus::NotInstalled
    };

    DiscoveredEngine {
        engine: EngineKind::Redis,
        engine_label: "Redis / Valkey (Key-Value)".to_string(),
        status,
        version: if bin.is_some() {
            "Redis/Valkey Binary + Embedded KV Store".to_string()
        } else {
            "Embedded KV Store (redis-server tidak terpasang di host)".to_string()
        },
        binary_path: bin,
        endpoint: Some(if tcp_lat.is_some() {
            "127.0.0.1:6379".into()
        } else {
            "Embedded Store".into()
        }),
        latency_ms: tcp_lat.or(Some(1)),
        parameters: params,
        databases,
        can_provision_local: false,
    }
}

async fn query_redis_info(addr: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(b"*1\r\n$4\r\nINFO\r\n").await?;
    let mut reader = BufReader::new(stream);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).await?;
    let mut map = BTreeMap::new();
    if first_line.starts_with('$') {
        let mut line = String::new();
        for _ in 0..80 {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break;
            }
            let t = line.trim();
            if let Some((k, v)) = t.split_once(':') {
                if matches!(
                    k,
                    "redis_version"
                        | "tcp_port"
                        | "used_memory_human"
                        | "connected_clients"
                        | "role"
                ) {
                    map.insert(format!("live.{k}"), v.to_string());
                }
            }
        }
    }
    Ok(map)
}

pub async fn provision_local_engine(
    state: &AppState,
    engine: EngineKind,
) -> AppResult<ProvisionEngineResponse> {
    match engine {
        EngineKind::Postgres => provision_local_postgres(state).await,
        EngineKind::Mysql => provision_local_mariadb(state).await,
        _ => Err(AppError::BadRequest(
            "Local daemon provisioning applies to PostgreSQL or MySQL/MariaDB".into(),
        )),
    }
}

async fn provision_local_postgres(state: &AppState) -> AppResult<ProvisionEngineResponse> {
    if !Path::new("/usr/bin/initdb").exists() || !Path::new("/usr/bin/pg_ctl").exists() {
        return Err(AppError::BadRequest(
            "PostgreSQL server binaries (/usr/bin/initdb, /usr/bin/pg_ctl) not installed".into(),
        ));
    }

    let pg_dir = state.data_dir.join("local_postgres");
    let sock_dir = state.data_dir.join("pg_sockets");
    std::fs::create_dir_all(&sock_dir).map_err(|e| AppError::Database(e.to_string()))?;

    let chosen_port: u16 = if tcp_probe("127.0.0.1:55432").await.is_some() && !pg_dir.join("PG_VERSION").exists() {
        55433
    } else {
        55432
    };

    if !pg_dir.join("PG_VERSION").exists() {
        let out = Command::new("/usr/bin/initdb")
            .arg("-D")
            .arg(&pg_dir)
            .arg("-A")
            .arg("trust")
            .arg("--encoding=UTF8")
            .output()
            .await
            .map_err(|e| AppError::Database(format!("initdb failed: {e}")))?;
        if !out.status.success() {
            return Err(AppError::Database(format!(
                "initdb error: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let conf = pg_dir.join("postgresql.conf");
        let extra = format!(
            "\nlisten_addresses = '127.0.0.1'\nport = {chosen_port}\nunix_socket_directories = '{}'\n",
            sock_dir.display()
        );
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&conf)
            .map_err(|e| AppError::Database(e.to_string()))?;
        f.write_all(extra.as_bytes())
            .map_err(|e| AppError::Database(e.to_string()))?;
    }

    let addr = format!("127.0.0.1:{chosen_port}");
    if tcp_probe(&addr).await.is_none() {
        let log_file = state.data_dir.join("local_postgres.log");
        let out = Command::new("/usr/bin/pg_ctl")
            .arg("-D")
            .arg(&pg_dir)
            .arg("-l")
            .arg(&log_file)
            .arg("-w")
            .arg("start")
            .output()
            .await
            .map_err(|e| AppError::Database(format!("pg_ctl start failed: {e}")))?;
        if !out.status.success() {
            return Err(AppError::Database(format!(
                "pg_ctl start error: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
    }

    let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_string());
    let admin_dsn = format!("postgres://{user}@{addr}/postgres");
    let admin_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&admin_dsn)
        .await
        .map_err(|e| AppError::Database(format!("PostgreSQL admin connect failed: {e}")))?;

    let db_exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pg_database WHERE datname = 'guidb_pg'")
            .fetch_one(&admin_pool)
            .await
            .unwrap_or(0);
    if db_exists == 0 {
        sqlx::raw_sql("CREATE DATABASE guidb_pg;")
            .execute(&admin_pool)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
    }
    admin_pool.close().await;

    let target_dsn = format!("postgres://{user}@{addr}/guidb_pg");
    let db_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&target_dsn)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    sqlx::raw_sql(
        r#"
        CREATE TABLE IF NOT EXISTS server_nodes (
            id SERIAL PRIMARY KEY,
            node_name TEXT NOT NULL UNIQUE,
            region TEXT NOT NULL,
            cpu_cores INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'ONLINE'
        );
        INSERT INTO server_nodes (node_name, region, cpu_cores, status) VALUES
            ('pg-node-jkt-01', 'ap-southeast-3', 16, 'ONLINE'),
            ('pg-node-sby-02', 'ap-southeast-3', 8, 'ONLINE')
        ON CONFLICT (node_name) DO NOTHING;
        "#,
    )
    .execute(&db_pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    db_pool.close().await;

    let connection = ensure_or_create_connection(
        state,
        "Local PostgreSQL 18 (guidb_pg)",
        EngineKind::Postgres,
        EnvironmentKind::Local,
        &target_dsn,
    )
    .await?;

    let discovery = scan_server_databases(state).await?;
    Ok(ProvisionEngineResponse {
        connection,
        message: format!(
            "PostgreSQL 18 lokal berhasil dinyalakan di {addr} (database: guidb_pg)."
        ),
        discovery,
    })
}

async fn provision_local_mariadb(state: &AppState) -> AppResult<ProvisionEngineResponse> {
    if !Path::new("/usr/bin/mariadbd").exists()
        || !Path::new("/usr/bin/mariadb-install-db").exists()
    {
        return Err(AppError::BadRequest(
            "MariaDB binaries (/usr/bin/mariadbd, /usr/bin/mariadb-install-db) not installed"
                .into(),
        ));
    }

    let db_dir = state.data_dir.join("local_mariadb");
    let data_subdir = db_dir.join("data");
    let sock_path = db_dir.join("mariadb.sock");
    let pid_path = db_dir.join("mariadb.pid");
    let log_path = db_dir.join("mariadb.log");
    let cnf_path = db_dir.join("my.cnf");
    std::fs::create_dir_all(&data_subdir).map_err(|e| AppError::Database(e.to_string()))?;

    if !data_subdir.join("mysql").exists() {
        let user = std::env::var("USER").unwrap_or_else(|_| "server1".to_string());
        let out = Command::new("/usr/bin/mariadb-install-db")
            .arg(format!("--datadir={}", data_subdir.display()))
            .arg(format!("--user={user}"))
            .arg("--auth-root-authentication-method=normal")
            .output()
            .await
            .map_err(|e| AppError::Database(format!("mariadb-install-db failed: {e}")))?;
        if !out.status.success() {
            return Err(AppError::Database(format!(
                "mariadb-install-db error: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
    }

    let cnf_content = format!(
        "[mariadbd]\ndatadir={}\nsocket={}\npid-file={}\nlog-error={}\nport=33307\nbind-address=127.0.0.1\n",
        data_subdir.display(),
        sock_path.display(),
        pid_path.display(),
        log_path.display()
    );
    std::fs::write(&cnf_path, cnf_content).map_err(|e| AppError::Database(e.to_string()))?;

    if tcp_probe("127.0.0.1:33307").await.is_none() {
        let mut child = Command::new("/usr/bin/mariadbd")
            .arg(format!("--defaults-file={}", cnf_path.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| AppError::Database(format!("Failed spawning mariadbd: {e}")))?;

        let mut ready = false;
        for _ in 0..40 {
            if tcp_probe("127.0.0.1:33307").await.is_some() {
                ready = true;
                break;
            }
            if let Ok(Some(status)) = child.try_wait() {
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                return Err(AppError::Database(format!(
                    "mariadbd exited ({status}): {log}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if !ready {
            return Err(AppError::Database(
                "Timed out waiting for local MariaDB on 127.0.0.1:33307".into(),
            ));
        }
    }

    let admin_dsn = "mysql://root@127.0.0.1:33307/mysql";
    let admin_pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(admin_dsn)
        .await
        .map_err(|e| AppError::Database(format!("MariaDB admin connect failed: {e}")))?;

    sqlx::raw_sql("CREATE DATABASE IF NOT EXISTS guidb_mariadb;")
        .execute(&admin_pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    admin_pool.close().await;

    let target_dsn = "mysql://root@127.0.0.1:33307/guidb_mariadb";
    let db_pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(target_dsn)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    sqlx::raw_sql(
        r#"
        CREATE TABLE IF NOT EXISTS service_endpoints (
            id INT AUTO_INCREMENT PRIMARY KEY,
            service_name VARCHAR(120) NOT NULL UNIQUE,
            protocol VARCHAR(32) NOT NULL,
            port INT NOT NULL,
            healthy TINYINT(1) NOT NULL DEFAULT 1
        );
        INSERT IGNORE INTO service_endpoints (service_name, protocol, port, healthy) VALUES
            ('billing-core', 'HTTPS', 8443, 1),
            ('warehouse-sync', 'gRPC', 9090, 1);
        "#,
    )
    .execute(&db_pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    db_pool.close().await;

    let connection = ensure_or_create_connection(
        state,
        "Local MariaDB 12.3 (guidb_mariadb)",
        EngineKind::Mysql,
        EnvironmentKind::Local,
        target_dsn,
    )
    .await?;

    let discovery = scan_server_databases(state).await?;
    Ok(ProvisionEngineResponse {
        connection,
        message: "MariaDB 12.3 lokal berhasil dinyalakan di 127.0.0.1:33307 (database: guidb_mariadb).".into(),
        discovery,
    })
}

async fn ensure_or_create_connection(
    state: &AppState,
    name: &str,
    engine: EngineKind,
    environment: EnvironmentKind,
    target: &str,
) -> AppResult<ConnectionProfile> {
    let existing = catalog::list_connections(&state.catalog_pool).await?;
    if let Some(found) = existing
        .into_iter()
        .find(|c| c.engine == engine && c.target == target)
    {
        return Ok(found);
    }
    catalog::create_connection(
        state,
        CreateConnectionRequest {
            name: name.to_string(),
            engine,
            environment,
            target: target.to_string(),
            protection_mode: None,
        },
    )
    .await
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
