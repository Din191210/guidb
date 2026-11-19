# Audit GuiDB Studio, 19 November 2026

GuiDB Studio adalah alat administrasi database berbasis web. Axum menangani API dan merender halaman awal lewat Leptos. JavaScript menjalankan antarmuka dan memanggil WASM untuk formatter serta pratinjau mutasi. SQLite menyimpan profil koneksi, histori, query tersimpan, dan metadata backup. Driver terpisah menangani PostgreSQL, MySQL/MariaDB, SQLite, penyimpanan dokumen lokal, dan key-value lokal atau Redis jaringan.

Audit memakai commit `8891c61` dan asumsi satu operator lokal dengan default `127.0.0.1:8787`. Risiko meningkat jika beberapa pengguna berbagi mesin, browser, atau akses melalui proxy. Pencarian nama laporan di workspace dan riwayat Git tidak menemukan laporan sebelumnya.

## Hasil pemeriksaan

| Area | Hasil |
| --- | --- |
| Rust | 6 tes integrasi HTTP dan 3 unit test lulus. Build server, Clippy workspace untuk semua target, dan pemeriksaan format lulus. |
| Browser | Playwright Chromium memvalidasi WASM dan membuka UI tanpa error JavaScript yang tercatat. Pada 375, 768, dan 1440 px, halaman awal tidak melebar di luar viewport. Tab keluar dari dialog koneksi dan Escape tidak memulihkan fokus ke tombol pembuka. |
| Aset | `node --check static/app.js` lulus. Aset WASM teridentifikasi sebagai WebAssembly v1. `bash -n scripts/verify_all.sh` lulus. |
| HTTP dan persistensi | Query baca dan mutasi grid berhasil pada server yang memakai database sementara. Baris baru dikonfirmasi langsung lewat SQLite. Uji tambahan mengonfirmasi TTL lokal dan pemotongan backup pada temuan 4 dan 8. |
| SQLite workspace | Kelima file database mengembalikan `integrity_check=ok` dan tanpa pelanggaran foreign key. Pemeriksaan baca-saja pada file utama. Kelima WAL berukuran 0 byte saat pemeriksaan. |
| Arsip yang tersedia | Seluruh 10 dump SQLite cocok dengan SHA-256 dan ukuran pada katalog. Setiap dump berhasil diimpor ke SQLite in-memory, menghasilkan 23 baris sesuai katalog, lolos pemeriksaan integritas, dan tanpa pelanggaran foreign key. Arsip yang tersedia semuanya untuk satu koneksi, Commerce Core. |
| Dependensi | `cargo audit --no-fetch` memindai 345 dependensi dan melaporkan 2 advisori serta 2 peringatan pemeliharaan. Snapshot RustSec lokal bertanggal 18 November 2026. Indeks crates.io tidak dapat dikunci di lingkungan audit, sehingga status yanked belum terverifikasi. |
| Git dan artefak | 33 berkas terlacak sebelum laporan, working tree awal bersih, dan `git fsck` selesai tanpa temuan. Graphify memetakan 544 node. `target/` sekitar 3,7 GiB dan `graphify-out/` sekitar 4,2 MiB. Keduanya diabaikan Git. |

## Wajib diperbaiki

