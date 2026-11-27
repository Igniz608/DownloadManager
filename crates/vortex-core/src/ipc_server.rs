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
use std::path::{Path, PathBuf};
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

pub const DEFAULT_CHROME_EXTENSION_ID: &str = "cpkkkcgchpbpjkdieckdkgdgadfbello";
pub const EDGE_STORE_EXTENSION_ID: &str = "kgkaideidlhnmjneefpgilmifehcncni";
pub const EDGE_STORE_UPDATE_URL: &str = "https://edge.microsoft.com/extensionwebstorebase/v1/crx";
pub const EDGE_STORE_LISTING_URL: &str =
    "https://microsoftedge.microsoft.com/addons/detail/kgkaideidlhnmjneefpgilmifehcncni";
pub const FIREFOX_EXTENSION_ID: &str = "vortexdl@igniz608.dev";
pub const FIREFOX_SIGNED_XPI_NAME: &str = "vortexdl-firefox.xpi";

pub fn build_chromium_allowed_origins(chrome_extension_id: Option<&str>) -> Vec<String> {
    let effective_id = chrome_extension_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_CHROME_EXTENSION_ID);

    let mut origins = vec![format!("chrome-extension://{effective_id}/")];
    for extra_id in [EDGE_STORE_EXTENSION_ID, DEFAULT_CHROME_EXTENSION_ID] {
        let origin = format!("chrome-extension://{extra_id}/");
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    origins
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowserKind {
    Chrome,
    Edge,
    Brave,
    Vivaldi,
    Opera,
    Firefox,
}

impl BrowserKind {
    pub const ALL: [BrowserKind; 6] = [
        BrowserKind::Chrome,
        BrowserKind::Edge,
        BrowserKind::Brave,
        BrowserKind::Vivaldi,
        BrowserKind::Opera,
        BrowserKind::Firefox,
    ];

    pub fn id(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "chrome",
            BrowserKind::Edge => "edge",
            BrowserKind::Brave => "brave",
            BrowserKind::Vivaldi => "vivaldi",
            BrowserKind::Opera => "opera",
            BrowserKind::Firefox => "firefox",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "Google Chrome",
            BrowserKind::Edge => "Microsoft Edge",
            BrowserKind::Brave => "Brave",
            BrowserKind::Vivaldi => "Vivaldi",
            BrowserKind::Opera => "Opera / Opera GX",
            BrowserKind::Firefox => "Firefox / Zen",
        }
    }

    pub fn is_chromium(self) -> bool {
        !matches!(self, BrowserKind::Firefox)
    }

    pub fn extensions_url(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "chrome://extensions",
            BrowserKind::Edge => "edge://extensions",
            BrowserKind::Brave => "brave://extensions",
            BrowserKind::Vivaldi => "vivaldi://extensions",
            BrowserKind::Opera => "opera://extensions",
            BrowserKind::Firefox => "about:debugging#/runtime/this-firefox",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "chrome" | "google-chrome" => Some(BrowserKind::Chrome),
            "edge" | "msedge" | "microsoft-edge" => Some(BrowserKind::Edge),
            "brave" => Some(BrowserKind::Brave),
            "vivaldi" => Some(BrowserKind::Vivaldi),
            "opera" | "operagx" | "opera-gx" => Some(BrowserKind::Opera),
            "firefox" | "mozilla-firefox" | "zen" | "zen-browser" | "floorp" | "librewolf"
            | "waterfox" => Some(BrowserKind::Firefox),
            _ => None,
        }
    }

    pub fn parse_list(spec: &str) -> Vec<Self> {
        let trimmed = spec.trim();
        if trimmed.eq_ignore_ascii_case("all") {
            return Self::ALL.to_vec();
        }
        let mut out = Vec::new();
        for token in trimmed.split(',') {
            if let Some(kind) = Self::from_id(token) {
                if !out.contains(&kind) {
                    out.push(kind);
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct DetectedBrowser {
    pub kind: BrowserKind,
    pub exe_path: PathBuf,
}

#[cfg(windows)]
fn lookup_app_path(exe_name: &str) -> Option<PathBuf> {
    let subkey = format!(r"Software\Microsoft\Windows\CurrentVersion\App Paths\{exe_name}");
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let hive = RegKey::predef(root);
        if let Ok(key) = hive.open_subkey(&subkey) {
            if let Ok(raw) = key.get_value::<String, _>("") {
                let cleaned = raw.trim().trim_matches('"');
                let p = PathBuf::from(cleaned);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

pub fn detect_installed_browsers() -> Vec<DetectedBrowser> {
    let mut found = Vec::new();
    #[cfg(windows)]
    {
        let pf = std::env::var_os("ProgramFiles").map(PathBuf::from);
        let pf86 = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from);
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);

        let specs: &[(BrowserKind, &[&str], &[&str])] = &[
            (
                BrowserKind::Chrome,
                &["chrome.exe"],
                &[
                    r"Google\Chrome\Application\chrome.exe",
                ],
            ),
            (
                BrowserKind::Edge,
                &["msedge.exe"],
                &[
                    r"Microsoft\Edge\Application\msedge.exe",
                ],
            ),
            (
                BrowserKind::Brave,
                &["brave.exe"],
                &[
                    r"BraveSoftware\Brave-Browser\Application\brave.exe",
                ],
            ),
            (
                BrowserKind::Vivaldi,
                &["vivaldi.exe"],
                &[
                    r"Vivaldi\Application\vivaldi.exe",
                ],
            ),
            (
                BrowserKind::Opera,
                &["opera.exe", "launcher.exe"],
                &[
                    r"Programs\Opera\opera.exe",
                    r"Programs\Opera GX\opera.exe",
                    r"Opera\opera.exe",
                ],
            ),
            (
                BrowserKind::Firefox,
                &[
                    "firefox.exe",
                    "zen.exe",
                    "floorp.exe",
                    "librewolf.exe",
                    "waterfox.exe",
                ],
                &[
                    r"Mozilla Firefox\firefox.exe",
                    r"Zen Browser\zen.exe",
                    r"Programs\Zen Browser\zen.exe",
                    r"Ablaze Floorp\floorp.exe",
                    r"LibreWolf\librewolf.exe",
                    r"Waterfox\waterfox.exe",
                ],
            ),
        ];

        for &(kind, app_names, rel_paths) in specs {
            let mut exe_path = None;
            for name in app_names {
                if let Some(p) = lookup_app_path(name) {
                    exe_path = Some(p);
                    break;
                }
            }
            if exe_path.is_none() {
                'outer: for base in [&pf, &pf86, &local].into_iter().flatten() {
                    for rel in rel_paths {
                        let candidate = base.join(rel);
                        if candidate.is_file() {
                            exe_path = Some(candidate);
                            break 'outer;
                        }
                    }
                }
            }
            if let Some(exe_path) = exe_path {
                found.push(DetectedBrowser { kind, exe_path });
            }
        }
    }
    found
}

pub fn resolve_extension_dir(anchor_exe: Option<&Path>) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(exe) = anchor_exe {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("extension"));
            if let Some(p1) = dir.parent() {
                candidates.push(p1.join("extension"));
                if let Some(p2) = p1.parent() {
                    candidates.push(p2.join("extension"));
                }
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("extension"));
    }
    candidates
        .into_iter()
        .find(|dir| dir.join("manifest.json").is_file())
}

pub fn resolve_firefox_xpi_path(anchor_exe: Option<&Path>) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(exe) = anchor_exe {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(FIREFOX_SIGNED_XPI_NAME));
            candidates.push(dir.join("installer").join(FIREFOX_SIGNED_XPI_NAME));
            if let Some(p1) = dir.parent() {
                candidates.push(p1.join(FIREFOX_SIGNED_XPI_NAME));
                candidates.push(p1.join("installer").join(FIREFOX_SIGNED_XPI_NAME));
                if let Some(p2) = p1.parent() {
                    candidates.push(p2.join(FIREFOX_SIGNED_XPI_NAME));
                    candidates.push(p2.join("installer").join(FIREFOX_SIGNED_XPI_NAME));
                }
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(FIREFOX_SIGNED_XPI_NAME));
        candidates.push(cwd.join("installer").join(FIREFOX_SIGNED_XPI_NAME));
    }
    candidates.into_iter().find(|p| p.is_file())
}

