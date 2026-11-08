# media_downloader

Pengunduh video ala IDM untuk Windows. Arahkan kursor ke video di halaman web, klik tombol **Download video**, dan file tersimpan ke folder pilihan Anda dengan progres, **pause**, dan **resume** (tetap bisa dilanjutkan setelah error, browser ditutup, atau komputer restart).

```
 Browser (Chrome/Edge)                 Komputer Anda
┌───────────────────────┐  native   ┌──────────┐  named pipe   ┌──────────────────────┐
│ Ekstensi:             │ messaging │ mdl-shim │ ◄───────────► │ mdl-daemon           │
│ tombol + popup        │ ◄───────► │ (relay)  │  \\.\pipe\mdl │ mesin download       │
└───────────────────────┘           └──────────┘               │ (pause/resume/antri) │
                                                               └──────────────────────┘
```

- **mdl-daemon**: program latar belakang yang benar-benar mengunduh. Tetap berjalan walau popup atau browser ditutup.
- **mdl-shim**: penghubung kecil antara browser dan daemon. Dijalankan otomatis oleh browser.
- **Ekstensi**: menampilkan tombol di atas video dan popup progres.
- **mdl-cli**: alat baris perintah (opsional).

> **Status:** didukung: video MP4 biasa, **YouTube**, **Facebook**, dan ratusan situs lain yang dikenali [yt-dlp](https://github.com/yt-dlp/yt-dlp) (maksimal 1080p). Tidak didukung: video yang hanya tersedia sebagai stream HLS/DASH, serta konten DRM (Netflix, Disney+, dll.).

---

## 1. Persyaratan

| Kebutuhan | Keterangan |
|---|---|
| Windows 10/11 | Daemon memakai named pipe Windows |
| [Rust](https://rustup.rs) 1.85+ | Untuk build. Cek dengan `cargo --version` |
| Google Chrome 116+ atau Microsoft Edge | Untuk ekstensi |
| [yt-dlp](https://github.com/yt-dlp/yt-dlp) | Untuk YouTube, Facebook, dan situs streaming lain. Pasang: `winget install yt-dlp.yt-dlp` (otomatis ikut memasang **deno**, yang dibutuhkan YouTube) |
| [ffmpeg](https://ffmpeg.org) | Menggabungkan video dan audio yang diunduh terpisah. Pasang: `winget install Gyan.FFmpeg` |

Cek dengan `yt-dlp --version` dan `ffmpeg -version` di PowerShell baru. Tanpa keduanya, video MP4 biasa tetap bisa diunduh.

Tidak butuh hak Administrator; semuanya dipasang per pengguna.

---

## 2. Instalasi

### Langkah 1: Build dan pasang program

Buka **PowerShell** di folder proyek (folder yang berisi `install.ps1`), lalu jalankan:

```powershell
cd "F:\Rust Project\media_downloader\media_downloader"
.\install.ps1
```

Jika muncul error *"running scripts is disabled"*, jalankan sekali lalu ulangi:

```powershell
Set-ExecutionPolicy -Scope CurrentUser RemoteSigned
```

Skrip ini akan:
1. Build program (`cargo build --release`), sekitar 1 menit pada build pertama.
2. Menyalin `mdl-daemon.exe`, `mdl-shim.exe`, dan `mdl-cli.exe` ke `%LOCALAPPDATA%\mdl\bin`.
3. Mendaftarkan host `com.mdl.host` ke Chrome **dan** Edge (registry HKCU).

Hasil yang benar diakhiri dengan:

```
Installed to   C:\Users\<nama>\AppData\Local\mdl\bin
Allowed origin chrome-extension://nockdfbbjmnmaohkjhehabjllaoclfdm/
Registered for Chrome + Edge. ...
```

Opsi tambahan:

| Perintah | Fungsi |
|---|---|
| `.\install.ps1 -Autostart` | Daemon otomatis jalan saat login Windows (disarankan) |
| `.\install.ps1 -NoBuild` | Pasang ulang tanpa build |
| `.\install.ps1 -Uninstall` | Hapus semuanya (lihat bagian 6) |

> Tanpa `-Autostart` pun daemon akan dijalankan otomatis saat pertama kali ekstensi membutuhkannya.

### Langkah 2: Pasang ekstensi browser

1. Buka `chrome://extensions` (Chrome) atau `edge://extensions` (Edge).
2. Aktifkan **Developer mode** (pojok kanan atas di Chrome, panel kiri di Edge).
3. Klik **Load unpacked**, lalu pilih folder:
   ```
   F:\Rust Project\media_downloader\media_downloader\extension
   ```
4. Pastikan ID ekstensi yang tampil adalah:
   ```
   nockdfbbjmnmaohkjhehabjllaoclfdm
   ```
   Jika berbeda, host akan menolak koneksi. Lihat bagian Pemecahan Masalah.
5. (Opsional) Klik ikon puzzle 🧩 di toolbar lalu **pin** media_downloader agar ikonnya selalu terlihat.

> **Developer mode harus tetap ON.** Chrome menonaktifkan semua ekstensi yang dipasang lewat *Load unpacked* begitu Developer mode dimatikan (pesan: *"To use this extension, turn on developer mode"*). Ini aman untuk ekstensi buatan sendiri. Jika tidak sengaja dimatikan: nyalakan lagi, lalu aktifkan kembali media_downloader di halaman extensions.

### Langkah 3: Cek koneksi

Klik ikon media_downloader di toolbar. Di pojok kanan atas popup harus tertulis **● daemon running**.

Jika tertulis *offline* atau *Native host not installed*, lihat bagian 5.

---

## 3. Cara Pakai

### Mengunduh video

1. Buka halaman yang berisi video. Untuk uji coba, pakai halaman "HTML5 Video" di w3schools.
2. **Arahkan kursor** ke video (atau klik videonya). Tombol **⬇ Download video** muncul di pojok kanan atas video.
3. Klik tombolnya. Tombol berubah menjadi **✓ Added** dan ikon ekstensi menampilkan `+1`.
   Untuk YouTube/Facebook tombol menampilkan **Finding video…** selama beberapa detik dulu (yt-dlp mencari alamat video asli).
4. Klik ikon ekstensi untuk melihat progres.

### Popup: mengelola download

```
┌ media_downloader ─────────── ● daemon running ┐
│ Video kucing.mp4                     [⏸] [✕]  │
│ ███████████░░░░░░  running · 62% · 3.1 MB/s   │
│ Kuliah.mp4                           [▶] [✕]  │
│ ███████░░░░░░░░░░  paused · 41%               │
│ Reel.mp4                            [📂] [🗑]  │
│ █████████████████  done                       │
├───────────────────────────────────────────────┤
│ Save to: D:\Videos                            │
└───────────────────────────────────────────────┘
```

| Tombol | Fungsi |
|---|---|
| ⏸ | Pause; bagian yang sudah terunduh tetap tersimpan |
| ▶ | Lanjutkan dari posisi terakhir |
| ↻ | Coba lagi download yang gagal (tetap melanjutkan, tidak mulai dari nol) |
| ✕ | Batalkan dan hapus file sementara |
| 📂 | Buka folder dan sorot file di Explorer |
| 🗑 | Hapus dari daftar (file hasil download **tidak** dihapus) |

- Popup boleh ditutup kapan saja; download tetap berjalan di daemon.
- Maksimal 3 download berjalan bersamaan; sisanya menunggu antrean (*queued*).

### Mengatur folder penyimpanan

Di bagian bawah popup, isi **Save to** dengan path lengkap, misalnya `D:\Videos`. Pengaturan ini berlaku untuk download berikutnya.
Jika dikosongkan, file disimpan ke folder **Downloads** Anda.

Nama file diambil dari judul halaman. Jika nama sudah ada, otomatis menjadi `nama (1).mp4`.

### Resume setelah gangguan

| Kejadian | Yang terjadi |
|---|---|
| Internet putus | Otomatis dicoba ulang beberapa kali; jika tetap gagal, status *failed*. Klik ↻ |
| Browser ditutup | Download tetap berjalan di daemon |
| Komputer mati atau restart | Saat daemon jalan lagi, download yang belum selesai otomatis dilanjutkan |
| Link YouTube/Facebook kedaluwarsa | Saat resume, halaman dicari ulang otomatis dan download melanjutkan dari byte terakhir |

Selama proses, di folder tujuan ada dua file sementara: `nama.mp4.part` (data) dan `nama.mp4.mdl.json` (catatan bagian yang selesai). Jangan dihapus jika ingin bisa resume. Setelah selesai, keduanya otomatis menjadi `nama.mp4`.

---

## 4. Baris Perintah (opsional)

`mdl-cli.exe` ada di `%LOCALAPPDATA%\mdl\bin`. Contoh:

```powershell
$cli = "$env:LOCALAPPDATA\mdl\bin\mdl-cli.exe"

& $cli ping                                  # cek daemon hidup
& $cli add https://contoh.com/video.mp4 D:\Videos
& $cli add --page https://www.youtube.com/watch?v=... D:\Videos   # halaman video
& $cli list                                  # daftar semua download
& $cli watch                                 # tampilan progres live (Ctrl+C untuk keluar)
& $cli pause 1
& $cli resume 1
& $cli cancel 1
& $cli remove 1

# Download langsung tanpa daemon (bisa resume dengan menjalankan ulang perintah yang sama):
& $cli get https://contoh.com/video.mp4 D:\Videos -c 4
```

---

## 5. Pemecahan Masalah

| Gejala | Solusi |
|---|---|
| Popup: *Native host not installed* | Jalankan `.\install.ps1` lagi, lalu tutup dan buka ulang popup |
| Popup: *offline* / *Lost connection* | Jalankan `& "$env:LOCALAPPDATA\mdl\bin\mdl-daemon.exe"` untuk melihat errornya, atau cek log (di bawah) |
| ID ekstensi bukan `nockdfbb...` | Pastikan Anda memilih folder `extension` dari proyek ini dan file `manifest.json` tidak diubah. Atau pasang ulang host dengan ID yang tampil: `.\install.ps1 -ExtensionId <id>` |
| *To use this extension, turn on developer mode* | Nyalakan lagi **Developer mode** di `chrome://extensions` lalu aktifkan media_downloader. Ekstensi unpacked memang hanya jalan dengan Developer mode ON |
| Tombol tidak muncul di atas video | Muat ulang halaman (F5). Setelah ekstensi di-reload, halaman yang sudah terbuka harus di-refresh |
| *yt-dlp is not installed* / *ffmpeg is not installed* | Jalankan perintah `winget install` yang disebut di pesan, lalu coba lagi (tidak perlu restart browser) |
| *No downloadable video found on this page* | Anda berada di halaman feed. Buka video-nya sendiri (klik judul / tanggal postingan) lalu klik Download lagi |
| *only offered as an HLS/DASH stream* | Video itu (sering: siaran langsung) belum bisa diunduh |
| YouTube/Facebook tiba-tiba gagal semua | Situsnya berubah. Perbarui yt-dlp: `winget upgrade yt-dlp.yt-dlp` |
| Video Facebook privat / grup | Pastikan Anda login Facebook di browser yang sama; ekstensi mengirim cookie hanya jika situs memintanya |
| *rate-limited (HTTP 429)* | Server membatasi koneksi. Tunggu sebentar lalu klik ↻ |
| *HTTP 403 — the URL may have expired* | Link video kedaluwarsa. Buka ulang halamannya dan klik Download lagi |
| Setelah mengubah kode Rust tidak ada perubahan | Jalankan `.\install.ps1` lagi; browser memakai salinan di `%LOCALAPPDATA%\mdl\bin`, bukan folder `target` |

**Lokasi log dan data:**

| File | Isi |
|---|---|
| `%LOCALAPPDATA%\mdl\daemon.log` | Log daemon |
| `%LOCALAPPDATA%\mdl\shim.log` | Log koneksi browser |
| `%LOCALAPPDATA%\mdl\jobs.json` | Daftar download (berisi cookie situs untuk resume; **jangan dibagikan**) |

Buka foldernya dengan: `explorer "$env:LOCALAPPDATA\mdl"`

---

## 6. Uninstall

```powershell
.\install.ps1 -Uninstall
```

Lalu hapus ekstensinya di `chrome://extensions` / `edge://extensions`.
Daftar download dan log di `%LOCALAPPDATA%\mdl` sengaja tidak dihapus; hapus manual jika tidak diperlukan.

---

## 7. Untuk Pengembang

```
crates/engine     mesin download: HTTP Range paralel, file .part + catatan resume
crates/protocol   format pesan JSON antara daemon, CLI, dan shim
crates/daemon     mdl-daemon: manajer job, antrean, named pipe
crates/shim       mdl-shim: host native messaging (penghubung browser ke daemon)
crates/cli        mdl-cli
extension/        ekstensi Chrome/Edge (Manifest V3)
install.ps1       pemasang per pengguna
keys/             kunci privat ekstensi (tidak di-commit)
```

```powershell
cargo build --release     # build semua
cargo test                # jalankan test
.\install.ps1 -NoBuild    # pasang hasil build
```

Setelah mengubah file di `extension/`, klik ↻ (reload) pada kartu ekstensi di halaman extensions, lalu refresh halaman web yang dibuka.

---

## Catatan Penggunaan

Aplikasi ini untuk penggunaan pribadi. Hormati hak cipta dan ketentuan layanan situs yang Anda kunjungi. Aplikasi ini tidak dan tidak akan membobol perlindungan DRM.