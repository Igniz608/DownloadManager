use crate::manager::DownloadManager;
use crate::media_engine::{detect_media_tools, probe_media_url, probe_single_media_url};
use crate::model::{MediaProbeResult, NewDownloadRequest, NewMediaRequest};
use crate::state::app_data_dir;
use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

#[cfg(windows)]
use winreg::{RegKey, enums::*};

#[derive(Debug, Serialize)]
pub struct PingResponse {
    pub app: &'static str,
    pub version: &'static str,
    pub active_downloads: usize,
    pub total_speed_bps: u64,
}

#[derive(Debug, Deserialize)]
pub struct GrabPayload {
    pub urls: Vec<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct TorrentPayload {
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct ApiActionResponse {
    pub ok: bool,
    pub message: String,
    pub id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MediaProbePayload {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub single_video: bool,
}

#[derive(Debug, Serialize)]
pub struct MediaProbeApiResponse {
    pub ok: bool,
    pub message: Option<String>,
    pub result: Option<MediaProbeResult>,
}

#[derive(Debug, Serialize)]
pub struct StatusItemSummary {
    pub id: String,
    pub filename: String,
    pub status: String,
    pub progress: f32,
    pub speed_bps: u64,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub active_count: usize,
    pub total_speed_bps: u64,
    pub items: Vec<StatusItemSummary>,
}

#[derive(Debug, Serialize)]
pub struct BrowserPendingResponse {
    pub pending_urls: Vec<String>,
    pub queue_len: usize,
}

#[derive(Debug, Deserialize)]
pub struct BrowserResolvedPayload {
    pub page_url: String,
    #[serde(default)]
    pub direct_url: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct BrowserResolvedResponse {
    pub ok: bool,
    pub matched: bool,
    pub next_url: Option<String>,
    pub remaining: usize,
    pub message: String,
}

pub fn spawn_ipc_server(manager: Arc<DownloadManager>, port: u16) {
    let runtime = tokio::runtime::Handle::current();
    runtime.spawn(async move {
        let app = Router::new()
            .route("/api/ping", get(handle_ping))
            .route("/api/status", get(handle_status))
            .route("/api/downloads", post(handle_add_download))
            .route("/api/grab", post(handle_grab))
            .route("/api/browser-pending", get(handle_browser_pending))
            .route("/api/browser-resolved", post(handle_browser_resolved))
            .route("/api/media", post(handle_add_media))
            .route("/api/media/probe", post(handle_probe_media))
            .route("/api/torrent", post(handle_add_torrent))
            .route("/api/pause-all", post(handle_pause_all))
            .route("/api/resume-all", post(handle_resume_all))
            .route("/api/ext-log", post(handle_ext_log))
            .layer(CorsLayer::permissive())
            .with_state(manager);

        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
            tracing::info!("VortexDL IPC server listening on http://{addr}");
            let _ = axum::serve(listener, app).await;
        } else {
            tracing::warn!("Could not bind IPC port {port}");
        }
    });
}

async fn handle_ping(State(mgr): State<Arc<DownloadManager>>) -> Json<PingResponse> {
    let snap = mgr.snapshot();
    Json(PingResponse {
        app: "VortexDL",
        version: env!("CARGO_PKG_VERSION"),
        active_downloads: snap.active_count,
        total_speed_bps: snap.total_download_bps,
    })
}

async fn handle_status(State(mgr): State<Arc<DownloadManager>>) -> Json<StatusResponse> {
    let snap = mgr.snapshot();
    let items = snap
        .downloads
        .iter()
        .take(12)
        .map(|d| StatusItemSummary {
            id: d.id.to_string(),
            filename: d.filename.clone(),
            status: d.status.label().to_string(),
            progress: d.progress_fraction(),
            speed_bps: d.speed_bps,
            total_bytes: d.total_bytes,
            downloaded_bytes: d.downloaded_bytes,
        })
        .collect();
    Json(StatusResponse {
        active_count: snap.active_count,
        total_speed_bps: snap.total_download_bps,
        items,
    })
}

async fn handle_add_download(
    State(mgr): State<Arc<DownloadManager>>,
    Json(req): Json<NewDownloadRequest>,
) -> (StatusCode, Json<ApiActionResponse>) {
    match mgr.add_http_download(req) {
        Ok(id) => (
            StatusCode::OK,
            Json(ApiActionResponse {
                ok: true,
                message: "Download adicionado ao VortexDL".into(),
                id: Some(id.to_string()),
            }),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ApiActionResponse {
                ok: false,
                message: e.to_string(),
                id: None,
            }),
        ),
    }
}

async fn handle_grab(
    State(mgr): State<Arc<DownloadManager>>,
    Json(payload): Json<GrabPayload>,
) -> Json<ApiActionResponse> {
    let count = payload.urls.len();
    mgr.request_focus_grabber();
    mgr.probe_urls_into_grabber(payload.urls, payload.headers);
    Json(ApiActionResponse {
        ok: true,
        message: format!("{count} link(s) enviados para o LinkGrabber"),
        id: None,
    })
}

async fn handle_browser_pending(
    State(mgr): State<Arc<DownloadManager>>,
) -> Json<BrowserPendingResponse> {
    let (pending_urls, queue_len) = mgr.pending_browser_urls();
    Json(BrowserPendingResponse {
        pending_urls,
        queue_len,
    })
}

async fn handle_browser_resolved(
    State(mgr): State<Arc<DownloadManager>>,
    Json(payload): Json<BrowserResolvedPayload>,
) -> Json<BrowserResolvedResponse> {
    let outcome = mgr.apply_browser_resolution(
        payload.page_url,
        payload.direct_url,
        payload.filename,
        payload.error,
        payload.headers,
    );
    Json(BrowserResolvedResponse {
        ok: true,
        matched: outcome.matched,
        next_url: outcome.next_url,
        remaining: outcome.remaining,
        message: outcome.message,
    })
}

async fn handle_add_media(
    State(mgr): State<Arc<DownloadManager>>,
    Json(req): Json<NewMediaRequest>,
) -> (StatusCode, Json<ApiActionResponse>) {
    match mgr.add_media_download(req) {
        Ok(id) => (
            StatusCode::OK,
            Json(ApiActionResponse {
                ok: true,
                message: "Download de mídia iniciado".into(),
                id: Some(id.to_string()),
            }),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ApiActionResponse {
                ok: false,
                message: e.to_string(),
                id: None,
            }),
        ),
    }
}

async fn handle_probe_media(
    State(mgr): State<Arc<DownloadManager>>,
    Json(req): Json<MediaProbePayload>,
) -> (StatusCode, Json<MediaProbeApiResponse>) {
    let tools = detect_media_tools(&mgr.settings.read());
    let Some(yt_dlp) = tools.yt_dlp_path else {
        return (
            StatusCode::OK,
            Json(MediaProbeApiResponse {
                ok: false,
                message: Some("yt-dlp não instalado".into()),
                result: None,
            }),
        );
    };

    let res = if req.single_video {
        probe_single_media_url(&yt_dlp, &req.url, &req.headers).await
    } else {
        probe_media_url(&yt_dlp, &req.url, &req.headers).await
    };

    match res {
        Ok(result) => (
            StatusCode::OK,
            Json(MediaProbeApiResponse {
                ok: true,
                message: None,
                result: Some(result),
            }),
        ),
        Err(e) => (
            StatusCode::OK,
            Json(MediaProbeApiResponse {
                ok: false,
                message: Some(e.to_string()),
                result: None,
            }),
        ),
    }
}

async fn handle_add_torrent(
    State(mgr): State<Arc<DownloadManager>>,
    Json(payload): Json<TorrentPayload>,
) -> (StatusCode, Json<ApiActionResponse>) {
    let dir = mgr.default_torrent_dir();
    match mgr.add_torrent_async(&payload.source, dir) {
        Ok(id) => (
            StatusCode::OK,
            Json(ApiActionResponse {
                ok: true,
                message: "Torrent enviado ao VortexDL".into(),
                id: Some(id.to_string()),
            }),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ApiActionResponse {
                ok: false,
                message: e.to_string(),
                id: None,
            }),
        ),
    }
}

