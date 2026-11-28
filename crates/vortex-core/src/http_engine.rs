use crate::model::{DownloadId, DownloadItem, DownloadStatus, FileCategory, SegmentState};
use crate::package_grabber::{fallback_filename_from_url, sanitize_filename, url_decode_lossy};
use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use md5::{Digest as Md5Digest, Md5};
use parking_lot::Mutex;
use reqwest::Client;
use reqwest::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    HeaderMap, HeaderName, HeaderValue, LAST_MODIFIED, RANGE, USER_AGENT,
};
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct HtmlPageDetected {
    pub final_url: String,
    pub content_type: String,
}

impl fmt::Display for HtmlPageDetected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "URL retornou página HTML ({}) em vez de arquivo direto: {}",
            self.content_type, self.final_url
        )
    }
}

impl std::error::Error for HtmlPageDetected {}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub final_url: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub has_content_disposition: bool,
    pub total_bytes: Option<u64>,
    pub supports_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub fn build_http_client() -> Result<Client> {
    let mut default_headers = HeaderMap::new();
    default_headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        ),
    );
    Ok(Client::builder()
        .default_headers(default_headers)
        .connect_timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?)
}

pub fn apply_custom_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &HashMap<String, String>,
) -> reqwest::RequestBuilder {
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            builder = builder.header(name, val);
        }
    }
    builder
}

pub fn parse_content_disposition(header_val: &str) -> Option<String> {
    // Check filename*=UTF-8''...
    for part in header_val.split(';') {
        let trimmed = part.trim();
        if let Some(rest) = trimmed
            .strip_prefix("filename*=")
            .or_else(|| trimmed.strip_prefix("FILENAME*="))
        {
            let value = rest.trim().trim_matches('"');
            let encoded = if let Some((_, rhs)) = value.split_once("''") {
                rhs
            } else {
                value
            };
            let decoded = url_decode_lossy(encoded);
            let clean = sanitize_filename(&decoded);
            if !clean.is_empty() {
                return Some(clean);
            }
        }
    }
    for part in header_val.split(';') {
        let trimmed = part.trim();
        if let Some(rest) = trimmed
            .strip_prefix("filename=")
            .or_else(|| trimmed.strip_prefix("FILENAME="))
        {
            let clean = sanitize_filename(rest);
            if !clean.is_empty() {
                return Some(clean);
            }
        }
    }
    None
}

pub async fn probe_url(
    client: &Client,
    url: &str,
    headers: &HashMap<String, String>,
) -> Result<ProbeResult> {
    let head_req = apply_custom_headers(client.head(url), headers);
    if let Ok(resp) = head_req.send().await {
        if resp.status().is_success() {
            let final_url = resp.url().to_string();
            let raw_cd = resp
                .headers()
                .get(CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok());
            let has_content_disposition = raw_cd.is_some();
            let cd = raw_cd.and_then(parse_content_disposition);
            let content_type = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase());
            let total_bytes = resp
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|&n| n > 0);
            let supports_ranges = resp
                .headers()
                .get(ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.eq_ignore_ascii_case("bytes"))
                .unwrap_or(false);
            let etag = resp
                .headers()
                .get(ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let last_modified = resp
                .headers()
                .get(LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);

            if total_bytes.is_some() || cd.is_some() {
                return Ok(ProbeResult {
                    final_url,
                    filename: cd,
                    content_type,
                    has_content_disposition,
                    total_bytes,
                    supports_ranges,
                    etag,
                    last_modified,
                });
            }
        }
    }

    // Fallback: GET with Range: bytes=0-0
    let get_req = apply_custom_headers(client.get(url), headers).header(RANGE, "bytes=0-0");
    let resp = get_req.send().await?;
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(anyhow!("HTTP {} ao analisar URL", resp.status()));
    }

    let final_url = resp.url().to_string();
    let raw_cd = resp
        .headers()
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok());
    let has_content_disposition = raw_cd.is_some();
    let cd = raw_cd.and_then(parse_content_disposition);
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase());

    let mut supports_ranges = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut total_bytes = None;

    if let Some(cr) = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
    {
        if let Some((_, total_str)) = cr.rsplit_once('/') {
            if let Ok(total) = total_str.trim().parse::<u64>() {
                total_bytes = Some(total);
                supports_ranges = true;
            }
        }
    }

    if total_bytes.is_none() {
        total_bytes = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n > 0);
    }

    let etag = resp
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let last_modified = resp
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    Ok(ProbeResult {
        final_url,
        filename: cd,
        content_type,
        has_content_disposition,
        total_bytes,
        supports_ranges,
        etag,
        last_modified,
    })
}

