# RemoteDesk

Remote desktop ringan: host berupa program Rust, viewer berupa halaman web yang dibuka di browser
(Linux, Windows, atau Safari di iPhone). Video dikirim sebagai VP8 lewat WebSocket TLS.

## Cara kerja

- Host Linux (Wayland) memakai xdg-desktop-portal (RemoteDesktop + ScreenCast) untuk menangkap layar dan menyuntikkan input.
- Aliran PipeWire diencode dengan `gst-launch-1.0` (`vp8enc`); `--codec jpeg` tersedia sebagai cadangan.
- Viewer mendekode dengan WebCodecs (butuh HTTPS; Safari iOS 16.4+).
- Host Windows memakai `ffmpeg.exe` (taruh di folder yang sama dengan `remotedesk.exe`).

## Jalankan

Kebutuhan di Linux: Rust, GStreamer dengan plugin `pipewire` dan `vpx`.

```bash
cargo build --release
./target/release/remotedesk
```

Lalu buka `https://<IP-host>:8443` di browser dan masukkan password yang ditampilkan di terminal.

Opsi: `--port`, `--password`, `--fps`, `--codec vp8|jpeg`, `--bitrate`, `--quality`, `--max-width`, `--grab ddagrab|gdigrab` (Windows).

Password, sertifikat self-signed, dan token izin portal disimpan di `~/.config/remotedesk/`, di luar repositori.

## Keamanan

Proyek ini belum diaudit. Program ini memberi kontrol penuh atas mesin Anda, jadi jangan buka port 8443 langsung ke internet.
Untuk akses jarak jauh, pakai VPN seperti Tailscale atau WireGuard. Browser akan memperingatkan sertifikat self-signed; itu wajar.

## Belum ada

Relay/ID lintas internet, clipboard, transfer file, audio, dan uji sentuhan di iPhone sungguhan.
Tangkapan layar di Windows dan viewer di browser sungguhan belum diuji menyeluruh.

## Lisensi

MIT, lihat [LICENSE](LICENSE).