async fn handle_pause_all(State(mgr): State<Arc<DownloadManager>>) -> Json<ApiActionResponse> {
    mgr.pause_all();
    Json(ApiActionResponse {
        ok: true,
        message: "Todos os downloads pausados".into(),
        id: None,
    })
}

async fn handle_resume_all(State(mgr): State<Arc<DownloadManager>>) -> Json<ApiActionResponse> {
    mgr.resume_all();
    Json(ApiActionResponse {
        ok: true,
        message: "Downloads retomados".into(),
        id: None,
    })
}

async fn handle_ext_log(Json(payload): Json<serde_json::Value>) -> StatusCode {
    use std::io::Write;
    let log_path = app_data_dir().join("extension-debug.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(file, "{payload}");
    }
    StatusCode::OK
}

/// Registers Chrome, Edge, Brave, and Firefox Native Messaging manifests in Windows HKCU Registry.
pub fn register_browser_native_host(native_host_exe: &Path, chrome_extension_id: Option<&str>) -> Result<Vec<String>> {
    let data_dir = app_data_dir().join("native-host");
    std::fs::create_dir_all(&data_dir)?;

    let ext_origin = chrome_extension_id
        .filter(|s| !s.trim().is_empty())
        .map(|id| format!("chrome-extension://{}/", id.trim()))
        .unwrap_or_else(|| "chrome-extension://*/".to_string());

    let chromium_manifest = serde_json::json!({
        "name": "com.vortexdl.host",
        "description": "VortexDL Native Messaging Host",
        "path": native_host_exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": [ext_origin]
    });
    let chromium_manifest_path = data_dir.join("com.vortexdl.host.chromium.json");
    std::fs::write(
        &chromium_manifest_path,
        serde_json::to_string_pretty(&chromium_manifest)?,
    )?;

    let firefox_manifest = serde_json::json!({
        "name": "com.vortexdl.host",
        "description": "VortexDL Native Messaging Host",
        "path": native_host_exe.to_string_lossy(),
        "type": "stdio",
        "allowed_extensions": ["vortexdl@igniz608.dev"]
    });
    let firefox_manifest_path = data_dir.join("com.vortexdl.host.firefox.json");
    std::fs::write(
        &firefox_manifest_path,
        serde_json::to_string_pretty(&firefox_manifest)?,
    )?;

    let mut registered = Vec::new();
    #[cfg(windows)]
    {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let chromium_keys = [
            ("Google Chrome", r"Software\Google\Chrome\NativeMessagingHosts\com.vortexdl.host"),
            ("Microsoft Edge", r"Software\Microsoft\Edge\NativeMessagingHosts\com.vortexdl.host"),
            ("Brave", r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.vortexdl.host"),
            ("Vivaldi", r"Software\Vivaldi\NativeMessagingHosts\com.vortexdl.host"),
        ];
        for (browser, key_path) in chromium_keys {
            if let Ok((key, _)) = hkcu.create_subkey(key_path) {
                if key
                    .set_value("", &chromium_manifest_path.to_string_lossy().to_string())
                    .is_ok()
                {
                    registered.push(browser.to_string());
                }
            }
        }
        if let Ok((key, _)) =
            hkcu.create_subkey(r"Software\Mozilla\NativeMessagingHosts\com.vortexdl.host")
        {
            if key
                .set_value("", &firefox_manifest_path.to_string_lossy().to_string())
                .is_ok()
            {
                registered.push("Firefox".to_string());
            }
        }
    }
    Ok(registered)
}

/// Registers VortexDL as the Windows HKCU `magnet:` URL protocol handler.
pub fn register_magnet_protocol_handler(gui_exe: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let exe_str = gui_exe.to_string_lossy();
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (root, _) = hkcu.create_subkey(r"Software\Classes\magnet")?;
        root.set_value("", &"URL:Magnet Protocol")?;
        root.set_value("URL Protocol", &"")?;

        let (icon_key, _) = hkcu.create_subkey(r"Software\Classes\magnet\DefaultIcon")?;
        icon_key.set_value("", &format!("\"{exe_str}\",0"))?;

        let (cmd_key, _) = hkcu.create_subkey(r"Software\Classes\magnet\shell\open\command")?;
        cmd_key.set_value("", &format!("\"{exe_str}\" \"%1\""))?;
    }
    #[cfg(not(windows))]
    {
        let _ = gui_exe;
    }
    Ok(())
}

pub fn is_magnet_protocol_registered(gui_exe: &Path) -> bool {
    #[cfg(windows)]
    {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let Ok(cmd_key) = hkcu.open_subkey(r"Software\Classes\magnet\shell\open\command") else {
            return false;
        };
        let Ok(val) = cmd_key.get_value::<String, _>("") else {
            return false;
        };
        let expected_exe = gui_exe.to_string_lossy().to_ascii_lowercase();
        val.to_ascii_lowercase().contains(&expected_exe)
    }
    #[cfg(not(windows))]
    {
        let _ = gui_exe;
        false
    }
}