pub type DestinationResolver =
    Arc<dyn Fn(DownloadId, &Path, &str, bool) -> PathBuf + Send + Sync>;

pub fn part_file_path(file_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.vortexpart", file_path.display()))
}

pub fn paths_conflict(a: &Path, b: &Path) -> bool {
    path_collision_key(a) == path_collision_key(b)
}

fn path_collision_key(path: &Path) -> String {
    let mut normalized = PathBuf::new();
    for comp in path.components() {
        normalized.push(comp);
    }
    let s = normalized.to_string_lossy();
    #[cfg(windows)]
    {
        s.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s.into_owned()
    }
}

fn split_filename_for_counter(filename: &str) -> (&str, Option<&str>) {
    for compound_ext in ["tar.gz", "tar.bz2", "tar.xz", "tar.zst"] {
        let suffix = format!(".{compound_ext}");
        if filename.len() > suffix.len()
            && filename[filename.len() - suffix.len()..].eq_ignore_ascii_case(&suffix)
        {
            let stem = &filename[..filename.len() - suffix.len()];
            if !stem.is_empty() {
                let ext = &filename[filename.len() - compound_ext.len()..];
                return (stem, Some(ext));
            }
        }
    }

    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    let ext = path.extension().and_then(|e| e.to_str());
    (stem, ext)
}

pub fn unique_destination_path_with_reserved(
    dir: &Path,
    filename: &str,
    reserved_paths: &[PathBuf],
) -> PathBuf {
    let clean_name = if filename.trim().is_empty() {
        "download.bin"
    } else {
        filename
    };
    let reserved_keys: std::collections::HashSet<String> =
        reserved_paths.iter().map(|p| path_collision_key(p)).collect();
    let is_taken = |candidate: &Path| -> bool {
        candidate.exists()
            || part_file_path(candidate).exists()
            || reserved_keys.contains(&path_collision_key(candidate))
    };

    let candidate = dir.join(clean_name);
    if !is_taken(&candidate) {
        return candidate;
    }

    let (stem, ext) = split_filename_for_counter(clean_name);
    for i in 1..10_000 {
        let next_name = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let next = dir.join(next_name);
        if !is_taken(&next) {
            return next;
        }
    }
    dir.join(format!("{clean_name}.{}", uuid::Uuid::new_v4()))
}

pub fn unique_destination_path(dir: &Path, filename: &str) -> PathBuf {
    unique_destination_path_with_reserved(dir, filename, &[])
}

#[derive(Clone)]
pub struct BandwidthLimiter {
    limit_bps: Arc<AtomicU64>,
    state: Arc<Mutex<(Instant, f64)>>,
}

impl BandwidthLimiter {
    pub fn new(limit_kbps: u64) -> Self {
        Self {
            limit_bps: Arc::new(AtomicU64::new(limit_kbps * 1024)),
            state: Arc::new(Mutex::new((Instant::now(), 0.0))),
        }
    }

    pub fn set_limit_kbps(&self, limit_kbps: u64) {
        self.limit_bps.store(limit_kbps * 1024, Ordering::Relaxed);
    }

    pub async fn consume(&self, bytes: usize) {
        let rate = self.limit_bps.load(Ordering::Relaxed);
        if rate == 0 || bytes == 0 {
            return;
        }
        let wait_dur = {
            let mut guard = self.state.lock();
            let now = Instant::now();
            let elapsed = now.duration_since(guard.0).as_secs_f64();
            guard.0 = now;
            let rate_f = rate as f64;
            guard.1 = (guard.1 + elapsed * rate_f).min(rate_f);
            guard.1 -= bytes as f64;
            if guard.1 < 0.0 {
                let deficit = -guard.1;
                Some(Duration::from_secs_f64((deficit / rate_f).min(1.0)))
            } else {
                None
            }
        };
        if let Some(d) = wait_dur {
            tokio::time::sleep(d).await;
        }
    }
}

pub struct DownloadCancelHandle {
    pub paused: AtomicBool,
    pub cancelled: AtomicBool,
    pub delete_on_cancel: AtomicBool,
    pub notify: Notify,
}

