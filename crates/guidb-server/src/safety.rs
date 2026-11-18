use crate::error::{AppError, AppResult};
use guidb_shared::{DdlAction, EngineKind, ProtectionMode, StagedMutation};

/// Check whether a raw query / command is safe under global anti-destruction rules and connection ProtectionMode.
pub fn check_query_safety(
    engine: EngineKind,
    mode: ProtectionMode,
    query: &str,
) -> AppResult<()> {
    let normalized = strip_comments_and_collapse(query);
    if normalized.is_empty() {
        return Err(AppError::BadRequest("Query cannot be empty".into()));
    }
    let upper = normalized.to_ascii_uppercase();

    // Global hard block: GuiDB Studio must NEVER drop an entire database/schema or wipe a server cache.
    if upper.contains("DROP DATABASE")
        || upper.contains("DROP SCHEMA")
        || upper.contains("SHUTDOWN")
    {
        return Err(AppError::BadRequest(
            "DIBLOKIR (Anti-Hapus Global): Perintah DROP DATABASE / DROP SCHEMA / SHUTDOWN dilarang permanen di GuiDB Studio demi melindungi database server yang ada.".into(),
        ));
    }

    if engine == EngineKind::Redis {
        let first_cmd = upper.split_whitespace().next().unwrap_or("");
        if matches!(first_cmd, "FLUSHALL" | "FLUSHDB" | "SHUTDOWN") {
            return Err(AppError::BadRequest(format!(
                "DIBLOKIR (Anti-Hapus Global): Perintah Redis '{first_cmd}' dilarang permanen."
            )));
        }
        if mode == ProtectionMode::ReadOnly
            && !matches!(
                first_cmd,
                "PING" | "DBSIZE" | "KEYS" | "SCAN" | "GET" | "HGETALL" | "TTL" | "INFO" | "TYPE"
            )
        {
            return Err(AppError::BadRequest(format!(
                "DIBLOKIR (Mode READ-ONLY): Perintah tulis Redis '{first_cmd}' ditolak."
            )));
        }
        if mode == ProtectionMode::SafeGuard && first_cmd == "DEL" {
            return Err(AppError::BadRequest(
                "DIBLOKIR (Mode SAFE GUARD): Perintah hapus key 'DEL' ditahan. Ubah mode proteksi ke UNRESTRICTED jika memang ingin menghapus.".into(),
            ));
        }
        return Ok(());
    }

    if engine == EngineKind::MongoDb {
        if mode == ProtectionMode::ReadOnly && (upper.contains("DELETE") || upper.contains("DROP")) {
            return Err(AppError::BadRequest(
                "DIBLOKIR (Mode READ-ONLY): Operasi penghapusan dokumen tidak diizinkan.".into(),
            ));
        }
        return Ok(());
    }

    // Relational SQL checks
    let first_word = upper.split_whitespace().next().unwrap_or("");
    let is_read_only_stmt = matches!(
        first_word,
        "SELECT" | "WITH" | "EXPLAIN" | "PRAGMA" | "SHOW" | "DESCRIBE" | "DESC"
    );

    if mode == ProtectionMode::ReadOnly && !is_read_only_stmt {
        return Err(AppError::BadRequest(format!(
            "DIBLOKIR (Mode READ-ONLY): Koneksi terkunci hanya-baca; perintah '{first_word}' ditolak."
        )));
    }

    if mode == ProtectionMode::SafeGuard {
        if upper.contains("DROP TABLE")
            || upper.contains("DROP VIEW")
            || upper.contains("TRUNCATE")
        {
            return Err(AppError::BadRequest(
                "DIBLOKIR (Mode SAFE GUARD): DROP TABLE / DROP VIEW / TRUNCATE ditahan agar tabel tidak terhapus. Buat backup dahulu atau ganti mode ke UNRESTRICTED.".into(),
            ));
        }
        if upper.starts_with("DELETE") && !upper.contains(" WHERE ") {
            return Err(AppError::BadRequest(
                "DIBLOKIR (Mode SAFE GUARD): DELETE tanpa klausa WHERE ditolak untuk mencegah penghapusan seluruh isi tabel.".into(),
            ));
        }
        if upper.starts_with("UPDATE") && !upper.contains(" WHERE ") {
            return Err(AppError::BadRequest(
                "DIBLOKIR (Mode SAFE GUARD): UPDATE tanpa klausa WHERE ditolak agar tidak menimpa seluruh baris tabel.".into(),
            ));
        }
    }

    Ok(())
}

/// Check staged grid mutations against the connection ProtectionMode.
pub fn check_mutations_safety(
    mode: ProtectionMode,
    mutations: &[StagedMutation],
) -> AppResult<()> {
    if mode == ProtectionMode::ReadOnly {
        return Err(AppError::BadRequest(
            "DIBLOKIR (Mode READ-ONLY): Koneksi terkunci hanya-baca; mutasi baris tidak dapat disimpan.".into(),
        ));
    }
    if mode == ProtectionMode::SafeGuard
        && mutations
            .iter()
            .any(|m| matches!(m, StagedMutation::Delete { .. }))
    {
        return Err(AppError::BadRequest(
            "DIBLOKIR (Mode SAFE GUARD): Penghapusan baris ditahan. Ubah proteksi koneksi ke UNRESTRICTED untuk menghapus baris.".into(),
        ));
    }
    Ok(())
}

/// Check right-click / toolbar DDL actions against the connection ProtectionMode.
pub fn check_ddl_safety(mode: ProtectionMode, action: &DdlAction) -> AppResult<()> {
    if mode == ProtectionMode::ReadOnly {
        return Err(AppError::BadRequest(
            "DIBLOKIR (Mode READ-ONLY): Koneksi terkunci hanya-baca; perubahan skema DDL ditolak.".into(),
        ));
    }
    if mode == ProtectionMode::SafeGuard
        && matches!(
            action,
            DdlAction::TruncateItem { .. } | DdlAction::DropItem { .. }
        )
    {
        return Err(AppError::BadRequest(
            "DIBLOKIR (Mode SAFE GUARD): Operasi Truncate / Drop ditahan demi mencegah kehilangan data. Buat backup dan aktifkan mode UNRESTRICTED bila diperlukan.".into(),
        ));
    }
    Ok(())
}

fn strip_comments_and_collapse(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(trimmed);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