1. **Kritis bila HTTP dapat dijangkau pihak lain. API administrasi tidak punya autentikasi dan otorisasi.** Router yang sama menyediakan operasi query, perubahan proteksi, DDL, restore, unduh backup, dan provisioning tanpa middleware identitas. Default loopback mengurangi paparan jaringan. Jika servis dipasang di proxy atau dibagikan, setiap pemanggil yang bisa menjangkaunya mendapat akses operasi admin. Pertahankan akses lokal dan tambahkan autentikasi serta pembatasan tindakan sebelum pemakaian bersama. [Router](crates/guidb-server/src/lib.rs#L19), [bind](crates/guidb-server/src/main.rs#L23).

2. **Tinggi. DSN utuh masuk ke respons dan HTML.** Katalog menyimpan target sebagai teks dan mengirimkannya lewat profil koneksi. SSR menyisipkan profil itu ke JSON halaman. Jika URI berisi password, password diterima browser meski layar tidak menampilkannya. Simpan rahasia terpisah dan kirim target yang sudah disamarkan. Batasi izin data, terutama karena `data/` berizin `0777` dan file database serta dump berizin `0644` pada pemeriksaan ini. Empat profil workspace terdaftar memakai penyimpanan lokal. [Katalog](crates/guidb-server/src/catalog.rs#L647), [SSR](crates/guidb-server/src/ui.rs#L677).

3. **Tinggi. Proteksi raw query dan Schema Sync tidak konsisten.** Pengecekan mentah memakai kata pertama dan pencarian substring. Klasifikasi prefix SQL saja tidak cukup untuk menjamin transaksi hanya-baca. Jalur Schema Sync menerima daftar SQL dari klien, memeriksa mode ReadOnly, lalu mengeksekusi setiap statement tanpa pemeriksaan SafeGuard yang dipakai jalur query. Bila langkah lanjut gagal, langkah sebelumnya dapat sudah tersimpan. Buat rencana migrasi di server, pakai pemeriksaan bersama di semua jalur tulis, dan pakai hak database hanya-baca untuk profil ReadOnly. Terapkan migrasi secara transaksional bila engine mendukungnya. [Proteksi](crates/guidb-server/src/safety.rs#L57), [handler](crates/guidb-server/src/api.rs#L305), [eksekusi](crates/guidb-server/src/drivers/sync.rs#L199).

4. **Tinggi. Backup NoSQL berlabel penuh dapat kehilangan data.** Uji HTTP pada key-value lokal mengarsipkan 200 dari 205 key dan mengembalikan sukses. Backup dokumen juga membaca satu halaman, maksimal 500 dokumen per koleksi. Di Redis jaringan, nilai non-string direpresentasikan sebagai label tipe sehingga dump tidak membawa datanya. Checksum tetap bisa lolos karena menguji file hasil penulisan. Gunakan backup native untuk Redis jaringan atau batasi fitur sampai mendukung semua tipe. Untuk store lokal, gunakan snapshot atau baca seluruh halaman dengan urutan stabil. [Backup](crates/guidb-server/src/backup.rs#L243), [driver Redis/KV](crates/guidb-server/src/drivers/kv.rs#L211), [driver dokumen](crates/guidb-server/src/drivers/document.rs#L162).

5. **Tinggi. Dump relasional tidak menjamin pemulihan utuh.** Dump membaca halaman terpisah dengan offset dan tanpa snapshot konsisten. Tabel yang berubah selama backup dapat menghasilkan baris hilang atau ganda. Inventaris dump SQLite hanya mengambil tabel dan view sehingga trigger tidak ikut. Restore SQL selalu menuju SQLite dan mengganti teks `AUTO_INCREMENT` secara global. DDL eksternal bisa gagal dan string data yang memuat kata tersebut bisa berubah. Pakai mekanisme snapshot native per engine atau batasi dukungan restore secara eksplisit dan hentikan konversi teks global. [Dump](crates/guidb-server/src/backup.rs#L183), [inventaris SQLite](crates/guidb-server/src/drivers/sqlite.rs#L100), [restore](crates/guidb-server/src/backup.rs#L458).

6. **Tinggi. Mati listrik dapat memisahkan arsip dari katalognya.** Backup langsung ditulis ke nama final sebelum metadata masuk SQLite, tanpa sinkronisasi file eksplisit. Pruning mengabaikan kegagalan hapus file maupun kegagalan hapus metadata. Gangguan di antaranya dapat meninggalkan file yatim atau katalog yang menunjuk file hilang. Tulis dan sinkronkan file sementara, ganti nama secara atomik, lalu simpan metadata. Rekonsiliasi arsip dan katalog saat startup serta catat kegagalan pruning. [Penulisan](crates/guidb-server/src/backup.rs#L294), [pruning](crates/guidb-server/src/backup.rs#L703).

7. **Sedang. Restore NoSQL dapat menerbitkan koneksi yang baru terisi sebagian.** Profil koneksi dibuat sebelum bulk insert. Insert dilakukan satu per satu tanpa transaksi. Bila restore berhenti di tengah, profil yang terlihat di UI bisa menunjuk data setengah jadi. Isi database sementara dalam transaksi dan daftarkan profil setelah verifikasi berhasil. [Restore NoSQL](crates/guidb-server/src/backup.rs#L498).

8. **Sedang. TTL penyimpanan key-value lokal tidak berjalan.** Uji HTTP menulis key dengan TTL 1 detik. Setelah 1,4 detik, API masih mengembalikan TTL 1 dan baris tetap tersedia di SQLite. Driver menyimpan durasi tetapi tidak menghitung waktu kedaluwarsa saat membaca. Simpan waktu kedaluwarsa dan terapkan secara konsisten pada GET, TTL, browser key, dan backup. [SET, EXPIRE, TTL](crates/guidb-server/src/drivers/kv.rs#L474).

9. **Sedang. Dukungan MongoDB eksternal yang ditawarkan tidak operasional.** Tes koneksi memeriksa TCP dan dapat melaporkan reachable. Jalur workspace menolak URI `mongodb://`. Tes integrasi yang bernama MongoDB memakai penyimpanan dokumen lokal di SQLite. Bedakan kemampuan lokal, deteksi host, dan koneksi native di UI dan README. Tambahkan driver native bila koneksi eksternal memang diperlukan. [Adapter](crates/guidb-server/src/drivers/mod.rs#L58), [tes koneksi](crates/guidb-server/src/drivers/mod.rs#L189), [tes integrasi](crates/guidb-server/tests/api_integration.rs#L184).

10. **Sedang. Ekspor berhenti diam-diam setelah satu halaman.** Route ekspor meminta offset 0 dan limit 500. Driver KV membatasi 200. Pengguna yang mengunduh tabel lebih besar bisa memakai file parsial tanpa penanda. Labeli batas di UI atau stream seluruh hasil. Gunakan escaping CSV yang sama untuk judul kolom dan isi. [Ekspor](crates/guidb-server/src/api.rs#L466), [CSV](crates/guidb-server/src/api.rs#L552).

11. **Sedang. Lockfile memuat dua advisori RustSec.** Pemindaian lokal menandai `libsqlite3-sys 0.30.1` dan `rsa 0.9.10`. Ini temuan dependensi, dengan keterjangkauan jalur rentan pada GuiDB belum diuji. Rencanakan pembaruan SQLx dan dependensi transitif yang kompatibel, lalu jalankan tes tiap engine. `chrono` dan `paste` muncul sebagai peringatan pemeliharaan. [libsqlite3-sys](https://rustsec.org/advisories/RUSTSEC-2026-0459.html), [rsa](https://rustsec.org/advisories/RUSTSEC-2023-0071.html).

## Perlu diperbaiki

12. **Permintaan lama bisa menimpa hasil koneksi baru.** Query memakai koneksi yang aktif saat dikirim, lalu menulis hasil ke `state.workspace` setelah respons tanpa memeriksa apakah pengguna sudah berpindah. Saat klik koneksi B sementara query A berjalan, grid B bisa memperlihatkan hasil A. Simpan identitas workspace per permintaan dan abaikan respons yang sudah usang. Blokir commit ganda selama mutasi berjalan. [Query dan commit browser](static/app.js#L875).

13. **Pekerjaan besar menahan memori dan memperlambat halaman.** Raw query relasional mengumpulkan seluruh hasil di memori. Browser dokumen mengambil dan mengurai satu koleksi sebelum memotong halaman. Daftar objek SQL juga melakukan hitungan per objek. Batasi hasil dan waktu query, pindahkan pagination yang bisa dilakukan ke database, serta hitung jumlah saat dibutuhkan. [Raw query SQLite](crates/guidb-server/src/drivers/sqlite.rs#L320), [dokumen](crates/guidb-server/src/drivers/document.rs#L114), [daftar objek](crates/guidb-server/src/drivers/sqlite.rs#L100).

14. **Scheduler bisa mengulang job bersamaan dan saling memangkas arsip.** Scheduler dan tombol jalankan sekarang dapat masuk ke fungsi yang sama tanpa klaim job. Dua jadwal untuk satu koneksi memakai kumpulan retensi koneksi yang sama walau kebijakannya berbeda. Gunakan klaim atomik per jadwal dan kaitkan retensi ke jadwal, atau batasi satu jadwal per koneksi. [Scheduler dan retensi](crates/guidb-server/src/backup.rs#L673).

15. **Skrip verifikasi sulit dipindah dan mudah salah sasaran.** Skrip mengunci path repo dan port 8787. Jika port sudah melayani proses lain, probe kesiapan bisa lulus terhadap proses lama. Skrip juga menyebut pengujian MongoDB/Redis tanpa menjelaskan bahwa keduanya menggunakan SQLite. Pakai path relatif ke skrip, port terisolasi, dan label engine yang akurat. [Skrip](scripts/verify_all.sh#L1).

16. **Dialog belum menjaga fokus keyboard.** Playwright mengonfirmasi Tab bisa memindahkan fokus keluar dari dialog koneksi. Setelah Escape, fokus tidak kembali ke tombol pembuka. Terapkan perilaku dialog native atau implementasikan siklus fokus dan pemulihan fokus pada semua dialog. [Pembukaan dan penutupan](static/app.js#L134), [handler keyboard](static/app.js#L1459).

## Batas audit

Pengujian runtime memakai server sementara dan data demo nyata di SQLite. PostgreSQL, MySQL/MariaDB, serta Redis eksternal belum diuji end-to-end. MongoDB eksternal dinilai dari implementasi. Provisioning daemon, simulasi mati listrik, uji beban, pentest, dan reverse engineering seluruh artefak build tidak dijalankan. Folder metadata agen tetap di luar pemeriksaan isi. Pemeriksaan bacaan SQLite dan arsip membuktikan kondisi saat audit. Konsistensi antarwaktu dan pemulihan dari kegagalan berikutnya masih perlu diuji.

Urutan perbaikan untuk penggunaan lokal adalah backup dan pemulihan, penegakan proteksi, lalu akurasi data dan UI. Untuk akses bersama, autentikasi dan pemisahan kredensial jadi syarat pertama.

## Tindak lanjut implementasi, 19 November 2026

Temuan di atas mendeskripsikan commit awal. Tabel ini mencatat kondisi setelah perbaikan di workspace.

| Temuan | Perubahan dan batas |
| --- | --- |
| 1–2 | Bind di luar loopback memerlukan password admin. Password dapat diaktifkan juga di loopback, termasuk di belakang proxy. Uji HTTP mengonfirmasi 401 tanpa kredensial dan 200 dengan kredensial. Profil yang dikirim ke browser menyamarkan userinfo dan parameter URI. Akun masih tunggal dan DSN tersimpan di katalog lokal. |
| 3 | Schema Sync memeriksa ulang rencana dari server dan proteksi target sebelum backup. ReadOnly memakai koneksi baca-saja SQLite atau transaksi baca-saja PostgreSQL/MySQL untuk konsol query. Grid relasional memerlukan seluruh primary key untuk edit/hapus dan memeriksa jumlah baris terdampak. Migrasi DDL lintas-statement masih dapat berjalan sebagian jika engine gagal di tengah. |
| 4–5 | Backup penuh SQLite menggunakan snapshot native. Uji pemulihan mencakup isi tabel serta trigger. Dokumen dan KV lokal memakai satu transaksi baca untuk arsip lengkap. Uji HTTP memulihkan 555 key. Backup Redis jaringan diarahkan ke mekanisme RDB native. Dump dan restore native PostgreSQL/MariaDB masih terbuka. |
| 6–7 | File backup disinkronkan, diganti nama, dan direktorinya disinkronkan sebelum pencatatan metadata. Restore NoSQL mengisi data dalam transaksi lalu menerbitkan profil. Pruning mempertahankan metadata bila penghapusan file gagal. Pemulihan otomatis file yatim serta uji putus listrik fisik masih terbuka. |
| 8–9 | TTL lokal dihitung terhadap waktu penyimpanan dan key yang kedaluwarsa dibersihkan. Uji dengan TTL satu detik menghasilkan status key hilang. Pesan aplikasi menjelaskan dukungan deteksi TCP MongoDB eksternal. Editor dokumen memakai SQLite lokal. |
| 10–11 | Ekspor relasional membaca halaman berikutnya. Tes mengunduh seluruh 501 baris SQLite. Ekspor besar masih memakai memori dan ekspor NoSQL masih terbatas pada halaman data. Dua advisori dependensi dari hasil audit awal tetap terbuka. |
| 12–13 | Permintaan navigasi dan query memakai nomor urut agar respons lama tidak mengganti grid terbaru. Commit ganda dibatasi, hasil ad hoc hanya-baca, dan tabel memiliki navigasi halaman. Raw query dan ekspor besar masih membutuhkan pembatasan penggunaan memori. |
| 14–15 | Job memakai file lock per jadwal dan memeriksa kembali waktu jatuh tempo. Retensi arsip baru mengacu pada ID jadwal. Migrasi hanya mengaitkan arsip lama bila koneksi memiliki tepat satu jadwal. Skrip E2E memakai port dinamis serta direktori data sementara. |
| 16 | Uji Chromium mengonfirmasi fokus tetap dalam dialog koneksi saat Tab ditekan dan kembali ke tombol pembuka setelah Escape. Pemeriksaan halaman utama pada lebar 375, 768 dan 1440 piksel dalam kedua tema lolos pemeriksaan overflow, error runtime, permintaan gagal, dan axe. |

Alur grid dan perubahan struktur diuji melalui browser untuk SQLite. Driver PostgreSQL dan MariaDB diuji melalui HTTP terhadap daemon sementara dan hasilnya diperiksa langsung dengan klien SQL native. Pengujian memakai database sementara. Semua perubahan tersedia di workspace.

## Pembaruan dependensi dan deployment lokal

Lockfile rilis memakai SQLx 0.9.1, libsqlite3-sys 0.40.0, dan rsa 0.10.0. Pemindaian ulang `cargo audit` pada 339 paket melaporkan nol kerentanan dan dua peringatan pemeliharaan untuk chrono dan paste. Startup layanan systemd memerlukan sandi admin. Layanan membatasi izin file baru lewat umask 0077. Header CSP membatasi sumber skrip dan melarang framing. Log rute hanya menyertakan path. Pada pemuatan workspace, ERD menggunakan kembali daftar tabel sehingga driver relasional hanya menghitung jumlah baris sekali per tabel pada tahap inventaris.