pub fn open_firefox_xpi_installer(anchor_exe: Option<&Path>) -> Result<PathBuf> {
    let Some(xpi_path) = resolve_firefox_xpi_path(anchor_exe) else {
        anyhow::bail!("Pacote assinado {FIREFOX_SIGNED_XPI_NAME} não encontrado");
    };
    let installed = detect_installed_browsers();
    if let Some(firefox) = installed
        .into_iter()
        .find(|b| b.kind == BrowserKind::Firefox)
    {
        std::process::Command::new(&firefox.exe_path)
            .arg(&xpi_path)
            .spawn()?;
        return Ok(xpi_path);
    }
    anyhow::bail!("Firefox / Zen Browser não encontrado no sistema");
}

pub fn open_browser_extensions_page(kind: BrowserKind) -> Result<()> {
    let installed = detect_installed_browsers();
    if let Some(browser) = installed.into_iter().find(|b| b.kind == kind) {
        std::process::Command::new(&browser.exe_path)
            .arg(kind.extensions_url())
            .spawn()?;
        return Ok(());
    }
    anyhow::bail!("{} não encontrado no sistema", kind.display_name());
}

pub fn open_edge_store_listing() -> Result<()> {
    let installed = detect_installed_browsers();
    if let Some(edge) = installed.into_iter().find(|b| b.kind == BrowserKind::Edge) {
        std::process::Command::new(&edge.exe_path)
            .arg(EDGE_STORE_LISTING_URL)
            .spawn()?;
        return Ok(());
    }
    #[cfg(windows)]
    {
        std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", EDGE_STORE_LISTING_URL])
            .spawn()?;
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("Microsoft Edge não encontrado no sistema")
    }
}

