use crate::api::{load_workspace_for, WorkspaceQuery};
use crate::catalog;
use crate::error::AppResult;
use crate::state::AppState;
use axum::extract::State;
use axum::response::Html;
use guidb_shared::BootstrapResponse;
use leptos::prelude::*;

#[component]
fn StudioIcon(kind: &'static str) -> impl IntoView {
    let path = match kind {
        "database" => "M12 3C7.58 3 4 4.34 4 6v12c0 1.66 3.58 3 8 3s8-1.34 8-3V6c0-1.66-3.58-3-8-3zm0 2c3.87 0 6 .96 6 1s-2.13 1-6 1-6-.96-6-1 2.13-1 6-1zm0 8c-3.87 0-6-.96-6-1v-2.2c1.45.75 3.63 1.2 6 1.2s4.55-.45 6-1.2V12c0 .04-2.13 1-6 1zm0 6c-3.87 0-6-.96-6-1v-2.2c1.45.75 3.63 1.2 6 1.2s4.55-.45 6-1.2V18c0 .04-2.13 1-6 1z",
        "play" => "M8 5v14l11-7z",
        "plus" => "M11 5h2v6h6v2h-6v6h-2v-6H5v-2h6z",
        "code" => "M8.7 16.3L4.4 12l4.3-4.3 1.4 1.4L7.2 12l2.9 2.9-1.4 1.4zm6.6 0l-1.4-1.4 2.9-2.9-2.9-2.9 1.4-1.4L19.6 12l-4.3 4.3z",
        "save" => "M17 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V7l-4-4zm-5 16a3 3 0 1 1 0-6 3 3 0 0 1 0 6zm3-10H5V5h10v4z",
        "sync" => "M12 4V1L8 5l4 4V6c3.31 0 6 2.69 6 6 0 1.01-.25 1.97-.7 2.8l1.46 1.46A7.93 7.93 0 0 0 20 12c0-4.42-3.58-8-8-8zm0 14c-3.31 0-6-2.69-6-6 0-1.01.25-1.97.7-2.8L5.24 7.74A7.93 7.93 0 0 0 4 12c0 4.42 3.58 8 8 8v3l4-4-4-4v3z",
        "search" => "M15.5 14h-.79l-.28-.27A6.47 6.47 0 0 0 16 9.5 6.5 6.5 0 1 0 9.5 16c1.61 0 3.09-.59 4.23-1.57l.27.28v.79l5 4.99L20.49 19l-4.99-5zm-6 0C7.01 14 5 11.99 5 9.5S7.01 5 9.5 5 14 7.01 14 9.5 11.99 14 9.5 14z",
        "scan" => "M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm0 18c-4.42 0-8-3.58-8-8s3.58-8 8-8 8 3.58 8 8-3.58 8-8 8zm0-13a5 5 0 1 0 5 5 5 5 0 0 0-5-5zm0 8a3 3 0 1 1 3-3 3 3 0 0 1-3 3z",
        "theme" => "M12 3a9 9 0 1 0 9 9c0-.46-.04-.92-.1-1.36a5.389 5.389 0 0 1-4.4 2.26 5.403 5.403 0 0 1-3.14-9.8c-.44-.06-.9-.1-1.36-.1z",
        "menu" => "M3 6h18v2H3V6zm0 5h18v2H3v-2zm0 5h18v2H3v-2z",
        _ => "M4 4h16v16H4z",
    };

    view! {
        <svg class="ui-icon" viewBox="0 0 24 24" width="16" height="16" fill="currentColor" aria-hidden="true">
            <path d=path />
        </svg>
    }
}