impl DownloadCancelHandle {
    pub fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            delete_on_cancel: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_stopped(&self) -> bool {
        self.paused.load(Ordering::SeqCst) || self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct ProgressSnapshot {
    pub id: DownloadId,
    pub status: Option<DownloadStatus>,
    pub filename: Option<String>,
    pub file_path: Option<PathBuf>,
    pub category: Option<FileCategory>,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub speed_bps: u64,
    pub eta_seconds: Option<u64>,
    pub supports_ranges: Option<bool>,
    pub segments: Option<Vec<SegmentState>>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub checksum_verified: Option<bool>,
}

pub fn build_initial_segments(total_bytes: u64, connections: usize) -> Vec<SegmentState> {
    if total_bytes == 0 {
        return Vec::new();
    }
    let min_chunk = 256 * 1024u64; // 256 KiB minimum per segment
    let max_by_size = (total_bytes / min_chunk).max(1) as usize;
    let count = connections.clamp(1, 32).min(max_by_size);
    let chunk = total_bytes / count as u64;
    let mut segments = Vec::with_capacity(count);
    let mut start = 0u64;
    for i in 0..count {
        let end = if i + 1 == count {
            total_bytes - 1
        } else {
            start + chunk - 1
        };
        segments.push(SegmentState {
            id: i,
            start,
            end,
            downloaded: 0,
            done: false,
        });
        start = end + 1;
    }
    segments
}

fn default_destination_resolver() -> DestinationResolver {
    Arc::new(|_id, dir, filename, _keep_own_part| unique_destination_path(dir, filename))
}

pub async fn run_http_download(
    client: Client,
    item: DownloadItem,
    categorize_subfolders: bool,
    limiter: BandwidthLimiter,
    cancel: Arc<DownloadCancelHandle>,
    on_progress: Arc<dyn Fn(ProgressSnapshot) + Send + Sync>,
) -> Result<DownloadItem> {
    run_http_download_with_options(
        client,
        item,
        categorize_subfolders,
        false,
        limiter,
        cancel,
        on_progress,
    )
    .await
}

pub async fn run_http_download_with_options(
    client: Client,
    item: DownloadItem,
    categorize_subfolders: bool,
    allow_html: bool,
    limiter: BandwidthLimiter,
    cancel: Arc<DownloadCancelHandle>,
    on_progress: Arc<dyn Fn(ProgressSnapshot) + Send + Sync>,
) -> Result<DownloadItem> {
    run_http_download_with_options_and_resolver(
        client,
        item,
        categorize_subfolders,
        allow_html,
        limiter,
        cancel,
        default_destination_resolver(),
        on_progress,
    )
    .await
}

pub async fn run_http_download_with_options_and_resolver(
    client: Client,
    item: DownloadItem,
    categorize_subfolders: bool,
    allow_html: bool,
    limiter: BandwidthLimiter,
    cancel: Arc<DownloadCancelHandle>,
    resolve_destination: DestinationResolver,
    on_progress: Arc<dyn Fn(ProgressSnapshot) + Send + Sync>,
) -> Result<DownloadItem> {
    let mut current = item;

    // 1. Probe if we don't know total_bytes or we're starting fresh
    let needs_probe = current.total_bytes.is_none() || current.segments.is_empty();
    let mut download_url = current.url.clone();

    if needs_probe {
        on_progress(ProgressSnapshot {
            id: current.id,
            status: Some(DownloadStatus::Probing),
            filename: None,
            file_path: None,
            category: None,
            total_bytes: None,
            downloaded_bytes: current.downloaded_bytes,
            speed_bps: 0,
            eta_seconds: None,
            supports_ranges: None,
            segments: None,
            etag: None,
            last_modified: None,
            checksum_verified: None,
        });

        let probe = probe_url(&client, &current.url, &current.headers).await?;
        if !allow_html
            && !probe.has_content_disposition
            && current.downloaded_bytes == 0
            && current.checksum_expected.is_none()
        {
            if let Some(ref ct) = probe.content_type {
                if ct == "text/html" || ct == "application/xhtml+xml" {
                    return Err(anyhow::Error::new(HtmlPageDetected {
                        final_url: probe.final_url,
                        content_type: ct.clone(),
                    }));
                }
            }
        }
        download_url = probe.final_url.clone();

        if current.filename.is_empty() || current.filename == "download.bin" {
            current.filename = probe
                .filename
                .clone()
                .unwrap_or_else(|| fallback_filename_from_url(&probe.final_url));
        } else if let Some(cd_name) = probe.filename {
            // Prefer Content-Disposition when filename was auto-derived from URL
            if current.downloaded_bytes == 0 {
                current.filename = cd_name;
            }
        }
        current.filename = sanitize_filename(&current.filename);
        current.category = FileCategory::from_filename(&current.filename);

        let target_dir = if categorize_subfolders {
            let base_str = current.save_dir.to_string_lossy();
            if base_str.ends_with(current.category.subfolder()) {
                current.save_dir.clone()
            } else {
                current.save_dir.join(current.category.subfolder())
            }
        } else {
            current.save_dir.clone()
        };
        tokio::fs::create_dir_all(&target_dir).await?;
        let keep_own_part = (current.downloaded_bytes > 0
            || !current.segments.is_empty()
            || current.total_bytes.is_some())
            && part_file_path(&current.file_path).exists();
        if !keep_own_part || current.file_path.exists() {
            current.save_dir = target_dir.clone();
            current.file_path = resolve_destination(
                current.id,
                &target_dir,
                &current.filename,
                keep_own_part,
            );
            if let Some(name) = current.file_path.file_name().and_then(|n| n.to_str()) {
                current.filename = name.to_string();
            }
        }

        current.total_bytes = probe.total_bytes;
        current.supports_ranges = probe.supports_ranges;
        current.etag = probe.etag;
        current.last_modified = probe.last_modified;

        if let Some(total) = current.total_bytes {
            let conns = if current.supports_ranges {
                current.connections.max(1)
            } else {
                1
            };
            if current.segments.is_empty() {
                current.segments = build_initial_segments(total, conns);
            }
        }
    } else {
        tokio::fs::create_dir_all(&current.save_dir).await?;
        if !part_file_path(&current.file_path).exists() || current.file_path.exists() {
            current.file_path = resolve_destination(
                current.id,
                &current.save_dir,
                &current.filename,
                false,
            );
            if let Some(name) = current.file_path.file_name().and_then(|n| n.to_str()) {
                current.filename = name.to_string();
            }
        }
    }

    let part_path = part_file_path(&current.file_path);
    {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&part_path)
            .with_context(|| format!("Falha ao abrir arquivo temporário {}", part_path.display()))?;
        if let Some(total) = current.total_bytes {
            if file.metadata()?.len() != total {
                file.set_len(total)?;
            }
        }
    }

    current.status = DownloadStatus::Downloading;
    on_progress(ProgressSnapshot {
        id: current.id,
        status: Some(DownloadStatus::Downloading),
        filename: Some(current.filename.clone()),
        file_path: Some(current.file_path.clone()),
        category: Some(current.category),
        total_bytes: current.total_bytes,
        downloaded_bytes: current.downloaded_bytes,
        speed_bps: 0,
        eta_seconds: None,
        supports_ranges: Some(current.supports_ranges),
        segments: Some(current.segments.clone()),
        etag: current.etag.clone(),
        last_modified: current.last_modified.clone(),
        checksum_verified: None,
    });

    if !current.supports_ranges || current.total_bytes.is_none() || current.segments.is_empty() {
        // Single-stream fallback (also handles servers without Content-Length or Range)
        let res = download_single_stream(
            &client,
            &download_url,
            &current,
            &part_path,
            &limiter,
            &cancel,
            &on_progress,
        )
        .await;
        match res {
            Ok((total_written, total_len)) => {
                current.downloaded_bytes = total_written;
                if total_len.is_some() {
                    current.total_bytes = total_len;
                }
            }
            Err(e) => {
                if cancel.paused.load(Ordering::SeqCst) {
                    current.status = DownloadStatus::Paused;
                    return Ok(current);
                }
                return Err(e);
            }
        }
    } else {
        // Multi-connection segmented engine with dynamic work-stealing
        let res = download_segmented_workstealing(
            &client,
            &download_url,
            &mut current,
            &part_path,
            &limiter,
            &cancel,
            &on_progress,
        )
        .await;
        if let Err(e) = res {
            if cancel.paused.load(Ordering::SeqCst) {
                current.status = DownloadStatus::Paused;
                current.speed_bps = 0;
                current.eta_seconds = None;
                return Ok(current);
            }
            return Err(e);
        }
    }

    if cancel.cancelled.load(Ordering::SeqCst) {
        let _ = tokio::fs::remove_file(&part_path).await;
        return Err(anyhow!("Download cancelado pelo usuário"));
    }
    if cancel.paused.load(Ordering::SeqCst) {
        current.status = DownloadStatus::Paused;
        current.speed_bps = 0;
        current.eta_seconds = None;
        return Ok(current);
    }

    // Move .vortexpart -> final destination without overwriting any file that appeared mid-download
    if current.file_path.exists() {
        let resolved = resolve_destination(current.id, &current.save_dir, &current.filename, false);
        current.file_path = resolved;
        if let Some(name) = current.file_path.file_name().and_then(|n| n.to_str()) {
            current.filename = name.to_string();
        }
    }
    tokio::fs::rename(&part_path, &current.file_path).await?;

    // Verify optional checksum
    if let Some(ref expected) = current.checksum_expected {
        let verified = verify_file_checksum(&current.file_path, expected).await?;
        current.checksum_verified = Some(verified);
        if !verified {
            return Err(anyhow!("Falha na verificação de checksum ({expected})"));
        }
    }

    current.status = DownloadStatus::Completed;
    current.speed_bps = 0;
    current.eta_seconds = Some(0);
    current.completed_at = Some(chrono::Utc::now());
    Ok(current)
}

async fn download_single_stream(
    client: &Client,
    url: &str,
    item: &DownloadItem,
    part_path: &Path,
    limiter: &BandwidthLimiter,
    cancel: &Arc<DownloadCancelHandle>,
    on_progress: &Arc<dyn Fn(ProgressSnapshot) + Send + Sync>,
) -> Result<(u64, Option<u64>)> {
    let req = apply_custom_headers(client.get(url), &item.headers);
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {} ao baixar arquivo", resp.status()));
    }
    let content_len = resp
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or(item.total_bytes);

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(part_path)?;