fn edge_profile_preferences_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            let user_data = local.join("Microsoft").join("Edge").join("User Data");
            if let Ok(entries) = std::fs::read_dir(&user_data) {
                for entry in entries.flatten() {
                    let pref = entry.path().join("Preferences");
                    if pref.is_file() {
                        paths.push(pref);
                    }
                }
            }
        }
    }
    paths
}

fn preferences_has_external_uninstall(raw: &str, ext_id: &str) -> bool {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    val.get("extensions")
        .and_then(|e| e.get("external_uninstalls"))
        .and_then(|arr| arr.as_array())
        .is_some_and(|arr| {
            arr.iter()
                .any(|item| item.as_str() == Some(ext_id))
        })
}

fn remove_external_uninstall_from_preferences(raw: &str, ext_id: &str) -> Option<String> {
    let mut val = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    let arr = val
        .get_mut("extensions")?
        .get_mut("external_uninstalls")?
        .as_array_mut()?;
    let before = arr.len();
    arr.retain(|item| item.as_str() != Some(ext_id));
    if arr.len() == before {
        return None;
    }
    serde_json::to_string(&val).ok()
}

pub fn is_edge_process_running() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let Ok(out) = std::process::Command::new("tasklist.exe")
            .args(["/FI", "IMAGENAME eq msedge.exe", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        else {
            return false;
        };
        let stdout = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
        stdout.contains("msedge.exe")
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn is_edge_store_extension_suppressed() -> bool {
    edge_profile_preferences_paths().into_iter().any(|pref| {
        std::fs::read_to_string(&pref)
            .is_ok_and(|raw| preferences_has_external_uninstall(&raw, EDGE_STORE_EXTENSION_ID))
    })
}

pub fn clear_edge_store_external_uninstall_if_closed() -> bool {
    if is_edge_process_running() {
        return false;
    }
    let mut cleared = false;
    for pref in edge_profile_preferences_paths() {
        let Ok(raw) = std::fs::read_to_string(&pref) else {
            continue;
        };
        if let Some(updated) = remove_external_uninstall_from_preferences(&raw, EDGE_STORE_EXTENSION_ID) {
            if std::fs::write(&pref, updated).is_ok() {
                cleared = true;
            }
        }
    }
    cleared
}

pub fn register_edge_store_extension() -> Result<()> {
    #[cfg(windows)]
    {
        let _ = clear_edge_store_external_uninstall_if_closed();
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key_path = format!(r"Software\Microsoft\Edge\Extensions\{EDGE_STORE_EXTENSION_ID}");
        let (key, _) = hkcu.create_subkey(&key_path)?;
        key.set_value("update_url", &EDGE_STORE_UPDATE_URL)?;
    }
    Ok(())
}

pub fn is_edge_store_extension_registered() -> bool {
    #[cfg(windows)]
    {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key_path = format!(r"Software\Microsoft\Edge\Extensions\{EDGE_STORE_EXTENSION_ID}");
        let Ok(key) = hkcu.open_subkey(&key_path) else {
            return false;
        };
        key.get_value::<String, _>("update_url")
            .is_ok_and(|v| v.trim().eq_ignore_ascii_case(EDGE_STORE_UPDATE_URL))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Registers Chrome, Edge, Brave, Vivaldi, Opera, and Firefox Native Messaging manifests in Windows HKCU Registry.
pub fn register_browser_native_host(
    native_host_exe: &Path,
    chrome_extension_id: Option<&str>,
) -> Result<Vec<String>> {
    register_browser_native_host_for(native_host_exe, chrome_extension_id, None)
}

pub fn register_browser_native_host_for(
    native_host_exe: &Path,
    chrome_extension_id: Option<&str>,
    browsers: Option<&[BrowserKind]>,
) -> Result<Vec<String>> {
    let selected: Vec<BrowserKind> = match browsers {
        Some(list) if !list.is_empty() => list.to_vec(),
        Some(_) => return Ok(Vec::new()),
        None => BrowserKind::ALL.to_vec(),
    };
    let wants_chromium = selected.iter().any(|b| b.is_chromium());
    let wants_firefox = selected.contains(&BrowserKind::Firefox);
    let wants_edge = selected.contains(&BrowserKind::Edge);

    let data_dir = app_data_dir().join("native-host");
    std::fs::create_dir_all(&data_dir)?;

    let allowed_origins = build_chromium_allowed_origins(chrome_extension_id);

    let chromium_manifest_path = data_dir.join("com.vortexdl.host.chromium.json");
    if wants_chromium {
        let chromium_manifest = serde_json::json!({
            "name": "com.vortexdl.host",
            "description": "VortexDL Native Messaging Host",
            "path": native_host_exe.to_string_lossy(),
            "type": "stdio",
            "allowed_origins": allowed_origins
        });
        std::fs::write(
            &chromium_manifest_path,
            serde_json::to_string_pretty(&chromium_manifest)?,
        )?;
    }

    let firefox_manifest_path = data_dir.join("com.vortexdl.host.firefox.json");
    if wants_firefox {
        let firefox_manifest = serde_json::json!({
            "name": "com.vortexdl.host",
            "description": "VortexDL Native Messaging Host",
            "path": native_host_exe.to_string_lossy(),
            "type": "stdio",
            "allowed_extensions": [FIREFOX_EXTENSION_ID]
        });
        std::fs::write(
            &firefox_manifest_path,
            serde_json::to_string_pretty(&firefox_manifest)?,
        )?;
    }

    if wants_edge {
        register_edge_store_extension()?;
    }

    let mut registered = Vec::new();
    #[cfg(windows)]
    {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let chromium_key_for = |kind: BrowserKind| -> Option<&'static str> {
            match kind {
                BrowserKind::Chrome | BrowserKind::Opera => {
                    Some(r"Software\Google\Chrome\NativeMessagingHosts\com.vortexdl.host")
                }
                BrowserKind::Edge => {
                    Some(r"Software\Microsoft\Edge\NativeMessagingHosts\com.vortexdl.host")
                }
                BrowserKind::Brave => {
                    Some(r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.vortexdl.host")
                }
                BrowserKind::Vivaldi => {
                    Some(r"Software\Vivaldi\NativeMessagingHosts\com.vortexdl.host")
                }
                BrowserKind::Firefox => None,
            }
        };

        for kind in selected {
            if let Some(key_path) = chromium_key_for(kind) {
                if let Ok((key, _)) = hkcu.create_subkey(key_path) {
                    if key
                        .set_value("", &chromium_manifest_path.to_string_lossy().to_string())
                        .is_ok()
                    {
                        registered.push(kind.display_name().to_string());
                    }
                }
            } else if kind == BrowserKind::Firefox {
                if let Ok((key, _)) =
                    hkcu.create_subkey(r"Software\Mozilla\NativeMessagingHosts\com.vortexdl.host")
                {
                    if key
                        .set_value("", &firefox_manifest_path.to_string_lossy().to_string())
                        .is_ok()
                    {
                        registered.push(kind.display_name().to_string());
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (&chromium_manifest_path, &firefox_manifest_path, &selected);
    }
    Ok(registered)
}

pub fn unregister_browser_integration() {
    #[cfg(windows)]
    {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        for key_path in [
            r"Software\Google\Chrome\NativeMessagingHosts\com.vortexdl.host",
            r"Software\Microsoft\Edge\NativeMessagingHosts\com.vortexdl.host",
            r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.vortexdl.host",
            r"Software\Vivaldi\NativeMessagingHosts\com.vortexdl.host",
            r"Software\Mozilla\NativeMessagingHosts\com.vortexdl.host",
        ] {
            let _ = hkcu.delete_subkey_all(key_path);
        }
        let _ = hkcu.delete_subkey_all(format!(
            r"Software\Microsoft\Edge\Extensions\{EDGE_STORE_EXTENSION_ID}"
        ));
        let _ = hkcu.delete_subkey_all(r"Software\Classes\magnet");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_kind_parses_comma_lists_and_all() {
        assert_eq!(
            BrowserKind::parse_list("chrome, Edge ,opera-gx,zen,unknown,chrome"),
            vec![
                BrowserKind::Chrome,
                BrowserKind::Edge,
                BrowserKind::Opera,
                BrowserKind::Firefox
            ]
        );
        assert_eq!(BrowserKind::parse_list("all").len(), BrowserKind::ALL.len());
    }

    #[test]
    fn default_extension_id_is_valid_chromium_id() {
        assert_eq!(DEFAULT_CHROME_EXTENSION_ID.len(), 32);
        assert!(DEFAULT_CHROME_EXTENSION_ID
            .bytes()
            .all(|b| (b'a'..=b'p').contains(&b)));
    }

    #[test]
    fn edge_store_extension_id_is_valid_chromium_id() {
        assert_eq!(EDGE_STORE_EXTENSION_ID.len(), 32);
        assert!(EDGE_STORE_EXTENSION_ID
            .bytes()
            .all(|b| (b'a'..=b'p').contains(&b)));
    }

    #[test]
    fn chromium_allowed_origins_includes_unpacked_and_edge_store_without_duplicates() {
        let origins = build_chromium_allowed_origins(None);
        assert_eq!(
            origins,
            vec![
                format!("chrome-extension://{DEFAULT_CHROME_EXTENSION_ID}/"),
                format!("chrome-extension://{EDGE_STORE_EXTENSION_ID}/"),
            ]
        );

        let edge_only_first = build_chromium_allowed_origins(Some(EDGE_STORE_EXTENSION_ID));
        assert_eq!(
            edge_only_first,
            vec![
                format!("chrome-extension://{EDGE_STORE_EXTENSION_ID}/"),
                format!("chrome-extension://{DEFAULT_CHROME_EXTENSION_ID}/"),
            ]
        );
    }

    #[test]
    fn resolve_firefox_xpi_path_finds_installer_bundle() {
        let dir = std::env::temp_dir().join(format!("vortex_xpi_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let fake_exe = dir.join("vortex-gui.exe");
        let xpi = dir.join(FIREFOX_SIGNED_XPI_NAME);
        std::fs::write(&fake_exe, b"").expect("write exe");
        std::fs::write(&xpi, b"PK").expect("write xpi");
        let found = resolve_firefox_xpi_path(Some(&fake_exe));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, Some(xpi));
    }

    #[test]
    fn edge_external_uninstall_cleanup_preserves_other_ids() {
        let raw = r#"{"extensions":{"external_uninstalls":["otherid","kgkaideidlhnmjneefpgilmifehcncni"],"other":1}}"#;
        assert!(preferences_has_external_uninstall(raw, EDGE_STORE_EXTENSION_ID));
        let updated = remove_external_uninstall_from_preferences(raw, EDGE_STORE_EXTENSION_ID)
            .expect("should update");
        assert!(!preferences_has_external_uninstall(&updated, EDGE_STORE_EXTENSION_ID));
        assert!(preferences_has_external_uninstall(&updated, "otherid"));
        assert!(updated.contains("\"other\":1"));
    }
}
