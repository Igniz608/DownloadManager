# VortexDL

Modern, lightweight, customizable download manager written in Rust for Windows — combining multi-connection HTTP/HTTPS acceleration, universal video/media downloading, BitTorrent support, JDownloader 2-style multi-part archive handling, selectable post-download extractors, and a Manifest V3 browser extension.

## Architecture

- **`crates/vortex-core`**:
  - `http_engine`: Multi-connection segmented HTTP/HTTPS downloader with dynamic work-stealing, `Range` / `ETag` / `Content-Disposition` (RFC 5987), global bandwidth limiter, pause/resume, and SHA-256 / MD5 verification.
  - `media_engine`: Universal video & audio downloader (`yt-dlp` + `FFmpeg` integration with 1-click installers under `%LOCALAPPDATA%\VortexDL\bin`), format/playlist probing, plus a native Rust HLS (`.m3u8`) multi-segment downloader fallback.
  - `torrent_engine`: Native BitTorrent engine via `librqbit` supporting magnet links, `.torrent` files, per-file selection, peer/ratio stats, and seeding limits.
  - `package_grabber`: Link extraction and JDownloader 2-style multi-part grouping (`.partXX.rar`, `.7z.001`, `.zip.001`, `.z01`–`.z99`, `.r00`–`.r99`, `.001`) with missing-part detection.
  - `extractor`: Auto-detection and execution of **NanaZip**, **7-Zip**, **WinRAR**, **PeaZip**, **Windows Native** (`tar.exe` / PowerShell `Expand-Archive`), and custom CLI plugins with automatic password-list retries.
  - `ipc_server`: Local HTTP API (`http://127.0.0.1:17654`) + Windows `HKCU` Native Messaging registry installer for Chrome, Edge, Brave, Vivaldi, and Firefox.
- **`crates/vortex-native-host`**: Lightweight stdio Native Messaging bridge (`com.vortexdl.host`) that auto-launches `vortex-gui.exe` if closed.
- **`crates/vortex-gui`**: Native GPU-accelerated `eframe` / `egui` desktop UI with system tray, live speed sparkline, segment progress strip, LinkGrabber, Theme Studio (6 presets + live color/layout editor), and 1-click tool/extension setup.
- **`extension/`**: Manifest V3 browser extension for download interception, `.m3u8`/`.mpd`/video/audio stream sniffing with session cookies & `Referer`, context menus, and batch LinkGrabber capture.

## Build & Run

```powershell
cargo test --workspace
cargo build --release --workspace
.\target\release\vortex-gui.exe
```

## Installing the Browser Extension

### Microsoft Edge (Automatic via Edge Add-ons Store)
1. Install VortexDL with the Windows Setup (`VortexDL-Setup`) keeping **Microsoft Edge** checked (or click **Integrar Selecionados** inside **Extensão Navegador**).
2. VortexDL registers `com.vortexdl.host` (`allowed_origins` includes Store ID `kgkaideidlhnmjneefpgilmifehcncni`) and configures `HKCU\Software\Microsoft\Edge\Extensions\kgkaideidlhnmjneefpgilmifehcncni` automatically — no Developer mode required.

### Mozilla Firefox (1-Click Signed XPI)
1. Install VortexDL with **Mozilla Firefox** checked (or click **Instalar no Firefox (.xpi)** / **Integrar Selecionados** inside **Extensão Navegador**).
2. VortexDL registers `com.vortexdl.host` (`vortexdl@igniz608.dev`) and launches the Mozilla-signed `vortexdl-firefox.xpi` directly in Firefox so you can confirm in 1 click (**Add**) — no `about:debugging` required.

### Chrome / Brave / Vivaldi / Opera (Local Unpacked)
1. Open `chrome://extensions` (or the browser's extensions page) and enable **Developer mode**.
2. Click **Load unpacked** and select the `extension/` folder.
3. In VortexDL, open **Extensão Navegador** and click **Integrar Selecionados**.