    let mut stream = resp.bytes_stream();
    let mut downloaded = 0u64;
    let mut last_tick = Instant::now();
    let mut bytes_since_tick = 0u64;

    loop {
        if cancel.is_stopped() {
            break;
        }
        let next = tokio::select! {
            chunk = stream.next() => chunk,
            _ = cancel.notify.notified() => None,
        };
        match next {
            Some(Ok(bytes)) => {
                limiter.consume(bytes.len()).await;
                file.write_all(&bytes)?;
                downloaded += bytes.len() as u64;
                bytes_since_tick += bytes.len() as u64;

                let elapsed = last_tick.elapsed();
                if elapsed >= Duration::from_millis(250) {
                    let speed = (bytes_since_tick as f64 / elapsed.as_secs_f64()) as u64;
                    let eta = content_len.and_then(|t| {
                        if speed > 0 && t > downloaded {
                            Some((t - downloaded) / speed)
                        } else {
                            None
                        }
                    });
                    on_progress(ProgressSnapshot {
                        id: item.id,
                        status: None,
                        filename: None,
                        file_path: None,
                        category: None,
                        total_bytes: content_len,
                        downloaded_bytes: downloaded,
                        speed_bps: speed,
                        eta_seconds: eta,
                        supports_ranges: None,
                        segments: None,
                        etag: None,
                        last_modified: None,
                        checksum_verified: None,
                    });
                    last_tick = Instant::now();
                    bytes_since_tick = 0;
                }
            }
            Some(Err(e)) => return Err(e.into()),
            None => break,
        }
    }
    file.flush()?;
    Ok((downloaded, content_len))
}