#[component]
fn StudioShell(initial_json: String, active_conn_name: String, active_item_name: String) -> impl IntoView {
    view! {
        <html lang="id">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1.0" />
                <title>"GuiDB Studio - Universal Multi-Model Database Client"</title>
                <link rel="stylesheet" href="/static/app.css" />
            </head>
            <body>
                <a class="skip-link" href="#main-workspace">"Lompat ke Area Kerja Utama"</a>

                <header class="studio-header">
                    <div class="header-left">
                        <button
                            type="button"
                            id="btn-toggle-sidebar"
                            class="btn btn-secondary mobile-only-btn"
                            aria-expanded="false"
                            aria-controls="object-sidebar"
                        >
                            <StudioIcon kind="menu" />
                            <span>"Objek DB"</span>
                        </button>

                        <div class="brand-lockup">
                            <span class="brand-mark" aria-hidden="true">
                                <StudioIcon kind="database" />
                            </span>
                            <div class="brand-titles">
                                <h1 class="brand-h1">"GuiDB Studio"</h1>
                                <span id="active-engine-badge" class="env-pill env-local">"SQLite / Multi-Model"</span>
                            </div>
                        </div>

                        <div class="conn-picker-wrap">
                            <label for="connection-select" class="sr-only">"Koneksi Aktif"</label>
                            <select id="connection-select" class="select-control" aria-label="Pilih koneksi database">
                                <option value="">{active_conn_name}</option>
                            </select>
                        </div>

                        <div class="protection-picker-wrap">
                            <label for="protection-mode-select" class="sr-only">"Mode Proteksi Database"</label>
                            <select id="protection-mode-select" class="select-control select-protection" aria-label="Mode proteksi koneksi aktif">
                                <option value="safe_guard">"SAFE GUARD (Anti-Hapus)"</option>
                                <option value="read_only">"READ-ONLY (Terkunci)"</option>
                                <option value="unrestricted">"UNRESTRICTED (Buka Kunci)"</option>
                            </select>
                        </div>
                    </div>

                    <nav class="header-actions" aria-label="Perintah Studio Utama">
                        <button type="button" id="btn-discover-server" class="btn btn-primary" title="Pindai semua engine dan database yang terpasang di server ini">
                            <StudioIcon kind="scan" />
                            <span>"Deteksi DB"</span>
                        </button>
                        <button type="button" id="btn-quick-backup" class="btn btn-secondary" title="Backup koneksi aktif atau atur jadwal otomatis">
                            <StudioIcon kind="save" />
                            <span>"Backup DB"</span>
                        </button>
                        <button type="button" id="btn-command-palette" class="btn btn-secondary" title="Command Palette (Ctrl+K)">
                            <StudioIcon kind="search" />
                            <span>"Cari Objek (Ctrl+K)"</span>
                        </button>
                        <button type="button" id="btn-new-connection" class="btn btn-secondary">
                            <StudioIcon kind="plus" />
                            <span>"+ Koneksi"</span>
                        </button>
                        <button type="button" id="btn-new-table" class="btn btn-secondary">
                            <StudioIcon kind="plus" />
                            <span>"+ Tabel / Koleksi"</span>
                        </button>
                        <button type="button" id="btn-theme-toggle" class="btn btn-secondary" aria-pressed="false">
                            <StudioIcon kind="theme" />
                            <span id="theme-toggle-label">"Mode Gelap"</span>
                        </button>
                    </nav>
                </header>

                <div class="studio-shell">
                    <aside id="object-sidebar" class="sidebar" aria-label="Penjelajah Objek Database">
                        <div class="sidebar-head">
                            <h2 class="panel-heading">"Penjelajah Objek"</h2>
                            <button type="button" id="btn-refresh-workspace" class="btn btn-sm btn-ghost">
                                "Segarkan"
                            </button>
                        </div>

                        <div class="sidebar-filter">
                            <label for="sidebar-search" class="sr-only">"Filter objek tabel atau koleksi"</label>
                            <input
                                type="search"
                                id="sidebar-search"
                                class="input-control"
                                placeholder="Saring tabel / koleksi..."
                                autocomplete="off"
                            />
                        </div>

                        <div class="sidebar-section">
                            <h3 class="sidebar-subhead">"Koneksi Tersimpan"</h3>
                            <ul id="connection-list" class="tree-list"></ul>
                        </div>

                        <div class="sidebar-section sidebar-objects-grow">
                            <div class="sidebar-subhead-row">
                                <h3 class="sidebar-subhead" id="objects-heading">"Tabel & Koleksi"</h3>
                                <span id="objects-count" class="count-badge">"0"</span>
                            </div>
                            <ul id="object-tree" class="tree-list">
                                <li class="tree-item-placeholder">{active_item_name.clone()}</li>
                            </ul>
                        </div>
                    </aside>

                    <main id="main-workspace" class="workspace-main">
                        <section class="query-pane" aria-label="Editor Query dan Visual Builder">
                            <div class="query-toolbar">
                                <div class="query-toolbar-left">
                                    <h2 class="panel-heading" id="editor-mode-label">"SQL & Command Console"</h2>
                                    <div class="saved-query-wrap">
                                        <label for="saved-query-select" class="sr-only">"Query Tersimpan"</label>
                                        <select id="saved-query-select" class="select-control select-compact">
                                            <option value="">"Pilih Query Tersimpan..."</option>
                                        </select>
                                    </div>
                                </div>

                                <div class="query-toolbar-actions">
                                    <button type="button" id="btn-toggle-builder" class="btn btn-secondary" aria-expanded="false">
                                        "Visual Builder"
                                    </button>
                                    <button type="button" id="btn-format-sql" class="btn btn-secondary" title="Format SQL/JSON via Rust WASM">
                                        <StudioIcon kind="code" />
                                        <span>"Format (WASM)"</span>
                                    </button>
                                    <button type="button" id="btn-explain-query" class="btn btn-secondary">
                                        "Explain Plan"
                                    </button>
                                    <button type="button" id="btn-save-query" class="btn btn-secondary">
                                        "Simpan Query"
                                    </button>
                                    <button type="button" id="btn-run-query" class="btn btn-primary" title="Jalankan Query (Ctrl+Enter)">
                                        <StudioIcon kind="play" />
                                        <span>"Jalankan"</span>
                                    </button>
                                </div>
                            </div>

                            <div id="visual-builder-drawer" class="builder-panel" hidden>
                                <div class="builder-grid">
                                    <div class="field-group">
                                        <label for="qb-table">"Tabel Utama"</label>
                                        <select id="qb-table" class="select-control"></select>
                                    </div>
                                    <div class="field-group">
                                        <label for="qb-join-table">"JOIN Tabel (Opsional)"</label>
                                        <select id="qb-join-table" class="select-control">
                                            <option value="">"Tanpa JOIN"</option>
                                        </select>
                                    </div>
                                    <div class="field-group">
                                        <label for="qb-where">"Kondisi WHERE"</label>
                                        <input type="text" id="qb-where" class="input-control" placeholder="status = 'PAID'" />
                                    </div>
                                    <div class="field-group">
                                        <label for="qb-order">"ORDER BY"</label>
                                        <input type="text" id="qb-order" class="input-control" placeholder="id DESC" />
                                    </div>
                                    <div class="field-group builder-actions">
                                        <button type="button" id="btn-apply-builder" class="btn btn-primary">"Terapkan & Jalankan"</button>
                                    </div>
                                </div>
                            </div>

                            <div class="editor-box">
                                <label for="sql-editor" class="sr-only">"Editor Query SQL, JSON MongoDB, atau Perintah Redis"</label>
                                <textarea
                                    id="sql-editor"
                                    class="sql-textarea"
                                    rows="4"
                                    spellcheck="false"
                                    placeholder="SELECT * FROM customers LIMIT 50;"
                                ></textarea>
                            </div>
                        </section>

                        <div id="staged-bar" class="staged-bar" role="region" aria-label="Perubahan Tertunda" hidden>
                            <div class="staged-bar-info">
                                <span id="staged-summary" class="staged-badge">"0 perubahan tertunda"</span>
                                <span class="staged-hint">"Edit sel seperti spreadsheet, pratinjau SQL via WASM, lalu Simpan."</span>
                            </div>
                            <div class="staged-bar-actions">
                                <button type="button" id="btn-preview-staged" class="btn btn-secondary">"Pratinjau SQL/Perintah"</button>
                                <button type="button" id="btn-discard-staged" class="btn btn-secondary">"Batalkan"</button>
                                <button type="button" id="btn-commit-staged" class="btn btn-primary">
                                    <StudioIcon kind="save" />
                                    <span>"Simpan Perubahan (Ctrl+S)"</span>
                                </button>
                            </div>
                        </div>

                        <div id="status-banner" class="status-banner" role="status" aria-live="polite" hidden></div>

                        <section class="results-pane" aria-label="Area Kerja Hasil dan Desain">
                            <div class="workspace-tabs" role="tablist" aria-label="Mode Tampilan Database">
                                <button type="button" role="tab" id="tab-btn-data" class="tab-btn is-active" aria-selected="true" aria-controls="panel-data">
                                    "Data Grid"
                                </button>
                                <button type="button" role="tab" id="tab-btn-structure" class="tab-btn" aria-selected="false" aria-controls="panel-structure">
                                    "Struktur & DDL"
                                </button>
                                <button type="button" role="tab" id="tab-btn-erd" class="tab-btn" aria-selected="false" aria-controls="panel-erd">
                                    "Diagram ERD"
                                </button>
                                <button type="button" role="tab" id="tab-btn-sync" class="tab-btn" aria-selected="false" aria-controls="panel-sync">
                                    <StudioIcon kind="sync" />
                                    <span>"Schema Sync"</span>
                                </button>
                                <button type="button" role="tab" id="tab-btn-backup" class="tab-btn" aria-selected="false" aria-controls="panel-backup">
                                    <StudioIcon kind="save" />
                                    <span>"Backup & Jadwal"</span>
                                </button>
                                <button type="button" role="tab" id="tab-btn-history" class="tab-btn" aria-selected="false" aria-controls="panel-history">
                                    "Riwayat Query"
                                </button>
                            </div>

                            <div id="panel-data" role="tabpanel" aria-labelledby="tab-btn-data" class="tab-panel">
                                <div class="data-controls">
                                    <div class="data-controls-left">
                                        <span id="active-item-title" class="item-title-pill">{active_item_name.clone()}</span>
                                        <label for="quick-filter-input" class="sr-only">"Saring baris cepat"</label>
                                        <input
                                            type="search"
                                            id="quick-filter-input"
                                            class="input-control input-compact"
                                            placeholder="Saring cepat (nilai / JSON / key)..."
                                        />
                                        <button type="button" id="btn-apply-filter" class="btn btn-secondary btn-sm">"Saring"</button>
                                        <button type="button" id="btn-add-row" class="btn btn-secondary btn-sm">"+ Tambah Baris"</button>
                                    </div>
                                    <div class="data-controls-right">
                                        <span id="query-meta-text" class="meta-text">"Memuat data..."</span>
                                        <div class="export-group">
                                            <button type="button" id="btn-export-csv" class="btn btn-secondary btn-sm">"CSV"</button>
                                            <button type="button" id="btn-export-json" class="btn btn-secondary btn-sm">"JSON"</button>
                                            <button type="button" id="btn-export-sql" class="btn btn-secondary btn-sm">"SQL"</button>
                                        </div>
                                    </div>
                                </div>

                                <div id="data-state-box" class="state-box" hidden></div>

                                <div class="table-viewport" id="data-grid-viewport" tabindex="0">
                                    <table class="data-table" id="data-table">
                                        <thead id="data-thead"></thead>
                                        <tbody id="data-tbody"></tbody>
                                    </table>
                                </div>

                                <div id="document-cards-view" class="doc-cards-grid" hidden></div>
                            </div>

                            <div id="panel-structure" role="tabpanel" aria-labelledby="tab-btn-structure" class="tab-panel" hidden>
                                <div class="structure-toolbar">
                                    <h3 class="section-subtitle" id="structure-title">"Struktur Objek"</h3>
                                    <div class="structure-actions">
                                        <button type="button" id="btn-truncate-item" class="btn btn-danger-outline btn-sm">"Kosongkan Isi (Truncate)"</button>
                                        <button type="button" id="btn-drop-item" class="btn btn-danger-outline btn-sm">"Hapus Objek (Drop)"</button>
                                    </div>
                                </div>

                                <div class="structure-split">
                                    <div class="structure-card">
                                        <h4>"Kolom & Tipe Data"</h4>
                                        <div class="table-viewport">
                                            <table class="data-table">
                                                <thead>
                                                    <tr>
                                                        <th>"Nama"</th>
                                                        <th>"Tipe"</th>
                                                        <th>"Kunci"</th>
                                                        <th>"Nullable"</th>
                                                        <th>"Default"</th>
                                                    </tr>
                                                </thead>
                                                <tbody id="structure-cols-body"></tbody>
                                            </table>
                                        </div>

                                        <form id="form-add-column" class="inline-ddl-form">
                                            <h5>"+ Tambah Kolom (ALTER TABLE)"</h5>
                                            <div class="inline-form-row">
                                                <div class="field-group">
                                                    <label for="addcol-name">"Nama Kolom"</label>
                                                    <input type="text" id="addcol-name" class="input-control" required placeholder="notes" />
                                                </div>
                                                <div class="field-group">
                                                    <label for="addcol-type">"Tipe Data"</label>
                                                    <select id="addcol-type" class="select-control">
                                                        <option value="TEXT">"TEXT"</option>
                                                        <option value="INTEGER">"INTEGER"</option>
                                                        <option value="REAL">"REAL"</option>
                                                        <option value="BOOLEAN">"BOOLEAN"</option>
                                                        <option value="TIMESTAMP">"TIMESTAMP"</option>
                                                        <option value="JSON">"JSON"</option>
                                                    </select>
                                                </div>
                                                <div class="field-group">
                                                    <label for="addcol-default">"Nilai Default"</label>
                                                    <input type="text" id="addcol-default" class="input-control" placeholder="Opsional" />
                                                </div>
                                                <div class="field-group checkbox-group">
                                                    <label for="addcol-nullable">
                                                        <input type="checkbox" id="addcol-nullable" checked />
                                                        <span>"Boleh NULL"</span>
                                                    </label>
                                                </div>
                                                <button type="submit" class="btn btn-primary">"Tambah Kolom"</button>
                                            </div>
                                        </form>
                                    </div>

                                    <div class="structure-card">
                                        <h4>"Indeks & Foreign Key"</h4>
                                        <ul id="structure-indexes-list" class="meta-list"></ul>
                                        <ul id="structure-fks-list" class="meta-list"></ul>

                                        <form id="form-add-index" class="inline-ddl-form">
                                            <h5>"+ Buat Indeks"</h5>
                                            <div class="inline-form-row">
                                                <div class="field-group">
                                                    <label for="addidx-name">"Nama Indeks"</label>
                                                    <input type="text" id="addidx-name" class="input-control" required placeholder="idx_table_col" />
                                                </div>
                                                <div class="field-group">
                                                    <label for="addidx-col">"Kolom"</label>
                                                    <input type="text" id="addidx-col" class="input-control" required placeholder="email" />
                                                </div>
                                                <div class="field-group checkbox-group">
                                                    <label for="addidx-unique">
                                                        <input type="checkbox" id="addidx-unique" />
                                                        <span>"UNIQUE"</span>
                                                    </label>
                                                </div>
                                                <button type="submit" class="btn btn-secondary">"Buat Indeks"</button>
                                            </div>
                                        </form>

                                        <h4>"Definisi DDL Asli"</h4>
                                        <pre id="structure-ddl-code" class="code-block"></pre>
                                    </div>
                                </div>
                            </div>

                            <div id="panel-erd" role="tabpanel" aria-labelledby="tab-btn-erd" class="tab-panel" hidden>
                                <div class="structure-toolbar">
                                    <h3 class="section-subtitle">"Entity-Relationship Diagram (Visual SVG)"</h3>
                                    <span id="erd-stats" class="meta-text">"Relasi tabel & kunci tamu"</span>
                                </div>
                                <div id="erd-canvas-wrap" class="erd-canvas-wrap"></div>
                            </div>

                            <div id="panel-sync" role="tabpanel" aria-labelledby="tab-btn-sync" class="tab-panel" hidden>
                                <div class="sync-controls">
                                    <div class="field-group">
                                        <label for="sync-source-select">"Database Sumber (Referensi)"</label>
                                        <select id="sync-source-select" class="select-control"></select>
                                    </div>
                                    <div class="field-group">
                                        <label for="sync-target-select">"Database Target (Akan Disamakan)"</label>
                                        <select id="sync-target-select" class="select-control"></select>
                                    </div>
                                    <div class="sync-btn-group">
                                        <button type="button" id="btn-compare-schemas" class="btn btn-primary">"Bandingkan Skema"</button>
                                        <button type="button" id="btn-apply-sync" class="btn btn-secondary" disabled>"Terapkan Migrasi ke Target"</button>
                                    </div>
                                </div>
                                <div id="sync-results-box" class="sync-results-box">
                                    <p class="meta-text">"Pilih koneksi Sumber dan Target lalu klik Bandingkan Skema untuk melihat perbedaan tabel, kolom, dan indeks."</p>
                                </div>
                            </div>

                            <div id="panel-backup" role="tabpanel" aria-labelledby="tab-btn-backup" class="tab-panel" hidden>
                                <div class="safety-banner">
                                    <strong>"Proteksi Anti-Hapus Aktif:"</strong>
                                    <span>" DROP DATABASE, DROP SCHEMA, FLUSHALL, dan FLUSHDB diblokir permanen. Mode SAFE GUARD juga menahan DROP TABLE, TRUNCATE, dan DELETE tanpa WHERE serta membuat backup otomatis sebelum Schema Sync."</span>
                                </div>

                                <div class="structure-split">
                                    <div class="structure-card">
                                        <h4>"Backup Manual (Snapshot SHA-256)"</h4>
                                        <form id="form-create-backup" class="modal-form">
                                            <div class="field-group">
                                                <label for="bkp-conn-select">"Database Target"</label>
                                                <select id="bkp-conn-select" class="select-control"></select>
                                            </div>
                                            <div class="field-group">
                                                <label for="bkp-scope-select">"Cakupan Backup"</label>
                                                <select id="bkp-scope-select" class="select-control">
                                                    <option value="full_schema_and_data">"Skema + Seluruh Data (Full)"</option>
                                                    <option value="schema_only">"Struktur Skema Saja (DDL)"</option>
                                                </select>
                                            </div>
                                            <div class="field-group">
                                                <label for="bkp-label-input">"Label Backup (Opsional)"</label>
                                                <input type="text" id="bkp-label-input" class="input-control" placeholder="Sebelum rilis v1.2" />
                                            </div>
                                            <button type="submit" class="btn btn-primary">"Buat Backup Sekarang"</button>
                                        </form>
                                    </div>

                                    <div class="structure-card">
                                        <h4>"Jadwal Backup Otomatis"</h4>
                                        <form id="form-create-schedule" class="modal-form">
                                            <div class="field-group">
                                                <label for="sched-conn-select">"Database Terjadwal"</label>
                                                <select id="sched-conn-select" class="select-control"></select>
                                            </div>
                                            <div class="inline-form-row">
                                                <div class="field-group">
                                                    <label for="sched-interval-select">"Interval Otomatis"</label>
                                                    <select id="sched-interval-select" class="select-control">
                                                        <option value="5">"Setiap 5 Menit"</option>
                                                        <option value="60" selected>"Setiap 1 Jam"</option>
                                                        <option value="360">"Setiap 6 Jam"</option>
                                                        <option value="1440">"Harian (24 Jam)"</option>
                                                        <option value="10080">"Mingguan (7 Hari)"</option>
                                                    </select>
                                                </div>
                                                <div class="field-group">
                                                    <label for="sched-retention-input">"Simpan Maksimal (Retensi)"</label>
                                                    <input type="number" id="sched-retention-input" class="input-control" min="1" max="100" value="7" />
                                                </div>
                                            </div>
                                            <div class="field-group">
                                                <label for="sched-scope-select">"Cakupan Jadwal"</label>
                                                <select id="sched-scope-select" class="select-control">
                                                    <option value="full_schema_and_data">"Skema + Data (Full)"</option>
                                                    <option value="schema_only">"Skema Saja"</option>
                                                </select>
                                            </div>
                                            <button type="submit" class="btn btn-secondary">"Tambah Jadwal Otomatis"</button>
                                        </form>
                                    </div>
                                </div>

                                <div class="structure-toolbar" style="margin-top: 14px;">
                                    <h3 class="section-subtitle">"Jadwal Otomatis Aktif"</h3>
                                    <button type="button" id="btn-refresh-backups" class="btn btn-secondary btn-sm">"Segarkan Daftar"</button>
                                </div>
                                <div class="table-viewport" style="margin-bottom: 14px;">
                                    <table class="data-table">
                                        <thead>
                                            <tr>
                                                <th>"Database"</th>
                                                <th>"Interval"</th>
                                                <th>"Retensi"</th>
                                                <th>"Status"</th>
                                                <th>"Eksekusi Berikutnya"</th>
                                                <th>"Aksi"</th>
                                            </tr>
                                        </thead>
                                        <tbody id="schedule-list-tbody"></tbody>
                                    </table>
                                </div>

                                <div class="structure-toolbar">
                                    <h3 class="section-subtitle">"Arsip Backup Tersimpan (SHA-256 Verified)"</h3>
                                    <span id="backup-dir-label" class="meta-text">"./data/backups"</span>
                                </div>
                                <div class="table-viewport">
                                    <table class="data-table">
                                        <thead>
                                            <tr>
                                                <th>"Waktu"</th>
                                                <th>"Database"</th>
                                                <th>"Jenis"</th>
                                                <th>"Label & Berkas"</th>
                                                <th>"Ukuran / Baris"</th>
                                                <th>"SHA-256"</th>
                                                <th>"Aksi"</th>
                                            </tr>
                                        </thead>
                                        <tbody id="backup-list-tbody"></tbody>
                                    </table>
                                </div>
                            </div>

                            <div id="panel-history" role="tabpanel" aria-labelledby="tab-btn-history" class="tab-panel" hidden>
                                <div class="structure-toolbar">
                                    <h3 class="section-subtitle">"Log Eksekusi & Audit Query"</h3>
                                    <button type="button" id="btn-refresh-history" class="btn btn-secondary btn-sm">"Muat Ulang Log"</button>
                                </div>
                                <div class="table-viewport">
                                    <table class="data-table">
                                        <thead>
                                            <tr>
                                                <th>"Waktu"</th>
                                                <th>"Koneksi"</th>
                                                <th>"Perintah / SQL"</th>
                                                <th>"Status"</th>
                                                <th>"Baris"</th>
                                                <th>"Durasi"</th>
                                            </tr>
                                        </thead>
                                        <tbody id="history-tbody"></tbody>
                                    </table>
                                </div>
                            </div>
                        </section>
                    </main>
                </div>

                <footer class="studio-footer">
                    <span id="footer-status">"Siap. Full-Stack Rust (Axum + Leptos + WASM + SQLx)."</span>
                    <span id="footer-wasm-badge" class="meta-text">"WASM Engine: Memuat..."</span>
                </footer>

                <div id="conn-modal" class="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="conn-modal-title" hidden>
                    <div class="modal-card">
                        <div class="modal-head">
                            <h2 id="conn-modal-title">"Tambah Koneksi Database"</h2>
                            <button type="button" class="btn btn-sm btn-ghost modal-close" data-close="conn-modal" aria-label="Tutup dialog">"Tutup"</button>
                        </div>
                        <form id="form-new-connection" class="modal-form">
                            <div class="field-group">
                                <label for="conn-name">"Nama Koneksi"</label>
                                <input type="text" id="conn-name" class="input-control" required placeholder="Production Postgres / Local Warehouse" />
                            </div>
                            <div class="field-group">
                                <label for="conn-engine">"Tipe Database"</label>
                                <select id="conn-engine" class="select-control">
                                    <option value="sqlite">"SQLite (File Lokal)"</option>
                                    <option value="postgres">"PostgreSQL (postgres://)"</option>
                                    <option value="mysql">"MySQL / MariaDB (mysql://)"</option>
                                    <option value="mongo_db">"MongoDB (Document Store)"</option>
                                    <option value="redis">"Redis (Key-Value / redis://)"</option>
                                </select>
                            </div>
                            <div class="field-group">
                                <label for="conn-env">"Lingkungan (Environment)"</label>
                                <select id="conn-env" class="select-control">
                                    <option value="local">"LOCAL"</option>
                                    <option value="staging">"STAGING"</option>
                                    <option value="production">"PRODUCTION"</option>
                                </select>
                            </div>
                            <div class="field-group">
                                <label for="conn-target">"URI / Path Database"</label>
                                <input type="text" id="conn-target" class="input-control" required placeholder="analytics.sqlite atau postgres://user:pass@localhost:5432/db" />
                            </div>
                            <div id="conn-test-result" class="status-inline" hidden></div>
                            <div class="modal-actions">
                                <button type="button" id="btn-test-conn" class="btn btn-secondary">"Uji Koneksi"</button>
                                <button type="submit" class="btn btn-primary">"Simpan & Hubungkan"</button>
                            </div>
                        </form>
                    </div>
                </div>

                <div id="ddl-modal" class="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="ddl-modal-title" hidden>
                    <div class="modal-card">
                        <div class="modal-head">
                            <h2 id="ddl-modal-title">"Buat Tabel atau Koleksi Baru"</h2>
                            <button type="button" class="btn btn-sm btn-ghost modal-close" data-close="ddl-modal" aria-label="Tutup dialog">"Tutup"</button>
                        </div>
                        <form id="form-new-table" class="modal-form">
                            <div class="field-group">
                                <label for="new-table-name">"Nama Tabel / Koleksi"</label>
                                <input type="text" id="new-table-name" class="input-control" required placeholder="invoices" />
                            </div>
                            <div class="field-group">
                                <label for="new-table-cols">"Daftar Kolom (format: nama:TIPE:PK/NN per baris)"</label>
                                <textarea id="new-table-cols" class="sql-textarea" rows="5">"id:INTEGER:PK\ninvoice_no:TEXT:NN\namount:INTEGER:NN\ncreated_at:TEXT:NN"</textarea>
                            </div>
                            <div class="modal-actions">
                                <button type="button" class="btn btn-secondary modal-close" data-close="ddl-modal">"Batal"</button>
                                <button type="submit" class="btn btn-primary">"Buat Sekarang"</button>
                            </div>
                        </form>
                    </div>
                </div>

                <div id="preview-modal" class="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="preview-modal-title" hidden>
                    <div class="modal-card">
                        <div class="modal-head">
                            <h2 id="preview-modal-title">"Pratinjau Perintah Transaksional (Rust WASM)"</h2>
                            <button type="button" class="btn btn-sm btn-ghost modal-close" data-close="preview-modal" aria-label="Tutup dialog">"Tutup"</button>
                        </div>
                        <div class="modal-form">
                            <pre id="staged-preview-code" class="code-block"></pre>
                            <div class="modal-actions">
                                <button type="button" class="btn btn-secondary modal-close" data-close="preview-modal">"Tutup"</button>
                                <button type="button" id="btn-modal-commit" class="btn btn-primary">"Commit Sekarang"</button>
                            </div>
                        </div>
                    </div>
                </div>

                <div id="palette-modal" class="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="palette-modal-title" hidden>
                    <div class="modal-card">
                        <div class="modal-head">
                            <h2 id="palette-modal-title">"Command Palette (Ctrl+K)"</h2>
                            <button type="button" class="btn btn-sm btn-ghost modal-close" data-close="palette-modal" aria-label="Tutup dialog">"Tutup"</button>
                        </div>
                        <div class="modal-form">
                            <label for="palette-input" class="sr-only">"Ketik nama tabel, koleksi, atau koneksi"</label>
                            <input type="search" id="palette-input" class="input-control" placeholder="Ketik nama tabel, koleksi, atau koneksi..." />
                            <ul id="palette-results" class="palette-list"></ul>
                        </div>
                    </div>
                </div>

                <div id="discovery-modal" class="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="discovery-modal-title" hidden>
                    <div class="modal-card modal-wide">
                        <div class="modal-head">
                            <h2 id="discovery-modal-title">"Deteksi Database & Parameter Server"</h2>
                            <div class="discovery-head-actions">
                                <button type="button" id="btn-rescan-server" class="btn btn-secondary btn-sm">"Pindai Ulang"</button>
                                <button type="button" class="btn btn-sm btn-ghost modal-close" data-close="discovery-modal" aria-label="Tutup dialog">"Tutup"</button>
                            </div>
                        </div>
                        <div id="discovery-summary" class="discovery-summary">
                            <p class="meta-text">"Memindai engine database, port, konfigurasi, dan skema pada server..."</p>
                        </div>
                        <div id="discovery-results" class="discovery-grid"></div>
                    </div>
                </div>

                <script id="guidb-bootstrap-data" type="application/json" inner_html=initial_json></script>
                <script src="/static/app.js" defer></script>
            </body>
        </html>
    }
}

pub async fn index_page(State(state): State<AppState>) -> AppResult<Html<String>> {
    let connections = catalog::list_connections(&state.catalog_pool).await?;
    let first = connections
        .first()
        .cloned()
        .ok_or_else(|| crate::error::AppError::NotFound("No connections configured".into()))?;
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

    let bootstrap = BootstrapResponse {
        connections,
        workspace,
        history,
        saved_queries,
    };

    let active_conn_name = bootstrap.workspace.connection.name.clone();
    let active_item_name = bootstrap
        .workspace
        .active_item
        .clone()
        .unwrap_or_else(|| "customers".to_string());

    // Escape closing </script> sequences cleanly inside inline JSON tag
    let initial_json = serde_json::to_string(&bootstrap)
        .unwrap_or_else(|_| "{}".to_string())
        .replace("</", "<\\/");

    let rendered = view! {
        <StudioShell
            initial_json=initial_json
            active_conn_name=active_conn_name
            active_item_name=active_item_name
        />
    }
    .to_html();

    Ok(Html(format!("<!DOCTYPE html>{rendered}")))
}
