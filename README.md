# GuiDB Studio

Full-stack Rust database management studio (`Axum + Tokio + Leptos SSR + Pure-Rust WASM + SQLx`) built as a unified visual client across Relational SQL (`SQLite`, `PostgreSQL`, `MySQL / MariaDB`), Document (koleksi lokal dengan filter bergaya MongoDB), and Key-Value (`Redis`) workflows.

## Architecture

- [`crates/guidb-shared`](crates/guidb-shared): Shared domain models (`ProtectionMode`, `BackupRecord`, `BackupSchedule`, `ConnectionProfile`) and SQL/NoSQL generator + formatter helpers.
- [`crates/guidb-wasm`](crates/guidb-wasm): Pure-Rust WebAssembly engine (`static/guidb_wasm.wasm`) handling SQL formatting and staged mutation SQL preview generation in the browser.
- [`crates/guidb-server`](crates/guidb-server): Axum + Tokio HTTP API, Leptos SSR shell, SQLite persistent catalog (`catalog.rs`), multi-engine query/mutation execution, host database discovery (`discovery.rs`), layered safety guardrails (`safety.rs`), and backup scheduler engine (`backup.rs`).
- [`static`](static): Compiled `guidb_wasm.wasm`, CSS design tokens and layout (`app.css`), and interactive studio client (`app.js`).

## Key Capabilities

1. **Unified Multi-Model Browser**: Switch seamlessly between SQLite, PostgreSQL, MySQL/MariaDB tables, embedded SQLite-backed document collections (with aggregation pipeline support), and Redis key-value namespaces.
2. **Staged Cell Editing & WASM Preview**: Edit cells directly in the Data Grid, stage batch `INSERT` / `UPDATE` / `DELETE` changes, inspect generated SQL via Rust WASM, and commit atomically.
3. **Structure, DDL, SVG ERD, & Schema Sync**: Inspect table DDL, browse interactive SVG entity-relationship diagrams, compare schemas between source and target connections, and apply synchronized migrations.
4. **Deteksi DB (Host Server Auto-Discovery)**: Scan active host database engines, open TCP ports, configuration parameters, and existing schemas/databases without modifying live services (`GET /api/discovery/scan`).
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

## Penggunaan editor

Pilih koneksi dan tabel di penjelajah objek. Pada tab Data Grid, klik sel untuk mengedit, Enter untuk menyiapkan perubahan, atau Escape untuk membatalkan edit sel. Tombol **Set NULL** mengosongkan nilai menjadi SQL NULL. **Tambah baris** dan **Duplikat** menyiapkan baris baru. Duplikat mengosongkan primary key agar bisa diisi ulang atau dibuat oleh database. **Simpan Perubahan** menerapkan seluruh batch. Hapus baris dan hapus kolom memerlukan mode **Unrestricted**.

Tab **Struktur & DDL** menyediakan tambah, ubah nama, dan hapus kolom serta ubah nama tabel. Navigasi halaman, pengurutan, dan filter grid relasional membaca database. Hasil query ad hoc ditampilkan hanya-baca. Di layar kecil, buka **Alat studio** untuk tindakan global dan **Tampilkan editor** untuk membuka konsol.

Dari toolbar Data Grid, gunakan **Sisip tabel**, **Sisip baris**, atau **Sisip kolom**. Baris masuk ke daftar perubahan tertunda. Tabel dan kolom dibuat saat formulir dikirim.

**Salin tabel** memilih tabel sumber di GuiDB. **Tempel tabel** meminta nama tabel baru pada koneksi yang sama dan menyalin seluruh baris yang tersimpan saat ditempel, termasuk baris di luar filter atau halaman aktif. Salinan membawa nama kolom, tipe data, dan primary key. Untuk menyalin trigger, indeks tambahan, foreign key, serta aturan nilai default, gunakan mekanisme backup yang sesuai dengan engine. Nama tujuan yang sudah dipakai akan ditolak.

## Akses dan backup

Untuk akses melalui jaringan, set `GUIDB_ADMIN_PASSWORD` dan gunakan nama pengguna `admin`. Semua rute server memakai HTTP Basic saat variabel ini diisi. Gunakan HTTPS pada reverse proxy. Mode loopback tetap bisa dipakai tanpa password. Akses bersama saat ini menggunakan satu akun administrator.

Backup penuh SQLite menghasilkan snapshot `.sqlite` yang menyertakan indeks, trigger, dan data biner. Dump SQL lama tetap bisa dipulihkan. Backup dokumen lokal dan KV lokal memakai JSON. Untuk Redis jaringan gunakan RDB native. Pemulihan snapshot dilakukan ke file dan profil baru.

## Layanan lokal dengan systemd

Unit pengguna di `deploy/guidb.service` menjalankan biner rilis pada `127.0.0.1:8788`. Contoh ini memakai checkout di `~/guidb` dan katalog persisten di `~/guidb/data`. Jalankan dari akar checkout sebagai pengguna pemilik data.

```bash
set -e
cargo build --locked --release -p guidb-server
cargo build --locked --release -p guidb-wasm --target wasm32-unknown-unknown
install -d -m 700 ~/.config/guidb ~/.local/share/guidb/static
if [ ! -e ~/.config/guidb/admin.env ]; then
  (umask 077; set -C; password=$(openssl rand -hex 32) && printf 'GUIDB_ADMIN_PASSWORD=%s\n' "$password" > ~/.config/guidb/admin.env)
fi
install -s -m 700 target/release/guidb-server ~/.local/share/guidb/guidb-server.new
mv -f ~/.local/share/guidb/guidb-server{.new,}
install -m 600 static/app.{css,js} ~/.local/share/guidb/static/
install -m 600 target/wasm32-unknown-unknown/release/guidb_wasm.wasm ~/.local/share/guidb/static/
install -Dm644 deploy/guidb.service ~/.config/systemd/user/guidb.service
systemctl --user daemon-reload
systemctl --user enable --now guidb.service
systemctl --user restart guidb.service
systemctl --user is-active guidb.service
```

Gunakan akun `admin` dan sandi dari `~/.config/guidb/admin.env`. Untuk instalasi lama, batasi akses direktori data dan backup ke pemiliknya. Unit memakai umask 0077 untuk file baru.