async fn download_segmented_workstealing(
    client: &Client,
    url: &str,
    item: &mut DownloadItem,
    part_path: &Path,
    limiter: &BandwidthLimiter,
    cancel: &Arc<DownloadCancelHandle>,
    on_progress: &Arc<dyn Fn(ProgressSnapshot) + Send + Sync>,
) -> Result<()> {
    let total_bytes = item.total_bytes.ok_or_else(|| anyhow!("Tamanho desconhecido"))?;
    let segments_shared = Arc::new(Mutex::new(item.segments.clone()));
    let active_workers = Arc::new(Mutex::new(vec![false; item.segments.len()]));
    let bytes_this_window = Arc::new(AtomicU64::new(0));
    let worker_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Pre-mark already completed segments
    let max_workers = item.connections.clamp(1, 32);
    let active_limit = Arc::new(AtomicUsize::new(max_workers));
    let throttled = Arc::new(AtomicBool::new(false));
    let url_arc = Arc::new(url.to_string());
    let headers_arc = Arc::new(item.headers.clone());
    let part_path_arc = Arc::new(part_path.to_path_buf());

    // Progress reporter task
    let done_flag = Arc::new(AtomicBool::new(false));
    let reporter_handle = {
        let done_flag = done_flag.clone();
        let segments_shared = segments_shared.clone();
        let bytes_this_window = bytes_this_window.clone();
        let on_progress = on_progress.clone();
        let id = item.id;
        tokio::spawn(async move {
            let mut last = Instant::now();
            let mut ema_speed: f64 = 0.0;
            while !done_flag.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let elapsed = last.elapsed().as_secs_f64().max(0.05);
                last = Instant::now();
                let delta = bytes_this_window.swap(0, Ordering::Relaxed);
                let inst_speed = delta as f64 / elapsed;
                ema_speed = if ema_speed <= 0.0 {
                    inst_speed
                } else {
                    ema_speed * 0.65 + inst_speed * 0.35
                };
                let segs = segments_shared.lock().clone();
                let downloaded: u64 = segs.iter().map(|s| s.downloaded).sum();
                let speed_bps = ema_speed.max(0.0) as u64;
                let eta = if speed_bps > 0 && total_bytes > downloaded {
                    Some((total_bytes - downloaded) / speed_bps)
                } else {
                    None
                };
                on_progress(ProgressSnapshot {
                    id,
                    status: None,
                    filename: None,
                    file_path: None,
                    category: None,
                    total_bytes: Some(total_bytes),
                    downloaded_bytes: downloaded,
                    speed_bps,
                    eta_seconds: eta,
                    supports_ranges: None,
                    segments: Some(segs),
                    etag: None,
                    last_modified: None,
                    checksum_verified: None,
                });
            }
        })
    };

    let mut handles = Vec::new();
    for worker_idx in 0..max_workers {
        let client = client.clone();
        let url = url_arc.clone();
        let headers = headers_arc.clone();
        let part_path = part_path_arc.clone();
        let segments = segments_shared.clone();
        let active_workers = active_workers.clone();
        let active_limit = active_limit.clone();
        let throttled = throttled.clone();
        let limiter = limiter.clone();
        let cancel = cancel.clone();
        let bytes_window = bytes_this_window.clone();
        let worker_error = worker_error.clone();

        handles.push(tokio::spawn(async move {
            loop {
                if cancel.is_stopped() || worker_error.lock().is_some() {
                    break;
                }

                let cap = active_limit.load(Ordering::Relaxed).max(1);
                if worker_idx >= cap {
                    let any_unfinished = {
                        let segs = segments.lock();
                        segs.iter().any(|s| !s.done && s.remaining() > 0)
                    };
                    if !any_unfinished {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }

                // Pick next unclaimed incomplete segment, or work-steal half of the largest remaining segment
                let (seg_idx_opt, any_active_or_pending) = {
                    let mut segs = segments.lock();
                    let mut active = active_workers.lock();

                    // First look for an unstarted/idle incomplete segment
                    let mut chosen = None;
                    for (i, s) in segs.iter().enumerate() {
                        if !s.done && !active.get(i).copied().unwrap_or(false) && s.remaining() > 0 {
                            chosen = Some(i);
                            break;
                        }
                    }

                    // Work-stealing: split the largest remaining active segment if >= 512 KiB remains (disabled when throttled)
                    if chosen.is_none() && !throttled.load(Ordering::Relaxed) {
                        let mut best: Option<(usize, u64)> = None;
                        for (i, s) in segs.iter().enumerate() {
                            let rem = s.remaining();
                            if !s.done && rem >= 512 * 1024 {
                                if best.map_or(true, |(_, best_rem)| rem > best_rem) {
                                    best = Some((i, rem));
                                }
                            }
                        }
                        if let Some((victim_idx, rem)) = best {
                            let victim_current = segs[victim_idx].start + segs[victim_idx].downloaded;
                            let victim_old_end = segs[victim_idx].end;
                            let split_point = victim_current + (rem / 2);
                            if split_point > victim_current && split_point <= victim_old_end {
                                segs[victim_idx].end = split_point - 1;
                                let new_id = segs.len();
                                segs.push(SegmentState {
                                    id: new_id,
                                    start: split_point,
                                    end: victim_old_end,
                                    downloaded: 0,
                                    done: false,
                                });
                                active.push(false);
                                chosen = Some(new_id);
                            }
                        }
                    }

                    if let Some(idx) = chosen {
                        active[idx] = true;
                    }
                    let still_busy = active.iter().any(|&a| a)
                        || segs.iter().any(|s| !s.done && s.remaining() > 0);
                    (chosen, still_busy)
                };

                let Some(seg_idx) = seg_idx_opt else {
                    if any_active_or_pending {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        continue;
                    }
                    break;
                };

                let res = download_segment_loop(
                    &client,
                    &url,
                    &headers,
                    &part_path,
                    seg_idx,
                    worker_idx,
                    &segments,
                    &active_limit,
                    &throttled,
                    &limiter,
                    &cancel,
                    &bytes_window,
                )
                .await;

                {
                    let mut active = active_workers.lock();
                    if let Some(flag) = active.get_mut(seg_idx) {
                        *flag = false;
                    }
                }

                if let Err(err) = res {
                    if !cancel.is_stopped() {
                        *worker_error.lock() = Some(format!("Worker {worker_idx}: {err}"));
                    }
                    break;
                }
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    done_flag.store(true, Ordering::Relaxed);
    let _ = reporter_handle.await;

    let final_segs = segments_shared.lock().clone();
    item.downloaded_bytes = final_segs.iter().map(|s| s.downloaded).sum();
    item.segments = final_segs;

    if let Some(err) = worker_error.lock().take() {
        return Err(anyhow!(err));
    }

    Ok(())
}

async fn download_segment_loop(
    client: &Client,
    url: &str,
    headers: &HashMap<String, String>,
    part_path: &Path,
    seg_idx: usize,
    worker_idx: usize,
    segments: &Arc<Mutex<Vec<SegmentState>>>,
    active_limit: &Arc<AtomicUsize>,
    throttled: &Arc<AtomicBool>,
    limiter: &BandwidthLimiter,
    cancel: &Arc<DownloadCancelHandle>,
    bytes_window: &Arc<AtomicU64>,
) -> Result<()> {
    let mut attempts = 0usize;
    loop {
        if cancel.is_stopped() {
            return Ok(());
        }
        let (range_start, range_end) = {
            let mut segs = segments.lock();
            let seg = &mut segs[seg_idx];
            let cur = seg.start + seg.downloaded;
            if cur > seg.end {
                seg.done = true;
                return Ok(());
            }
            (cur, seg.end)
        };

        let req = apply_custom_headers(client.get(url), headers)
            .header(RANGE, format!("bytes={range_start}-{range_end}"));

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                attempts += 1;
                if attempts >= 5 {
                    return Err(e.into());
                }
                tokio::time::sleep(Duration::from_millis(600 * attempts as u64)).await;
                continue;
            }
        };

        let status = resp.status();
        if status != reqwest::StatusCode::PARTIAL_CONTENT && status != reqwest::StatusCode::OK {
            let is_rate_or_conn_limit = matches!(
                status,
                reqwest::StatusCode::FORBIDDEN
                    | reqwest::StatusCode::TOO_MANY_REQUESTS
                    | reqwest::StatusCode::SERVICE_UNAVAILABLE
            );

            if is_rate_or_conn_limit {
                throttled.store(true, Ordering::Relaxed);
                let prev = active_limit.load(Ordering::Relaxed);
                let next_cap = if prev > 1 { (prev / 2).max(1) } else { 1 };
                let _ = active_limit.compare_exchange(
                    prev,
                    next_cap,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                let effective_cap = active_limit.load(Ordering::Relaxed).max(1);
                if worker_idx >= effective_cap {
                    // Release this segment so surviving workers can finish it at lower concurrency
                    return Ok(());
                }
            }

            attempts += 1;
            let max_attempts = if is_rate_or_conn_limit { 6 } else { 4 };
            if attempts >= max_attempts {
                return Err(anyhow!("HTTP {status} no segmento {seg_idx}"));
            }
            let delay_ms = if is_rate_or_conn_limit {
                1200 * attempts as u64 + (worker_idx as u64 * 150)
            } else {
                600 * attempts as u64
            };
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            continue;
        }

        let mut file = OpenOptions::new().write(true).open(part_path)?;
        let mut offset = range_start;
        file.seek(SeekFrom::Start(offset))?;
        let mut stream = resp.bytes_stream();

        while !cancel.is_stopped() {
            let next = tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancel.notify.notified() => None,
            };
            let Some(chunk_res) = next else {
                break;
            };
            let chunk = match chunk_res {
                Ok(b) => b,
                Err(e) => {
                    attempts += 1;
                    if attempts >= 4 {
                        return Err(e.into());
                    }
                    break;
                }
            };
            if chunk.is_empty() {
                continue;
            }

            limiter.consume(chunk.len()).await;

            // Clamp to dynamic segment end (in case work-stealing shrank our segment!)
            let allowed_len = {
                let mut segs = segments.lock();
                let seg = &mut segs[seg_idx];
                if offset > seg.end {
                    seg.done = true;
                    0
                } else {
                    let max_bytes = (seg.end - offset + 1) as usize;
                    chunk.len().min(max_bytes)
                }
            };

            if allowed_len == 0 {
                return Ok(());
            }

            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&chunk[..allowed_len])?;
            offset += allowed_len as u64;
            bytes_window.fetch_add(allowed_len as u64, Ordering::Relaxed);

            let finished = {
                let mut segs = segments.lock();
                let seg = &mut segs[seg_idx];
                seg.downloaded = offset.saturating_sub(seg.start);
                if offset > seg.end {
                    seg.done = true;
                    true
                } else {
                    false
                }
            };

            if finished {
                file.flush()?;
                return Ok(());
            }
        }

        file.flush()?;
        if cancel.is_stopped() {
            return Ok(());
        }

        let done_now = segments.lock()[seg_idx].done;
        if done_now {
            return Ok(());
        }
    }
}

pub async fn verify_file_checksum(path: &Path, expected: &str) -> Result<bool> {
    let path = path.to_path_buf();
    let expected = expected.trim().to_lowercase();
    tokio::task::spawn_blocking(move || -> Result<bool> {
        let (algo, hash_hex) = if let Some((a, h)) = expected.split_once(':') {
            (a.trim(), h.trim())
        } else if expected.len() == 32 {
            ("md5", expected.as_str())
        } else {
            ("sha256", expected.as_str())
        };

        let mut file = File::open(&path)?;
        let mut buf = vec![0u8; 256 * 1024];
        match algo {
            "md5" => {
                let mut hasher = Md5::new();
                loop {
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                }
                let got = hex::encode(hasher.finalize());
                Ok(got == hash_hex)
            }
            _ => {
                let mut hasher = Sha256::new();
                loop {
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                }
                let got = hex::encode(hasher.finalize());
                Ok(got == hash_hex)
            }
        }
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_disposition_plain_and_rfc5987() {
        assert_eq!(
            parse_content_disposition(r#"attachment; filename="installer-v2.exe""#).as_deref(),
            Some("installer-v2.exe")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''meu%20arquivo%20final.7z").as_deref(),
            Some("meu arquivo final.7z")
        );
    }

    #[test]
    fn builds_contiguous_segments() {
        assert_eq!(build_initial_segments(1000, 4).len(), 1);
        let total = 4 * 1024 * 1024;
        let segs = build_initial_segments(total, 4);
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs[3].end, total - 1);
    }

    #[test]
    fn unique_destination_avoids_disk_files_part_files_and_reserved_paths() {
        let dir = std::env::temp_dir().join(format!("vortex_http_dedup_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let base_name = "Super Smash Bros. Ultimate [01006A800016E000][v0](axekin.com).zip";
        let first = dir.join(base_name);
        std::fs::write(&first, b"done").unwrap();

        let second_part = part_file_path(&dir.join(
            "Super Smash Bros. Ultimate [01006A800016E000][v0](axekin.com) (1).zip",
        ));
        std::fs::write(&second_part, b"partial").unwrap();

        let reserved_third = dir.join(
            "Super Smash Bros. Ultimate [01006A800016E000][v0](axekin.com) (2).zip",
        );

        let chosen = unique_destination_path_with_reserved(&dir, base_name, &[reserved_third]);
        assert_eq!(
            chosen.file_name().and_then(|n| n.to_str()),
            Some("Super Smash Bros. Ultimate [01006A800016E000][v0](axekin.com) (3).zip")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_destination_handles_compound_extensions_and_no_extension() {
        let dir = std::env::temp_dir().join(format!("vortex_http_ext_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("backup.tar.gz"), b"1").unwrap();
        let next_tar = unique_destination_path_with_reserved(&dir, "backup.tar.gz", &[]);
        assert_eq!(
            next_tar.file_name().and_then(|n| n.to_str()),
            Some("backup (1).tar.gz")
        );

        std::fs::write(dir.join("README"), b"1").unwrap();
        let next_plain = unique_destination_path_with_reserved(&dir, "README", &[]);
        assert_eq!(
            next_plain.file_name().and_then(|n| n.to_str()),
            Some("README (1)")
        );

        #[cfg(windows)]
        {
            let reserved_upper = dir.join("GAME.ZIP");
            let next_case = unique_destination_path_with_reserved(&dir, "game.zip", &[reserved_upper]);
            assert_eq!(
                next_case.file_name().and_then(|n| n.to_str()),
                Some("game (1).zip")
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
