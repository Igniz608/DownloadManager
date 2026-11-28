use crate::extractor::{
    ExtractionPasswordRequired, ProgressCallback, detect_default_extractors,
    extract_archive_with_progress, is_extractable_archive, is_password_error_text,
    select_best_extractor,
};
use crate::http_engine::{
    BandwidthLimiter, DestinationResolver, DownloadCancelHandle, HtmlPageDetected,
    ProgressSnapshot, apply_custom_headers, build_http_client, part_file_path, paths_conflict,
    probe_url, run_http_download_with_options_and_resolver, unique_destination_path_with_reserved,
};
use crate::media_engine::{
    MediaToolsStatus, compress_video_file, detect_media_tools, detect_media_url,
    install_ffmpeg_essentials, install_yt_dlp, plan_video_compression, probe_media_url,
    probe_single_media_url, probe_video_file, run_media_download_with_resolver,
    unique_compressed_output_path,
};
use crate::model::{
    AppSettings, CompressionJobSnapshot, CompressionJobStatus, DownloadId, DownloadItem,
    DownloadKind, DownloadPackage, DownloadStatus, FileCategory, GrabbedLink, MediaDownloadMeta,
    MediaProbeResult, NewDownloadRequest, NewMediaRequest, NoticeEntry, PackageId,
    PasswordPromptRequest, TorrentDownloadMeta, VideoCompressionCodec, VideoProbeInfo,
};
use crate::package_grabber::{
    assign_package_totals, detect_multipart, extract_urls_from_text, fallback_filename_from_url,
    find_missing_parts, is_fileditch_host, is_html_probe, is_vikingfile_host, probe_grabbed_links,
    resolve_indirect_http_url, sanitize_filename,
};
use crate::state::{PersistedState, load_settings, load_state, save_settings, save_state};
use crate::torrent_engine::{
    TorrentEngine, extract_magnet_info_hash, fallback_torrent_name,
};
use anyhow::{Result, anyhow};
use chrono::Utc;
use parking_lot::RwLock;
use reqwest::Client;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SpeedSample {
    pub download_bps: u64,
    pub upload_bps: u64,
}

#[derive(Debug, Clone)]
pub struct BrowserResolvedOutcome {
    pub matched: bool,
    pub next_url: Option<String>,
    pub remaining: usize,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ManagerSnapshot {
    pub downloads: Vec<DownloadItem>,
    pub packages: Vec<DownloadPackage>,
    pub grabbed_links: Vec<GrabbedLink>,
    pub grabber_probing: bool,
    pub total_download_bps: u64,
    pub total_upload_bps: u64,
    pub speed_history: Vec<SpeedSample>,
    pub active_count: usize,
    pub media_tools: MediaToolsStatus,
    pub notifications: Vec<NoticeEntry>,
    pub pending_password_prompts: Vec<PasswordPromptRequest>,
    pub compression_jobs: Vec<CompressionJobSnapshot>,
}

pub struct DownloadManager {
    pub settings: Arc<RwLock<AppSettings>>,
    pub downloads: Arc<RwLock<Vec<DownloadItem>>>,
    pub packages: Arc<RwLock<Vec<DownloadPackage>>>,
    pub grabbed_links: Arc<RwLock<Vec<GrabbedLink>>>,
    browser_resolve_queue: Arc<RwLock<VecDeque<String>>>,
    active_browser_resolve_url: Arc<RwLock<Option<String>>>,
    grabber_probe_count: Arc<AtomicUsize>,
    focus_grabber_requested: Arc<AtomicBool>,
    repaint_cb: Arc<RwLock<Option<Arc<dyn Fn() + Send + Sync>>>>,
    pub speed_history: Arc<RwLock<VecDeque<SpeedSample>>>,
    pub notifications: Arc<RwLock<VecDeque<NoticeEntry>>>,
    pub pending_password_prompts: Arc<RwLock<Vec<PasswordPromptRequest>>>,
    pub compression_jobs: Arc<RwLock<Vec<CompressionJobSnapshot>>>,
    compression_cancels: Arc<RwLock<HashMap<Uuid, Arc<AtomicBool>>>>,
    client: Client,
    limiter: BandwidthLimiter,
    cancel_handles: Arc<RwLock<HashMap<DownloadId, Arc<DownloadCancelHandle>>>>,
    torrent_engine: Arc<AsyncMutex<Option<Arc<TorrentEngine>>>>,
    runtime: tokio::runtime::Handle,
}

impl DownloadManager {
    pub fn new(runtime: tokio::runtime::Handle) -> Arc<Self> {
        let mut settings = load_settings();
        settings.extractors = detect_default_extractors(&settings.extractors);
        let _ = save_settings(&settings);

        let mut persisted = load_state();
        for d in &mut persisted.downloads {
            if let Some(ref st) = d.extraction_status {
                if is_password_error_text(st) {
                    d.extraction_status = Some("Senha necessária para extração".to_string());
                }
            }
        }
        for p in &mut persisted.packages {
            if let Some(ref st) = p.extraction_status {
                if is_password_error_text(st) {
                    p.extraction_status = Some("Senha necessária para extração".to_string());
                }
            }
        }

        let client = build_http_client().expect("Failed to initialize HTTP client");
        let limiter = BandwidthLimiter::new(settings.speed_limit_kbps);

        let mut hist = VecDeque::with_capacity(90);
        for _ in 0..60 {
            hist.push_back(SpeedSample {
                download_bps: 0,
                upload_bps: 0,
            });
        }

        let mgr = Arc::new(Self {
            settings: Arc::new(RwLock::new(settings)),
            downloads: Arc::new(RwLock::new(persisted.downloads)),
            packages: Arc::new(RwLock::new(persisted.packages)),
            grabbed_links: Arc::new(RwLock::new(Vec::new())),
            browser_resolve_queue: Arc::new(RwLock::new(VecDeque::new())),
            active_browser_resolve_url: Arc::new(RwLock::new(None)),
            grabber_probe_count: Arc::new(AtomicUsize::new(0)),
            focus_grabber_requested: Arc::new(AtomicBool::new(false)),
            repaint_cb: Arc::new(RwLock::new(None)),
            speed_history: Arc::new(RwLock::new(hist)),
            notifications: Arc::new(RwLock::new(VecDeque::with_capacity(20))),
            pending_password_prompts: Arc::new(RwLock::new(Vec::new())),
            compression_jobs: Arc::new(RwLock::new(Vec::new())),
            compression_cancels: Arc::new(RwLock::new(HashMap::new())),
            client,
            limiter,
            cancel_handles: Arc::new(RwLock::new(HashMap::new())),
            torrent_engine: Arc::new(AsyncMutex::new(None)),
            runtime,
        });

        mgr.spawn_background_loops();
        mgr
    }

    fn push_notice(&self, msg: impl Into<String>) {
        self.push_notice_kind(msg, false);
    }

    fn push_error_notice(&self, msg: impl Into<String>) {
        self.push_notice_kind(msg, true);
    }

    fn push_notice_kind(&self, msg: impl Into<String>, is_error: bool) {
        let mut g = self.notifications.write();
        g.push_back(NoticeEntry {
            id: Uuid::new_v4(),
            message: msg.into(),
            is_error,
            created_at: Utc::now(),
        });
        while g.len() > 20 {
            g.pop_front();
        }
    }

    pub fn dismiss_notice(&self, id: Uuid) {
        self.notifications.write().retain(|n| n.id != id);
    }

    pub fn clear_notices(&self) {
        self.notifications.write().clear();
    }

    pub fn snapshot(&self) -> ManagerSnapshot {
        let downloads = self.downloads.read().clone();
        let packages = self.packages.read().clone();
        let grabbed_links = self.grabbed_links.read().clone();
        let grabber_probing = self.grabber_probe_count.load(Ordering::Relaxed) > 0;
        let total_download_bps: u64 = downloads.iter().map(|d| d.speed_bps).sum();
        let total_upload_bps: u64 = downloads.iter().map(|d| d.upload_speed_bps).sum();
        let active_count = downloads.iter().filter(|d| d.status.is_active()).count();
        let speed_history = self.speed_history.read().iter().cloned().collect();
        let media_tools = detect_media_tools(&self.settings.read());
        let now = Utc::now();
        let notifications = {
            let mut g = self.notifications.write();
            g.retain(|n| {
                let max_age = if n.is_error { 15 } else { 8 };
                (now - n.created_at).num_seconds() < max_age
            });
            g.iter().cloned().collect()
        };
        let pending_password_prompts = self.pending_password_prompts.read().clone();
        let compression_jobs = self.compression_jobs.read().clone();

        ManagerSnapshot {
            downloads,
            packages,
            grabbed_links,
            grabber_probing,
            total_download_bps,
            total_upload_bps,
            speed_history,
            active_count,
            media_tools,
            notifications,
            pending_password_prompts,
            compression_jobs,
        }
    }

    fn persist_now(&self) {
        let state = PersistedState {
            downloads: self.downloads.read().clone(),
            packages: self.packages.read().clone(),
        };
        let _ = save_state(&state);
    }

    pub fn save_current_settings(&self) -> Result<()> {
        let s = self.settings.read().clone();
        self.limiter.set_limit_kbps(s.speed_limit_kbps);
        save_settings(&s)
    }

    pub fn refresh_extractors(&self) {
        let mut s = self.settings.write();
        s.extractors = detect_default_extractors(&s.extractors);
        let _ = save_settings(&s);
    }

    fn spawn_background_loops(self: &Arc<Self>) {
        // 1. Speed history + Torrent polling + Queue scheduler (every 500ms)
        let mgr = self.clone();
        self.runtime.spawn(async move {
            let mut tick_count = 0u64;
            let mut last_status_sig: Option<(usize, usize, usize, usize)> = None;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                tick_count += 1;

                // Poll torrents
                let torrent_opt = mgr.torrent_engine.lock().await.clone();
                if let Some(te) = torrent_opt {
                    let (seed, max_ratio) = {
                        let s = mgr.settings.read();
                        (s.torrent_seed_after_complete, s.torrent_max_ratio)
                    };
                    let updates = te.poll_snapshots(seed, max_ratio);
                    let mut finished_to_pause = Vec::new();
                    {
                        let mut dls = mgr.downloads.write();
                        for (id, upd) in updates {
                            if let Some(item) = dls.iter_mut().find(|d| d.id == id) {
                                if let Some(name) = upd.name {
                                    item.filename = name;
                                }
                                item.save_dir = upd.output_folder;
                                item.file_path = upd.file_path;
                                item.status = upd.status.clone();
                                item.total_bytes = upd.total_bytes.or(item.total_bytes);
                                item.downloaded_bytes = upd.downloaded_bytes;
                                item.uploaded_bytes = upd.uploaded_bytes;
                                item.speed_bps = upd.speed_bps;
                                item.upload_speed_bps = upd.upload_speed_bps;
                                if let Some(ref mut ti) = item.torrent_info {
                                    ti.peers_connected = upd.peers_connected;
                                    ti.peers_seen = upd.peers_seen;
                                    ti.ratio = upd.ratio;
                                    ti.files = upd.files;
                                }
                                if upd.finished && item.completed_at.is_none() {
                                    item.completed_at = Some(Utc::now());
                                    if !seed || (max_ratio > 0.0 && (upd.ratio as f32) >= max_ratio) {
                                        finished_to_pause.push(id);
                                    }
                                }
                            }
                        }
                    }
                    for id in finished_to_pause {
                        let _ = te.pause(id).await;
                    }
                }

                // Speed history every 1s
                if tick_count % 2 == 0 {
                    let (dl_bps, ul_bps) = {
                        let dls = mgr.downloads.read();
                        (
                            dls.iter().map(|d| d.speed_bps).sum::<u64>(),
                            dls.iter().map(|d| d.upload_speed_bps).sum::<u64>(),
                        )
                    };
                    let mut hist = mgr.speed_history.write();
                    hist.push_back(SpeedSample {
                        download_bps: dl_bps,
                        upload_bps: ul_bps,
                    });
                    while hist.len() > 60 {
                        hist.pop_front();
                    }
                }

                // Queue pump
                mgr.pump_queue();

                let status_sig = {
                    let dls = mgr.downloads.read();
                    let active = dls
                        .iter()
                        .filter(|d| d.status.is_active() || d.status == DownloadStatus::Queued)
                        .count();
                    let completed = dls
                        .iter()
                        .filter(|d| matches!(d.status, DownloadStatus::Completed | DownloadStatus::Seeding))
                        .count();
                    let failed = dls
                        .iter()
                        .filter(|d| matches!(d.status, DownloadStatus::Failed(_)))
                        .count();
                    (active, completed, failed, dls.len())
                };
                if last_status_sig != Some(status_sig) {
                    last_status_sig = Some(status_sig);
                    mgr.request_ui_repaint();
                }

                // Periodic state save every 5 seconds
                if tick_count % 10 == 0 {
                    mgr.persist_now();
                }
            }
        });
    }

    pub fn pump_queue(self: &Arc<Self>) {
        let max_concurrent = self.settings.read().max_concurrent_downloads.max(1);
        let to_start: Vec<DownloadId> = {
            let dls = self.downloads.read();
            select_queued_downloads_to_start(&dls, max_concurrent)
        };

        for id in to_start {
            self.start_download_task(id);
        }
    }

    pub fn default_torrent_dir(&self) -> PathBuf {
        let s = self.settings.read();
        default_torrent_dir_from_settings(&s)
    }

    pub fn add_http_download(self: &Arc<Self>, req: NewDownloadRequest) -> Result<DownloadId> {
        let url_trimmed = req.url.trim().to_string();
        if is_torrent_request(&url_trimmed, req.filename.as_deref()) {
            let save_dir = req.save_dir.unwrap_or_else(|| self.default_torrent_dir());
            return self.add_torrent_async_with_headers(&url_trimmed, save_dir, req.headers);
        }

        if req.checksum.is_none() {
            if let Some(media_match) = detect_media_url(&url_trimmed) {
                return self.add_media_download_with_start(
                    NewMediaRequest {
                        url: url_trimmed,
                        save_dir: req.save_dir,
                        format_id: None,
                        quality_label: Some("Melhor".to_string()),
                        audio_only: false,
                        embed_subtitles: false,
                        headers: req.headers,
                        title_hint: req.filename,
                        is_hls: media_match.is_hls,
                        is_dash: media_match.is_dash,
                    },
                    req.start_immediately,
                );
            }
        }

        let settings = self.settings.read().clone();
        let raw_filename = req
            .filename
            .map(|f| sanitize_filename(&f))
            .unwrap_or_else(|| fallback_filename_from_url(&url_trimmed));
        let category = FileCategory::from_filename(&raw_filename);
        let base_dir = req.save_dir.unwrap_or_else(|| settings.default_download_dir.clone());
        let save_dir = if settings.categorize_into_subfolders {
            base_dir.join(category.subfolder())
        } else {
            base_dir
        };
        let auto_start = req.start_immediately.unwrap_or(settings.auto_start_downloads);

        let id = Uuid::new_v4();
        {
            let mut dls = self.downloads.write();
            let file_path = Self::reserve_unique_path_locked(&dls, None, &save_dir, &raw_filename);
            let filename = file_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
                .unwrap_or(raw_filename);
            let item = DownloadItem {
                id,
                package_id: None,
                part_index: None,
                part_total: None,
                kind: DownloadKind::Http,
                status: if auto_start {
                    DownloadStatus::Queued
                } else {
                    DownloadStatus::Paused
                },
                url: url_trimmed,
                filename,
                save_dir,
                file_path,
                category,
                total_bytes: None,
                downloaded_bytes: 0,
                uploaded_bytes: 0,
                speed_bps: 0,
                upload_speed_bps: 0,
                eta_seconds: None,
                connections: req.connections.unwrap_or(settings.default_connections).clamp(1, 32),
                supports_ranges: false,
                segments: Vec::new(),
                etag: None,
                last_modified: None,
                headers: req.headers,
                checksum_expected: req.checksum,
                checksum_verified: None,
                media_info: None,
                torrent_info: None,
                auto_extract: req.auto_extract.unwrap_or(settings.auto_extract_archives),
                extractor_override: req.extractor_override,
                extract_password: req.extract_password,
                extracted_to: None,
                extraction_status: None,
                extraction_progress: None,
                created_at: Utc::now(),
                completed_at: None,
                error: None,
                priority: 0,
            };

            dls.insert(0, item);
        }
        self.persist_now();
        self.pump_queue();
        Ok(id)
    }

    pub fn add_media_download(self: &Arc<Self>, req: NewMediaRequest) -> Result<DownloadId> {
        self.add_media_download_with_start(req, None)
    }

    fn add_media_download_with_start(
        self: &Arc<Self>,
        req: NewMediaRequest,
        start_immediately: Option<bool>,
    ) -> Result<DownloadId> {
        let settings = self.settings.read().clone();
        let base_dir = req.save_dir.unwrap_or_else(|| settings.default_download_dir.clone());
        let save_dir = if settings.categorize_into_subfolders {
            let sub = if req.audio_only {
                FileCategory::Audio.subfolder()
            } else {
                FileCategory::Video.subfolder()
            };
            base_dir.join(sub)
        } else {
            base_dir
        };

        let title_clean = req
            .title_hint
            .clone()
            .map(|t| sanitize_filename(&t))
            .unwrap_or_else(|| fallback_filename_from_url(&req.url));
        let auto_start = start_immediately.unwrap_or(settings.auto_start_downloads);

        let id = Uuid::new_v4();
        {
            let mut dls = self.downloads.write();
            let file_path = Self::reserve_unique_path_locked(&dls, None, &save_dir, &title_clean);
            let filename = file_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
                .unwrap_or(title_clean);
            let item = DownloadItem {
                id,
                package_id: None,
                part_index: None,
                part_total: None,
                kind: DownloadKind::Media,
                status: if auto_start {
                    DownloadStatus::Queued
                } else {
                    DownloadStatus::Paused
                },
                url: req.url,
                filename,
                save_dir,
                file_path,
                category: if req.audio_only {
                    FileCategory::Audio
                } else {
                    FileCategory::Video
                },
                total_bytes: None,
                downloaded_bytes: 0,
                uploaded_bytes: 0,
                speed_bps: 0,
                upload_speed_bps: 0,
                eta_seconds: None,
                connections: settings.default_connections,
                supports_ranges: false,
                segments: Vec::new(),
                etag: None,
                last_modified: None,
                headers: req.headers,
                checksum_expected: None,
                checksum_verified: None,
                media_info: Some(MediaDownloadMeta {
                    format_id: req.format_id,
                    quality_label: req.quality_label,
                    title: req.title_hint,
                    duration_secs: None,
                    is_hls: req.is_hls,
                    is_dash: req.is_dash,
                    audio_only: req.audio_only,
                    embed_subtitles: req.embed_subtitles,
                    extractor_used: if req.is_hls { "native-hls" } else { "yt-dlp" }.into(),
                }),
                torrent_info: None,
                auto_extract: false,
                extractor_override: None,
                extract_password: None,
                extracted_to: None,
                extraction_status: None,
                extraction_progress: None,
                created_at: Utc::now(),
                completed_at: None,
                error: None,
                priority: 0,
            };

            dls.insert(0, item);
        }
        self.persist_now();
        self.pump_queue();
        Ok(id)
    }

    pub fn add_torrent_async(self: &Arc<Self>, source: &str, save_dir: PathBuf) -> Result<DownloadId> {
        self.add_torrent_async_with_headers(source, save_dir, HashMap::new())
    }

    pub fn add_torrent_async_with_headers(
        self: &Arc<Self>,
        source: &str,
        save_dir: PathBuf,
        headers: HashMap<String, String>,
    ) -> Result<DownloadId> {
        let source_owned = source.trim().to_string();
        if source_owned.is_empty() {
            return Err(anyhow!("Link magnet ou arquivo .torrent vazio."));
        }

        let initial_hash = extract_magnet_info_hash(&source_owned).unwrap_or_default();
        let existing_action = {
            let mut dls = self.downloads.write();
            let pos = dls.iter().position(|d| {
                d.kind == DownloadKind::Torrent
                    && (d.url == source_owned
                        || (!initial_hash.is_empty()
                            && d.torrent_info
                                .as_ref()
                                .map(|ti| ti.info_hash.eq_ignore_ascii_case(&initial_hash))
                                .unwrap_or(false)))
            });
            match pos {
                Some(idx) => {
                    let status = dls[idx].status.clone();
                    let existing_id = dls[idx].id;
                    if matches!(status, DownloadStatus::Failed(_)) {
                        dls.remove(idx);
                        None
                    } else if status == DownloadStatus::Paused {
                        Some((existing_id, true))
                    } else {
                        Some((existing_id, false))
                    }
                }
                None => None,
            }
        };

        if let Some((existing_id, was_paused)) = existing_action {
            if was_paused {
                self.resume_download(existing_id);
                self.push_notice("Torrent retomado.");
            } else {
                self.push_notice("Torrent já está na lista de downloads.");
            }
            return Ok(existing_id);
        }

        let id = Uuid::new_v4();
        let initial_name = fallback_torrent_name(&source_owned);
        let item = DownloadItem {
            id,
            package_id: None,
            part_index: None,
            part_total: None,
            kind: DownloadKind::Torrent,
            status: DownloadStatus::Probing,
            url: source_owned.clone(),
            filename: initial_name.clone(),
            save_dir: save_dir.clone(),
            file_path: save_dir.join(&initial_name),
            category: FileCategory::Torrent,
            total_bytes: None,
            downloaded_bytes: 0,
            uploaded_bytes: 0,
            speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: 0,
            supports_ranges: false,
            segments: Vec::new(),
            etag: None,
            last_modified: None,
            headers: headers.clone(),
            checksum_expected: None,
            checksum_verified: None,
            media_info: None,
            torrent_info: Some(TorrentDownloadMeta {
                info_hash: initial_hash,
                magnet_or_path: source_owned.clone(),
                rqbit_id: None,
                peers_connected: 0,
                peers_seen: 0,
                ratio: 0.0,
                files: Vec::new(),
            }),
            auto_extract: false,
            extractor_override: None,
            extract_password: None,
            extracted_to: None,
            extraction_status: None,
            extraction_progress: None,
            created_at: Utc::now(),
            completed_at: None,
            error: None,
            priority: 0,
        };

        self.downloads.write().insert(0, item);
        self.persist_now();
        self.push_notice(format!("Analisando torrent: {initial_name}"));
        self.spawn_torrent_attach_task(id, source_owned, save_dir, headers, false);
        Ok(id)
    }

    fn spawn_torrent_attach_task(
        self: &Arc<Self>,
        id: DownloadId,
        source: String,
        save_dir: PathBuf,
        headers: HashMap<String, String>,
        paused: bool,
    ) {
        let cancel = Arc::new(DownloadCancelHandle::new());
        self.cancel_handles.write().insert(id, cancel.clone());

        let mgr = self.clone();
        self.runtime.spawn(async move {
            let attach_fut = async {
                let is_http = source
                    .get(..7)
                    .map(|p| p.eq_ignore_ascii_case("http://"))
                    .unwrap_or(false)
                    || source
                        .get(..8)
                        .map(|p| p.eq_ignore_ascii_case("https://"))
                        .unwrap_or(false);

                let bytes_override = if is_http && !headers.is_empty() {
                    let resp = apply_custom_headers(mgr.client.get(&source), &headers)
                        .send()
                        .await?
                        .error_for_status()?;
                    Some(resp.bytes().await?)
                } else {
                    None
                };

                let te = mgr.ensure_torrent_engine().await?;
                let attached = te
                    .add_magnet_or_file_with_id(id, &source, bytes_override, save_dir, paused)
                    .await?;
                Ok::<_, anyhow::Error>((te, attached))
            };

            let outcome = tokio::select! {
                biased;
                _ = cancel.notify.notified() => None,
                res = attach_fut => Some(res),
            };

            mgr.cancel_handles.write().remove(&id);

            let Some(res) = outcome else {
                if !cancel.cancelled.load(Ordering::SeqCst) {
                    let mut dls = mgr.downloads.write();
                    if let Some(d) = dls.iter_mut().find(|x| x.id == id) {
                        d.status = DownloadStatus::Paused;
                        d.speed_bps = 0;
                        d.eta_seconds = None;
                    }
                    drop(dls);
                    mgr.persist_now();
                }
                return;
            };

            let (still_present, was_paused) = {
                let dls = mgr.downloads.read();
                match dls.iter().find(|x| x.id == id) {
                    Some(d) => (true, d.status == DownloadStatus::Paused || cancel.paused.load(Ordering::SeqCst)),
                    None => (false, false),
                }
            };

            match res {
                Ok((te, attached)) => {
                    if !still_present || cancel.cancelled.load(Ordering::SeqCst) {
                        let delete_files = cancel.delete_on_cancel.load(Ordering::SeqCst);
                        let _ = te.remove(id, delete_files).await;
                        return;
                    }
                    if was_paused {
                        let _ = te.pause(id).await;
                    }
                    let mut final_name = attached.filename.clone();
                    {
                        let mut dls = mgr.downloads.write();
                        if let Some(item) = dls.iter_mut().find(|x| x.id == id) {
                            item.filename = attached.filename;
                            final_name = item.filename.clone();
                            item.save_dir = attached.save_dir;
                            item.file_path = attached.file_path;
                            item.status = if was_paused {
                                DownloadStatus::Paused
                            } else {
                                attached.status
                            };
                            item.total_bytes = attached.total_bytes.or(item.total_bytes);
                            item.downloaded_bytes = attached.downloaded_bytes;
                            item.uploaded_bytes = attached.uploaded_bytes;
                            item.error = None;
                            item.torrent_info = Some(TorrentDownloadMeta {
                                info_hash: attached.info_hash,
                                magnet_or_path: source,
                                rqbit_id: Some(attached.rqbit_id),
                                peers_connected: 0,
                                peers_seen: 0,
                                ratio: 0.0,
                                files: attached.files,
                            });
                        }
                    }
                    mgr.persist_now();
                    if !was_paused {
                        mgr.push_notice(format!("Torrent iniciado: {final_name}"));
                    }
                }
                Err(e) => {
                    if !still_present || cancel.cancelled.load(Ordering::SeqCst) {
                        return;
                    }
                    let err_str = e.to_string();
                    {
                        let mut dls = mgr.downloads.write();
                        if let Some(item) = dls.iter_mut().find(|x| x.id == id) {
                            item.status = DownloadStatus::Failed(err_str.clone());
                            item.error = Some(err_str.clone());
                            item.speed_bps = 0;
                            item.eta_seconds = None;
                        }
                    }
                    mgr.persist_now();
                    mgr.push_error_notice(format!("Falha ao adicionar torrent: {err_str}"));
                }
            }
        });
    }

    async fn ensure_torrent_engine(&self) -> Result<Arc<TorrentEngine>> {
        let mut guard = self.torrent_engine.lock().await;
        if let Some(ref existing) = *guard {
            return Ok(existing.clone());
        }
        let dir = self.default_torrent_dir();
        let engine = Arc::new(TorrentEngine::new(dir).await?);
        *guard = Some(engine.clone());
        Ok(engine)
    }

    pub fn probe_text_into_grabber(
        self: &Arc<Self>,
        raw_text: String,
        headers: HashMap<String, String>,
    ) {
        let urls = extract_urls_from_text(&raw_text);
        if urls.is_empty() {
            self.push_notice("Nenhuma URL válida encontrada para analisar.");
            return;
        }
        self.probe_urls_into_grabber(urls, headers);
    }

    pub fn set_repaint_callback(&self, cb: impl Fn() + Send + Sync + 'static) {
        *self.repaint_cb.write() = Some(Arc::new(cb));
    }

    pub fn request_ui_repaint(&self) {
        if let Some(cb) = self.repaint_cb.read().as_ref().cloned() {
            cb();
        }
    }

    pub fn request_focus_grabber(&self) {
        self.focus_grabber_requested.store(true, Ordering::SeqCst);
        self.request_ui_repaint();
    }

    pub fn take_focus_grabber_request(&self) -> bool {
        self.focus_grabber_requested.swap(false, Ordering::SeqCst)
    }

    pub fn probe_urls_into_grabber(
        self: &Arc<Self>,
        urls: Vec<String>,
        headers: HashMap<String, String>,
    ) {
        let mgr = self.clone();
        let client = self.client.clone();
        self.grabber_probe_count.fetch_add(1, Ordering::Relaxed);
        self.runtime.spawn(async move {
            let links = probe_grabbed_links(&client, urls, headers).await;
            let ignore_non_part = mgr.settings.read().grabber_ignore_non_part;
            let mut kept_count = 0usize;
            let mut ignored_non_part = 0usize;
            {
                let mut g = mgr.grabbed_links.write();
                for mut link in links {
                    if !should_keep_grabbed_link(&link, ignore_non_part) {
                        ignored_non_part += 1;
                        continue;
                    }
                    kept_count += 1;
                    if !g.iter().any(|existing| existing.url == link.url) {
                        link.selected = false;
                        g.push(link);
                    }
                }
                sort_grabbed_links(&mut g);
            }
            mgr.grabber_probe_count.fetch_sub(1, Ordering::Relaxed);
            if ignored_non_part > 0 {
                mgr.push_notice(format!(
                    "{kept_count} link(s) analisados no LinkGrabber ({ignored_non_part} sem Parte ignorado(s))."
                ));
            } else {
                mgr.push_notice(format!("{kept_count} link(s) analisados no LinkGrabber."));
            }
        });
    }

    pub fn queue_browser_resolutions(&self, urls: Vec<String>) -> Option<String> {
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for raw in urls {
            let norm = normalize_browser_page_url(&raw);
            if !(norm.starts_with("http://") || norm.starts_with("https://")) {
                continue;
            }
            if seen.insert(norm.clone()) {
                unique.push(norm);
            }
        }
        if unique.is_empty() {
            return None;
        }

        let target_set: HashSet<String> = unique.iter().cloned().collect();
        {
            let mut g = self.grabbed_links.write();
            for link in g.iter_mut() {
                if target_set.contains(&normalize_browser_page_url(&link.url)) {
                    link.probing = true;
                    link.error = Some("Aguardando resolução Turnstile no navegador...".to_string());
                }
            }
        }

        let total = unique.len();
        let mut iter = unique.into_iter();
        let first = iter.next()?;
        {
            let mut q = self.browser_resolve_queue.write();
            q.clear();
            q.extend(iter);
        }
        *self.active_browser_resolve_url.write() = Some(first.clone());

        self.push_notice(format!(
            "Abrindo 1/{total} no navegador para resolver Turnstile..."
        ));
        Some(format!("{first}#vortex_resolve"))
    }

    pub fn pending_browser_urls(&self) -> (Vec<String>, usize) {
        let g = self.grabbed_links.read();
        let mut urls = Vec::new();
        let mut seen = HashSet::new();
        for link in g.iter() {
            if link.online == Some(true) {
                continue;
            }
            if is_browser_resolvable_link(link) || link.probing {
                let norm = normalize_browser_page_url(&link.url);
                if seen.insert(norm.clone()) {
                    urls.push(norm);
                }
            }
        }
        let queue_len = self.browser_resolve_queue.read().len();
        (urls, queue_len)
    }

    pub fn apply_browser_resolution(
        self: &Arc<Self>,
        page_url: String,
        direct_url: Option<String>,
        filename: Option<String>,
        error: Option<String>,
        headers: HashMap<String, String>,
    ) -> BrowserResolvedOutcome {
        let norm_page = normalize_browser_page_url(&page_url);
        let active_fallback = self.active_browser_resolve_url.write().take();
        let (next_url, remaining) = {
            let mut q = self.browser_resolve_queue.write();
            q.retain(|u| !browser_page_url_matches(u, &norm_page));
            let next = q.pop_front();
            *self.active_browser_resolve_url.write() = next.clone();
            (next.map(|u| format!("{u}#vortex_resolve")), q.len())
        };

        let clean_direct = direct_url
            .map(|s| s.trim().to_string())
            .filter(|s| s.starts_with("http://") || s.starts_with("https://"));

        let mut matched_ids: Vec<(Uuid, String, HashMap<String, String>, bool)> = Vec::new();
        {
            let mut g = self.grabbed_links.write();
            let mut match_against =
                |target: &str, out: &mut Vec<(Uuid, String, HashMap<String, String>, bool)>| {
                    for link in g.iter_mut() {
                        if !browser_page_url_matches(&link.url, target) {
                            continue;
                        }
                        if let Some(ref direct) = clean_direct {
                            let same_page_retry = browser_page_url_matches(direct, &link.url);
                            if !same_page_retry {
                                link.url = direct.clone();
                            }
                            for (k, v) in &headers {
                                if !v.trim().is_empty() {
                                    link.headers.insert(k.clone(), v.clone());
                                }
                            }
                            if let Some(ref fname) = filename {
                                let clean_fname = sanitize_filename(fname.trim());
                                let looks_generic = !link.filename.contains('.')
                                    || link.filename.len() <= 16 && link.part_number.is_none();
                                if !clean_fname.is_empty()
                                    && (looks_generic || link.filename.is_empty())
                                {
                                    let mp = detect_multipart(&clean_fname);
                                    link.category = FileCategory::from_filename(&clean_fname);
                                    link.package_key = mp.as_ref().map(|m| m.package_key.clone());
                                    link.part_number = mp.as_ref().map(|m| m.part_number);
                                    link.filename = clean_fname;
                                }
                            }
                            link.online = if same_page_retry { None } else { Some(true) };
                            link.error = None;
                            link.probing = true;
                            out.push((
                                link.id,
                                if same_page_retry {
                                    link.url.clone()
                                } else {
                                    direct.clone()
                                },
                                link.headers.clone(),
                                same_page_retry,
                            ));
                        } else {
                            link.probing = false;
                            link.online = Some(false);
                            link.error = Some(
                                error
                                    .clone()
                                    .filter(|s| !s.trim().is_empty())
                                    .unwrap_or_else(|| {
                                        "Falha ao resolver Turnstile no navegador".to_string()
                                    }),
                            );
                            out.push((link.id, String::new(), HashMap::new(), false));
                        }
                    }
                };
            match_against(&norm_page, &mut matched_ids);
            if matched_ids.is_empty() {
                if let Some(ref active_url) = active_fallback {
                    match_against(active_url, &mut matched_ids);
                }
            }
            if !matched_ids.is_empty() {
                assign_package_totals(&mut g);
                sort_grabbed_links(&mut g);
            }
        }

        let matched = !matched_ids.is_empty();
        if clean_direct.is_some() && matched {
            let mgr = self.clone();
            let client = self.client.clone();
            let probe_targets: Vec<(Uuid, String, HashMap<String, String>, bool)> = matched_ids
                .into_iter()
                .filter(|(_, u, _, _)| !u.is_empty())
                .collect();
            self.runtime.spawn(async move {
                for (id, direct, hdrs, same_page_retry) in probe_targets {
                    if same_page_retry {
                        let resolved = probe_grabbed_links(&client, vec![direct], hdrs).await;
                        let mut g = mgr.grabbed_links.write();
                        if let Some(link) = g.iter_mut().find(|l| l.id == id) {
                            link.probing = false;
                            if let Some(first) = resolved.into_iter().next() {
                                link.url = first.url;
                                link.filename = first.filename;
                                link.size_bytes = first.size_bytes;
                                link.supports_ranges = first.supports_ranges;
                                link.online = first.online;
                                link.error = first.error;
                                link.category = first.category;
                                link.package_key = first.package_key;
                                link.part_number = first.part_number;
                                link.source_site = first.source_site;
                                for (k, v) in first.headers {
                                    link.headers.entry(k).or_insert(v);
                                }
                            }
                        }
                        assign_package_totals(&mut g);
                        sort_grabbed_links(&mut g);
                        continue;
                    }

                    let probe_res = probe_url(&client, &direct, &hdrs).await;
                    let mut g = mgr.grabbed_links.write();
                    if let Some(link) = g.iter_mut().find(|l| l.id == id) {
                        link.probing = false;
                        if let Ok(info) = probe_res {
                            if !is_html_probe(&info) {
                                if let Some(sz) = info.total_bytes {
                                    link.size_bytes = Some(sz);
                                }
                                link.supports_ranges = info.supports_ranges;
                                if let Some(cd_name) = info.filename {
                                    let clean = sanitize_filename(&cd_name);
                                    if !clean.is_empty() {
                                        let mp = detect_multipart(&clean);
                                        link.category = FileCategory::from_filename(&clean);
                                        if link.package_key.is_none() {
                                            link.package_key = mp.as_ref().map(|m| m.package_key.clone());
                                        }
                                        if link.part_number.is_none() {
                                            link.part_number = mp.as_ref().map(|m| m.part_number);
                                        }
                                        link.filename = clean;
                                    }
                                }
                            }
                        }
                    }
                    assign_package_totals(&mut g);
                    sort_grabbed_links(&mut g);
                }
            });

            if remaining > 0 {
                self.push_notice(format!(
                    "Link resolvido no navegador! Avançando ({remaining} restante(s))..."
                ));
            } else {
                self.push_notice("Links resolvidos no LinkGrabber!".to_string());
            }
        } else if matched && remaining > 0 {
            self.push_notice(format!(
                "Falha em 1 link; avançando ({remaining} restante(s))..."
            ));
        }

        let message = if clean_direct.is_some() {
            if matched {
                "Link atualizado no LinkGrabber".to_string()
            } else {
                "Link capturado, mas não estava pendente no LinkGrabber".to_string()
            }
        } else {
            error.unwrap_or_else(|| "Erro ao resolver no navegador".to_string())
        };

        BrowserResolvedOutcome {
            matched,
            next_url,
            remaining,
            message,
        }
    }

    pub fn enqueue_selected_grabbed(
        self: &Arc<Self>,
        save_dir_override: Option<PathBuf>,
        auto_extract: bool,
        extractor_id: Option<String>,
        password: Option<String>,
        delete_archives: bool,
    ) -> usize {
        let selected: Vec<GrabbedLink> = {
            let mut g = self.grabbed_links.write();
            let sel: Vec<GrabbedLink> = g.iter().filter(|l| l.selected).cloned().collect();
            g.retain(|l| !l.selected);
            sel
        };
        if selected.is_empty() {
            return 0;
        }

        let settings = self.settings.read().clone();
        let base_dir = save_dir_override.unwrap_or_else(|| settings.default_download_dir.clone());

        // Group links by package_key (multi-part packages) vs standalone
        let mut grouped: HashMap<String, Vec<GrabbedLink>> = HashMap::new();
        let mut standalone = Vec::new();
        for link in selected {
            if let Some(ref key) = link.package_key {
                grouped.entry(key.clone()).or_default().push(link);
            } else {
                standalone.push(link);
            }
        }

        let mut added_count = 0usize;

        for (pkg_name, mut parts) in grouped {
            parts.sort_by_key(|p| p.part_number.unwrap_or(9999));
            let pkg_id = Uuid::new_v4();
            let pkg_dir = base_dir.join(sanitize_filename(&pkg_name));
            let part_nums: Vec<u32> = parts.iter().filter_map(|p| p.part_number).collect();
            let missing_parts = find_missing_parts(&part_nums);
            let total_parts = parts.len() as u32;
            let mut item_ids = Vec::new();

            for link in parts {
                let id = Uuid::new_v4();
                item_ids.push(id);
                let mut dls = self.downloads.write();
                let file_path = Self::reserve_unique_path_locked(&dls, None, &pkg_dir, &link.filename);
                let filename = file_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| link.filename.clone());
                let item = DownloadItem {
                    id,
                    package_id: Some(pkg_id),
                    part_index: link.part_number,
                    part_total: Some(total_parts),
                    kind: DownloadKind::Http,
                    status: DownloadStatus::Queued,
                    url: link.url,
                    filename,
                    save_dir: pkg_dir.clone(),
                    file_path,
                    category: link.category,
                    total_bytes: link.size_bytes,
                    downloaded_bytes: 0,
                    uploaded_bytes: 0,
                    speed_bps: 0,
                    upload_speed_bps: 0,
                    eta_seconds: None,
                    connections: settings.default_connections,
                    supports_ranges: link.supports_ranges,
                    segments: Vec::new(),
                    etag: None,
                    last_modified: None,
                    headers: link.headers,
                    checksum_expected: None,
                    checksum_verified: None,
                    media_info: None,
                    torrent_info: None,
                    auto_extract: false, // package handles unified extraction once all parts complete
                    extractor_override: extractor_id.clone(),
                    extract_password: password.clone(),
                    extracted_to: None,
                    extraction_status: None,
                    extraction_progress: None,
                    created_at: Utc::now(),
                    completed_at: None,
                    error: None,
                    priority: 0,
                };
                dls.insert(0, item);
                added_count += 1;
            }

            let pkg = DownloadPackage {
                id: pkg_id,
                name: pkg_name.clone(),
                save_dir: pkg_dir,
                item_ids,
                archive_base: Some(pkg_name),
                total_parts_expected: Some(total_parts),
                missing_parts,
                auto_extract,
                extractor_id: extractor_id.clone(),
                extract_password: password.clone(),
                delete_archives_after_extract: delete_archives,
                extraction_status: None,
                extraction_progress: None,
                extracted_to: None,
                created_at: Utc::now(),
            };
            self.packages.write().insert(0, pkg);
        }

        for link in standalone {
            let req = NewDownloadRequest {
                url: link.url,
                filename: Some(link.filename),
                save_dir: Some(base_dir.clone()),
                connections: Some(settings.default_connections),
                headers: link.headers,
                checksum: None,
                auto_extract: Some(auto_extract),
                extractor_override: extractor_id.clone(),
                extract_password: password.clone(),
                start_immediately: Some(true),
            };
            if self.add_http_download(req).is_ok() {
                added_count += 1;
            }
        }

        self.persist_now();
        self.pump_queue();
        added_count
    }

    fn start_download_task(self: &Arc<Self>, id: DownloadId) {
        let item_opt = self.downloads.read().iter().find(|d| d.id == id).cloned();
        let Some(item) = item_opt else {
            return;
        };

        let cancel = Arc::new(DownloadCancelHandle::new());
        self.cancel_handles.write().insert(id, cancel.clone());

        let mgr = self.clone();
        self.runtime.spawn(async move {
            match item.kind {
                DownloadKind::Http => {
                    let client = mgr.client.clone();
                    let limiter = mgr.limiter.clone();
                    let cat_subfolders = mgr.settings.read().categorize_into_subfolders;
                    let mgr_cb = mgr.clone();
                    let on_prog = Arc::new(move |snap: ProgressSnapshot| {
                        mgr_cb.apply_progress_snapshot(snap);
                    });
                    let mgr_res = mgr.clone();
                    let resolve_dest: DestinationResolver = Arc::new(
                        move |dl_id, dir, desired_name, keep_own_part| {
                            mgr_res.reserve_destination_for_item(
                                dl_id,
                                dir,
                                desired_name,
                                keep_own_part,
                            )
                        },
                    );

                    let res = run_http_download_with_options_and_resolver(
                        client.clone(),
                        item.clone(),
                        cat_subfolders,
                        false,
                        limiter.clone(),
                        cancel.clone(),
                        resolve_dest.clone(),
                        on_prog.clone(),
                    )
                    .await;

                    match res {
                        Ok(finished_item) => {
                            mgr.cancel_handles.write().remove(&id);
                            let is_done = finished_item.status == DownloadStatus::Completed;
                            let merged_item = mgr.replace_item(finished_item);
                            if is_done {
                                mgr.handle_post_download_completion(merged_item).await;
                            }
                        }
                        Err(e) if e.downcast_ref::<HtmlPageDetected>().is_some() => {
                            let indirect_res =
                                resolve_indirect_http_url(&client, &item.url, &item.headers).await;

                            match indirect_res {
                                Ok(Some(resolved)) => {
                                    let mut resolved_item = item.clone();
                                    resolved_item.url = resolved.url;
                                    resolved_item.headers = resolved.headers;
                                    if let Some(name) = resolved.probe.filename {
                                        let clean = sanitize_filename(&name);
                                        if !clean.is_empty() {
                                            resolved_item.category =
                                                FileCategory::from_filename(&clean);
                                            if cat_subfolders && resolved_item.package_id.is_none()
                                            {
                                                if let Some(parent) = resolved_item.save_dir.parent()
                                                {
                                                    if resolved_item
                                                        .save_dir
                                                        .ends_with(item.category.subfolder())
                                                    {
                                                        resolved_item.save_dir = parent.join(
                                                            resolved_item.category.subfolder(),
                                                        );
                                                    }
                                                }
                                            }
                                            let reserved_path = mgr.reserve_destination_for_item(
                                                resolved_item.id,
                                                &resolved_item.save_dir,
                                                &clean,
                                                false,
                                            );
                                            resolved_item.file_path = reserved_path;
                                            resolved_item.filename = resolved_item
                                                .file_path
                                                .file_name()
                                                .and_then(|n| n.to_str())
                                                .map(str::to_string)
                                                .unwrap_or(clean);
                                        }
                                    }
                                    mgr.replace_item(resolved_item.clone());
                                    let direct_res = run_http_download_with_options_and_resolver(
                                        client,
                                        resolved_item,
                                        cat_subfolders,
                                        false,
                                        limiter,
                                        cancel.clone(),
                                        resolve_dest.clone(),
                                        on_prog,
                                    )
                                    .await;
                                    mgr.cancel_handles.write().remove(&id);
                                    match direct_res {
                                        Ok(finished_item) => {
                                            let is_done =
                                                finished_item.status == DownloadStatus::Completed;
                                            let merged_item = mgr.replace_item(finished_item);
                                            if is_done {
                                                mgr.handle_post_download_completion(merged_item)
                                                    .await;
                                            }
                                        }
                                        Err(err) => mgr.mark_failed(id, err.to_string()),
                                    }
                                    return;
                                }
                                Err(cf_err) => {
                                    mgr.cancel_handles.write().remove(&id);
                                    mgr.mark_failed(id, cf_err.to_string());
                                    return;
                                }
                                Ok(None) => {}
                            }

                            let tools = detect_media_tools(&mgr.settings.read());
                            let media_probe = if let Some(ref yt_dlp) = tools.yt_dlp_path {
                                probe_single_media_url(yt_dlp, &item.url, &item.headers)
                                    .await
                                    .ok()
                                    .filter(|p| !p.formats.is_empty() || !p.title.is_empty())
                            } else {
                                None
                            };

                            if cancel.is_stopped() {
                                mgr.cancel_handles.write().remove(&id);
                                if cancel.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                                    mgr.downloads.write().retain(|d| d.id != id);
                                } else if let Some(slot) = mgr.downloads.write().iter_mut().find(|d| d.id == id) {
                                    slot.status = DownloadStatus::Paused;
                                    slot.speed_bps = 0;
                                    slot.eta_seconds = None;
                                }
                            } else if let Some(probe) = media_probe {
                                let mut media_item = item.clone();
                                media_item.kind = DownloadKind::Media;
                                media_item.category = FileCategory::Video;
                                if cat_subfolders {
                                    if let Some(parent) = media_item.save_dir.parent() {
                                        if media_item.save_dir.ends_with(item.category.subfolder()) {
                                            media_item.save_dir =
                                                parent.join(FileCategory::Video.subfolder());
                                        }
                                    }
                                }
                                let clean_title = sanitize_filename(&probe.title);
                                if !clean_title.is_empty() {
                                    media_item.filename = clean_title;
                                }
                                media_item.file_path = mgr.reserve_destination_for_item(
                                    media_item.id,
                                    &media_item.save_dir,
                                    &media_item.filename,
                                    false,
                                );
                                if let Some(name) = media_item.file_path.file_name().and_then(|n| n.to_str()) {
                                    media_item.filename = name.to_string();
                                }
                                media_item.media_info = Some(MediaDownloadMeta {
                                    format_id: None,
                                    quality_label: Some("Melhor".to_string()),
                                    title: Some(probe.title),
                                    duration_secs: probe.duration_secs,
                                    is_hls: false,
                                    is_dash: false,
                                    audio_only: false,
                                    embed_subtitles: false,
                                    extractor_used: "yt-dlp".into(),
                                });
                                mgr.replace_item(media_item.clone());

                                let mgr_cb = mgr.clone();
                                let media_res = run_media_download_with_resolver(
                                    client,
                                    media_item,
                                    tools.yt_dlp_path,
                                    tools.ffmpeg_path,
                                    cancel.clone(),
                                    Some(resolve_dest),
                                    move |status, filename, file_path, downloaded, total, speed, eta| {
                                        mgr_cb.apply_progress_snapshot(ProgressSnapshot {
                                            id,
                                            status,
                                            filename,
                                            file_path,
                                            category: None,
                                            total_bytes: total,
                                            downloaded_bytes: downloaded,
                                            speed_bps: speed,
                                            eta_seconds: eta,
                                            supports_ranges: None,
                                            segments: None,
                                            etag: None,
                                            last_modified: None,
                                            checksum_verified: None,
                                        });
                                    },
                                )
                                .await;

                                mgr.cancel_handles.write().remove(&id);
                                match media_res {
                                    Ok(finished_item) => {
                                        mgr.replace_item(finished_item);
                                    }
                                    Err(err) => mgr.mark_failed(id, err.to_string()),
                                }
                            } else {
                                let fallback_res = run_http_download_with_options_and_resolver(
                                    client,
                                    item.clone(),
                                    cat_subfolders,
                                    true,
                                    limiter,
                                    cancel.clone(),
                                    resolve_dest,
                                    on_prog,
                                )
                                .await;
                                mgr.cancel_handles.write().remove(&id);
                                match fallback_res {
                                    Ok(finished_item) => {
                                        let is_done = finished_item.status == DownloadStatus::Completed;
                                        let merged_item = mgr.replace_item(finished_item);
                                        if is_done {
                                            mgr.handle_post_download_completion(merged_item).await;
                                        }
                                    }
                                    Err(err) => mgr.mark_failed(id, err.to_string()),
                                }
                            }
                        }
                        Err(e) => {
                            mgr.cancel_handles.write().remove(&id);
                            mgr.mark_failed(id, e.to_string());
                        }
                    }
                }
                DownloadKind::Media => {
                    let client = mgr.client.clone();
                    let tools = detect_media_tools(&mgr.settings.read());
                    let mgr_cb = mgr.clone();
                    let mgr_res = mgr.clone();
                    let resolve_dest: DestinationResolver = Arc::new(
                        move |dl_id, dir, desired_name, keep_own_part| {
                            mgr_res.reserve_destination_for_item(
                                dl_id,
                                dir,
                                desired_name,
                                keep_own_part,
                            )
                        },
                    );

                    let res = run_media_download_with_resolver(
                        client,
                        item.clone(),
                        tools.yt_dlp_path,
                        tools.ffmpeg_path,
                        cancel.clone(),
                        Some(resolve_dest),
                        move |status, filename, file_path, downloaded, total, speed, eta| {
                            mgr_cb.apply_progress_snapshot(ProgressSnapshot {
                                id,
                                status,
                                filename,
                                file_path,
                                category: None,
                                total_bytes: total,
                                downloaded_bytes: downloaded,
                                speed_bps: speed,
                                eta_seconds: eta,
                                supports_ranges: None,
                                segments: None,
                                etag: None,
                                last_modified: None,
                                checksum_verified: None,
                            });
                        },
                    )
                    .await;

                    mgr.cancel_handles.write().remove(&id);
                    match res {
                        Ok(finished_item) => {
                            mgr.replace_item(finished_item);
                        }
                        Err(e) => {
                            mgr.mark_failed(id, e.to_string());
                        }
                    }
                }
                DownloadKind::Torrent => {
                    mgr.cancel_handles.write().remove(&id);
                }
            }
            mgr.persist_now();
            mgr.pump_queue();
        });
    }

    fn apply_progress_snapshot(&self, snap: ProgressSnapshot) {
        let mut dls = self.downloads.write();
        if let Some(item) = dls.iter_mut().find(|d| d.id == snap.id) {
            if let Some(st) = snap.status {
                item.status = st;
            }
            if let Some(name) = snap.filename {
                item.filename = name;
            }
            if let Some(fp) = snap.file_path {
                item.file_path = fp;
            }
            if let Some(cat) = snap.category {
                item.category = cat;
            }
            if snap.total_bytes.is_some() {
                item.total_bytes = snap.total_bytes;
            }
            item.downloaded_bytes = snap.downloaded_bytes;
            item.speed_bps = snap.speed_bps;
            item.eta_seconds = snap.eta_seconds;
            if let Some(sr) = snap.supports_ranges {
                item.supports_ranges = sr;
            }
            if let Some(segs) = snap.segments {
                item.segments = segs;
            }
            if snap.etag.is_some() {
                item.etag = snap.etag;
            }
            if snap.last_modified.is_some() {
                item.last_modified = snap.last_modified;
            }
            if snap.checksum_verified.is_some() {
                item.checksum_verified = snap.checksum_verified;
            }
        }
    }

    fn replace_item(&self, updated: DownloadItem) -> DownloadItem {
        let mut dls = self.downloads.write();
        if let Some(slot) = dls.iter_mut().find(|d| d.id == updated.id) {
            let merged = merge_completed_download_item(slot, updated);
            *slot = merged.clone();
            return merged;
        }
        updated
    }

    fn reserved_paths_for_other_items(
        downloads: &[DownloadItem],
        exclude_id: Option<DownloadId>,
    ) -> Vec<PathBuf> {
        downloads
            .iter()
            .filter(|d| {
                Some(d.id) != exclude_id
                    && !matches!(
                        d.status,
                        DownloadStatus::Completed | DownloadStatus::Failed(_)
                    )
            })
            .map(|d| d.file_path.clone())
            .collect()
    }

    fn reserve_unique_path_locked(
        downloads: &[DownloadItem],
        exclude_id: Option<DownloadId>,
        save_dir: &Path,
        desired_filename: &str,
    ) -> PathBuf {
        let reserved = Self::reserved_paths_for_other_items(downloads, exclude_id);
        unique_destination_path_with_reserved(save_dir, desired_filename, &reserved)
    }

    fn reserve_destination_for_item(
        &self,
        id: DownloadId,
        save_dir: &Path,
        desired_filename: &str,
        keep_own_part: bool,
    ) -> PathBuf {
        let mut dls = self.downloads.write();
        let reserved = Self::reserved_paths_for_other_items(&dls, Some(id));
        if keep_own_part {
            if let Some(existing) = dls.iter().find(|d| d.id == id) {
                let own_part_ok = paths_conflict(&existing.save_dir, save_dir)
                    && !existing.file_path.exists()
                    && part_file_path(&existing.file_path).exists()
                    && !reserved
                        .iter()
                        .any(|r| paths_conflict(r, &existing.file_path));
                if own_part_ok {
                    return existing.file_path.clone();
                }
            }
        }
        let chosen = unique_destination_path_with_reserved(save_dir, desired_filename, &reserved);
        if let Some(item) = dls.iter_mut().find(|d| d.id == id) {
            item.save_dir = save_dir.to_path_buf();
            item.file_path = chosen.clone();
            if let Some(name) = chosen.file_name().and_then(|n| n.to_str()) {
                item.filename = name.to_string();
            }
        }
        chosen
    }

    fn mark_failed(&self, id: DownloadId, err: String) {
        let mut dls = self.downloads.write();
        if let Some(item) = dls.iter_mut().find(|d| d.id == id) {
            item.status = DownloadStatus::Failed(err.clone());
            item.error = Some(err);
            item.speed_bps = 0;
            item.eta_seconds = None;
        }
    }

    async fn handle_post_download_completion(self: &Arc<Self>, item: DownloadItem) {
        let item = self
            .downloads
            .read()
            .iter()
            .find(|d| d.id == item.id)
            .cloned()
            .unwrap_or(item);
        // Case 1: Part of a multi-part Package -> check if ALL parts of that package are now Completed!
        if let Some(pkg_id) = item.package_id {
            let pkg_info = {
                let pkgs = self.packages.read();
                let dls = self.downloads.read();
                pkgs.iter().find(|p| p.id == pkg_id).map(|p| {
                    let parts: Vec<DownloadItem> = p
                        .item_ids
                        .iter()
                        .filter_map(|iid| dls.iter().find(|d| d.id == *iid).cloned())
                        .collect();
                    (p.clone(), parts)
                })
            };

            if let Some((pkg, mut parts)) = pkg_info {
                let all_done = !parts.is_empty()
                    && parts
                        .iter()
                        .all(|d| d.status == DownloadStatus::Completed);
                if all_done && pkg.auto_extract {
                    parts.sort_by_key(|p| p.part_index.unwrap_or(9999));
                    let first_part = parts
                        .iter()
                        .find(|p| {
                            detect_multipart(&p.filename)
                                .map(|m| m.is_first_part)
                                .unwrap_or(false)
                        })
                        .or_else(|| parts.first())
                        .cloned();

                    if let Some(first) = first_part {
                        let _ = self
                            .run_extraction_for_target(
                                Some(pkg.id),
                                first.id,
                                first.file_path.clone(),
                                pkg.save_dir.clone(),
                                pkg.extractor_id.clone(),
                                pkg.extract_password.clone(),
                                pkg.delete_archives_after_extract,
                                parts.into_iter().map(|p| p.file_path).collect(),
                            )
                            .await;
                    }
                }
            }
            return;
        }

        // Case 2: Standalone archive
        if item.auto_extract && is_extractable_archive(&item.file_path) {
            let delete_after = self.settings.read().delete_archives_after_extract;
            let _ = self
                .run_extraction_for_target(
                    None,
                    item.id,
                    item.file_path.clone(),
                    item.save_dir.clone(),
                    item.extractor_override.clone(),
                    item.extract_password.clone(),
                    delete_after,
                    vec![item.file_path.clone()],
                )
                .await;
        }
    }

    pub fn set_extractor_override(
        &self,
        id: DownloadId,
        extractor_id: Option<String>,
        auto_extract: bool,
    ) {
        let package_id = self
            .downloads
            .read()
            .iter()
            .find(|x| x.id == id)
            .and_then(|x| x.package_id);

        if let Some(pid) = package_id {
            self.set_package_extractor(pid, extractor_id, auto_extract);
            return;
        }

        {
            let mut dls = self.downloads.write();
            if let Some(d) = dls.iter_mut().find(|x| x.id == id) {
                d.extractor_override = extractor_id;
                d.auto_extract = auto_extract;
            }
        }
        self.persist_now();
    }

    pub fn set_package_extractor(
        &self,
        pkg_id: PackageId,
        extractor_id: Option<String>,
        auto_extract: bool,
    ) {
        let item_ids: Vec<DownloadId> = {
            let mut pkgs = self.packages.write();
            if let Some(p) = pkgs.iter_mut().find(|x| x.id == pkg_id) {
                p.extractor_id = extractor_id.clone();
                p.auto_extract = auto_extract;
                p.item_ids.clone()
            } else {
                Vec::new()
            }
        };
        if !item_ids.is_empty() {
            let mut dls = self.downloads.write();
            for d in dls.iter_mut() {
                if item_ids.contains(&d.id) {
                    d.extractor_override = extractor_id.clone();
                }
            }
        }
        self.persist_now();
    }

    pub fn set_download_password(&self, id: DownloadId, password: Option<String>) {
        let clean = password.and_then(|p| {
            let t = p.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        });
        let mut package_id = None;
        {
            let mut dls = self.downloads.write();
            if let Some(d) = dls.iter_mut().find(|x| x.id == id) {
                d.extract_password = clean.clone();
                package_id = d.package_id;
            }
        }
        if let Some(pid) = package_id {
            self.set_package_password(pid, clean);
        } else {
            self.persist_now();
        }
    }

    pub fn set_package_password(&self, pkg_id: PackageId, password: Option<String>) {
        let clean = password.and_then(|p| {
            let t = p.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        });
        let item_ids: Vec<DownloadId> = {
            let mut pkgs = self.packages.write();
            if let Some(p) = pkgs.iter_mut().find(|x| x.id == pkg_id) {
                p.extract_password = clean.clone();
                p.item_ids.clone()
            } else {
                Vec::new()
            }
        };
        if !item_ids.is_empty() {
            let mut dls = self.downloads.write();
            for d in dls.iter_mut() {
                if item_ids.contains(&d.id) {
                    d.extract_password = clean.clone();
                }
            }
        }
        self.persist_now();
    }

    pub fn dismiss_password_prompt(&self, prompt_id: Uuid) {
        self.pending_password_prompts
            .write()
            .retain(|p| p.id != prompt_id);
    }

    pub fn submit_extraction_password(
        self: &Arc<Self>,
        prompt_id: Uuid,
        password: String,
        save_to_list: bool,
    ) {
        let prompt_opt = {
            let mut prompts = self.pending_password_prompts.write();
            let pos = prompts.iter().position(|p| p.id == prompt_id);
            pos.map(|idx| prompts.remove(idx))
        };
        let Some(prompt) = prompt_opt else {
            return;
        };

        let trimmed = password.trim().to_string();
        if trimmed.is_empty() {
            return;
        }

        if save_to_list {
            let mut s = self.settings.write();
            if !s.password_list.iter().any(|x| x == &trimmed) {
                s.password_list.push(trimmed.clone());
                let _ = save_settings(&s);
            }
        }

        if let Some(pkg_id) = prompt.package_id {
            self.set_package_password(pkg_id, Some(trimmed.clone()));
            self.trigger_package_extract(pkg_id, Some(trimmed));
        } else {
            self.set_download_password(prompt.item_id, Some(trimmed.clone()));
            let item_opt = self
                .downloads
                .read()
                .iter()
                .find(|d| d.id == prompt.item_id)
                .cloned();
            if let Some(item) = item_opt {
                let mgr = self.clone();
                let delete_after = self.settings.read().delete_archives_after_extract;
                self.runtime.spawn(async move {
                    let _ = mgr
                        .run_extraction_for_target(
                            None,
                            item.id,
                            item.file_path.clone(),
                            item.save_dir.clone(),
                            item.extractor_override,
                            Some(trimmed),
                            delete_after,
                            vec![item.file_path],
                        )
                        .await;
                });
            }
        }
    }

    pub fn trigger_package_extract(
        self: &Arc<Self>,
        pkg_id: PackageId,
        password_override: Option<String>,
    ) {
        let pkg_info = {
            let pkgs = self.packages.read();
            let dls = self.downloads.read();
            pkgs.iter().find(|p| p.id == pkg_id).map(|p| {
                let parts: Vec<DownloadItem> = p
                    .item_ids
                    .iter()
                    .filter_map(|iid| dls.iter().find(|d| d.id == *iid).cloned())
                    .collect();
                (p.clone(), parts)
            })
        };
        let Some((pkg, mut parts)) = pkg_info else {
            return;
        };
        if parts.is_empty() {
            return;
        }
        if !parts
            .iter()
            .all(|d| d.status == DownloadStatus::Completed)
        {
            self.push_error_notice(format!(
                "Aguarde todas as partes de \"{}\" terminarem antes de extrair.",
                pkg.name
            ));
            return;
        }
        parts.sort_by_key(|p| p.part_index.unwrap_or(9999));
        let first_part = parts
            .iter()
            .find(|p| {
                detect_multipart(&p.filename)
                    .map(|m| m.is_first_part)
                    .unwrap_or(false)
            })
            .or_else(|| parts.first())
            .cloned();
        let Some(first) = first_part else {
            return;
        };

        let password = password_override
            .or(pkg.extract_password.clone())
            .or(first.extract_password.clone());
        let mgr = self.clone();
        self.runtime.spawn(async move {
            let _ = mgr
                .run_extraction_for_target(
                    Some(pkg.id),
                    first.id,
                    first.file_path.clone(),
                    pkg.save_dir.clone(),
                    pkg.extractor_id.clone(),
                    password,
                    pkg.delete_archives_after_extract,
                    parts.into_iter().map(|p| p.file_path).collect(),
                )
                .await;
        });
    }

    pub fn trigger_manual_extract(self: &Arc<Self>, id: DownloadId, extractor_id: Option<String>) {
        let item_opt = self.downloads.read().iter().find(|d| d.id == id).cloned();
        let Some(item) = item_opt else {
            return;
        };
        if let Some(pkg_id) = item.package_id {
            if let Some(ext_id) = extractor_id {
                self.set_extractor_override(id, Some(ext_id), item.auto_extract);
            }
            self.trigger_package_extract(pkg_id, None);
            return;
        }
        let mgr = self.clone();
        let delete_after = self.settings.read().delete_archives_after_extract;
        self.runtime.spawn(async move {
            let _ = mgr
                .run_extraction_for_target(
                    None,
                    item.id,
                    item.file_path.clone(),
                    item.save_dir.clone(),
                    extractor_id.or(item.extractor_override),
                    item.extract_password,
                    delete_after,
                    vec![item.file_path],
                )
                .await;
        });
    }

    async fn run_extraction_for_target(
        &self,
        package_id: Option<PackageId>,
        item_id: DownloadId,
        archive_path: PathBuf,
        dest_dir: PathBuf,
        extractor_override: Option<String>,
        explicit_password: Option<String>,
        delete_archives: bool,
        files_to_delete: Vec<PathBuf>,
    ) -> Result<()> {
        let settings = self.settings.read().clone();
        let Some(profile) =
            select_best_extractor(&settings, extractor_override.as_deref()).cloned()
        else {
            return Err(anyhow!("Nenhum utilitário de extração configurado/detectado"));
        };

        let initial_progress = match profile.kind {
            crate::model::ExtractorKind::NanaZip
            | crate::model::ExtractorKind::SevenZip
            | crate::model::ExtractorKind::PeaZip => Some(0.0),
            _ => None,
        };

        {
            let mut dls = self.downloads.write();
            if let Some(d) = dls.iter_mut().find(|x| x.id == item_id) {
                d.status = DownloadStatus::Extracting;
                d.extraction_status = Some(format!("Extraindo com {}...", profile.name));
                d.extraction_progress = initial_progress;
            }
        }
        if let Some(pid) = package_id {
            let mut pkgs = self.packages.write();
            if let Some(p) = pkgs.iter_mut().find(|x| x.id == pid) {
                p.extraction_status = Some(format!("Extraindo com {}...", profile.name));
                p.extraction_progress = initial_progress;
            }
        }

        let downloads_ref = self.downloads.clone();
        let packages_ref = self.packages.clone();
        let progress_cb: ProgressCallback = Arc::new(move |frac: f32| {
            let clamped = frac.clamp(0.0, 1.0);
            {
                let mut dls = downloads_ref.write();
                if let Some(d) = dls.iter_mut().find(|x| x.id == item_id) {
                    d.extraction_progress = Some(clamped);
                }
            }
            if let Some(pid) = package_id {
                let mut pkgs = packages_ref.write();
                if let Some(p) = pkgs.iter_mut().find(|x| x.id == pid) {
                    p.extraction_progress = Some(clamped);
                }
            }
        });

        let res = extract_archive_with_progress(
            &archive_path,
            &dest_dir,
            &profile,
            explicit_password.as_deref(),
            &settings.password_list,
            settings.extract_into_subfolder,
            Some(progress_cb),
        )
        .await;

        match res {
            Ok(out_dir) => {
                if delete_archives {
                    for f in &files_to_delete {
                        let _ = tokio::fs::remove_file(f).await;
                    }
                }
                {
                    let mut dls = self.downloads.write();
                    if let Some(d) = dls.iter_mut().find(|x| x.id == item_id) {
                        d.status = DownloadStatus::Completed;
                        d.extracted_to = Some(out_dir.clone());
                        d.extraction_status = Some(format!("Extraído via {}", profile.name));
                        d.extraction_progress = None;
                    }
                }
                if let Some(pid) = package_id {
                    let mut pkgs = self.packages.write();
                    if let Some(p) = pkgs.iter_mut().find(|x| x.id == pid) {
                        p.extracted_to = Some(out_dir.clone());
                        p.extraction_status = Some(format!("Extraído via {}", profile.name));
                        p.extraction_progress = None;
                    }
                }
                self.push_notice(format!(
                    "Extração concluída ({}) em {}",
                    profile.name,
                    out_dir.display()
                ));
                self.persist_now();
                Ok(())
            }
            Err(e) => {
                if let Some(pw_err) = e.downcast_ref::<ExtractionPasswordRequired>() {
                    let display_name = package_id
                        .and_then(|pid| {
                            self.packages
                                .read()
                                .iter()
                                .find(|p| p.id == pid)
                                .map(|p| p.name.clone())
                        })
                        .unwrap_or_else(|| {
                            archive_path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "Arquivo".to_string())
                        });
                    let status_msg = if pw_err.wrong_password {
                        "Senha incorreta — informe a senha para extrair".to_string()
                    } else {
                        "Senha necessária para extração".to_string()
                    };
                    {
                        let mut dls = self.downloads.write();
                        if let Some(d) = dls.iter_mut().find(|x| x.id == item_id) {
                            d.status = DownloadStatus::Completed;
                            d.extraction_status = Some(status_msg.clone());
                            d.extraction_progress = None;
                        }
                    }
                    if let Some(pid) = package_id {
                        let mut pkgs = self.packages.write();
                        if let Some(p) = pkgs.iter_mut().find(|x| x.id == pid) {
                            p.extraction_status = Some(status_msg.clone());
                            p.extraction_progress = None;
                        }
                    }
                    {
                        let mut prompts = self.pending_password_prompts.write();
                        let already_queued = prompts.iter().any(|p| {
                            p.item_id == item_id
                                || (package_id.is_some() && p.package_id == package_id)
                        });
                        if !already_queued {
                            prompts.push(PasswordPromptRequest {
                                id: Uuid::new_v4(),
                                item_id,
                                package_id,
                                display_name: display_name.clone(),
                                archive_path: archive_path.clone(),
                                dest_dir: dest_dir.clone(),
                                extractor_override: extractor_override.clone(),
                                delete_archives,
                                files_to_delete: files_to_delete.clone(),
                                wrong_password: pw_err.wrong_password,
                            });
                        }
                    }
                    self.push_error_notice(format!(
                        "{}: {}",
                        status_msg, display_name
                    ));
                    self.persist_now();
                    return Err(e);
                }

                let msg = format!("Erro na extração ({}): {e}", profile.name);
                {
                    let mut dls = self.downloads.write();
                    if let Some(d) = dls.iter_mut().find(|x| x.id == item_id) {
                        d.status = DownloadStatus::Completed;
                        d.extraction_status = Some(msg.clone());
                        d.extraction_progress = None;
                    }
                }
                if let Some(pid) = package_id {
                    let mut pkgs = self.packages.write();
                    if let Some(p) = pkgs.iter_mut().find(|x| x.id == pid) {
                        p.extraction_status = Some(msg.clone());
                        p.extraction_progress = None;
                    }
                }
                self.push_error_notice(msg);
                self.persist_now();
                Err(e)
            }
        }
    }

    pub fn pause_download(&self, id: DownloadId) {
        if let Some(cancel) = self.cancel_handles.read().get(&id) {
            cancel.pause();
        }
        let kind = {
            let mut dls = self.downloads.write();
            if let Some(d) = dls.iter_mut().find(|x| x.id == id) {
                if matches!(d.status, DownloadStatus::Queued | DownloadStatus::Probing) {
                    d.status = DownloadStatus::Paused;
                }
                d.speed_bps = 0;
                d.eta_seconds = None;
                Some(d.kind)
            } else {
                None
            }
        };
        if kind == Some(DownloadKind::Torrent) {
            let mgr_te = self.torrent_engine.clone();
            self.runtime.spawn(async move {
                if let Some(te) = mgr_te.lock().await.as_ref() {
                    let _ = te.pause(id).await;
                }
            });
        }
        self.persist_now();
    }

    pub fn resume_download(self: &Arc<Self>, id: DownloadId) {
        let torrent_attach_info = {
            let mut dls = self.downloads.write();
            if let Some(d) = dls.iter_mut().find(|x| x.id == id) {
                d.error = None;
                if matches!(d.status, DownloadStatus::Paused | DownloadStatus::Failed(_)) {
                    d.status = if d.kind == DownloadKind::Torrent {
                        DownloadStatus::Probing
                    } else {
                        DownloadStatus::Queued
                    };
                }
                if d.kind == DownloadKind::Torrent {
                    let source = d
                        .torrent_info
                        .as_ref()
                        .map(|ti| ti.magnet_or_path.clone())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| d.url.clone());
                    Some((true, source, d.save_dir.clone(), d.headers.clone()))
                } else {
                    Some((false, String::new(), PathBuf::new(), HashMap::new()))
                }
            } else {
                None
            }
        };
        if let Some((true, source, save_dir, headers)) = torrent_attach_info {
            let mgr = self.clone();
            self.runtime.spawn(async move {
                let has_handle = {
                    let guard = mgr.torrent_engine.lock().await;
                    guard.as_ref().map(|te| te.has_handle(id)).unwrap_or(false)
                };
                if has_handle {
                    if let Some(te) = mgr.torrent_engine.lock().await.as_ref() {
                        let _ = te.resume(id).await;
                    }
                } else if !source.is_empty() {
                    mgr.spawn_torrent_attach_task(id, source, save_dir, headers, false);
                }
            });
        } else {
            self.pump_queue();
        }
        self.persist_now();
    }

    pub fn pause_all(&self) {
        let ids: Vec<DownloadId> = self
            .downloads
            .read()
            .iter()
            .filter(|d| d.status.is_active() || d.status == DownloadStatus::Queued)
            .map(|d| d.id)
            .collect();
        for id in ids {
            self.pause_download(id);
        }
    }

    pub fn resume_all(self: &Arc<Self>) {
        let ids: Vec<DownloadId> = self
            .downloads
            .read()
            .iter()
            .filter(|d| matches!(d.status, DownloadStatus::Paused | DownloadStatus::Failed(_)))
            .map(|d| d.id)
            .collect();
        for id in ids {
            self.resume_download(id);
        }
    }

    pub fn adjust_download_priority(self: &Arc<Self>, id: DownloadId, delta: i32) -> Option<i32> {
        let updated = {
            let mut dls = self.downloads.write();
            let item = dls.iter_mut().find(|d| d.id == id)?;
            let next = (item.priority + delta)
                .clamp(crate::model::MIN_DOWNLOAD_PRIORITY, crate::model::MAX_DOWNLOAD_PRIORITY);
            if next == item.priority {
                return Some(next);
            }
            item.priority = next;
            next
        };
        self.persist_now();
        self.pump_queue();
        Some(updated)
    }

    pub fn remove_download(&self, id: DownloadId, delete_disk_file: bool) {
        let was_active = {
            let handles = self.cancel_handles.read();
            if let Some(cancel) = handles.get(&id) {
                if delete_disk_file {
                    cancel.delete_on_cancel.store(true, Ordering::SeqCst);
                }
                cancel.cancel();
                true
            } else {
                false
            }
        };
        let removed = {
            let mut dls = self.downloads.write();
            if let Some(pos) = dls.iter().position(|d| d.id == id) {
                Some(dls.remove(pos))
            } else {
                None
            }
        };

        if let Some(item) = removed {
            if item.kind == DownloadKind::Torrent {
                let mgr_te = self.torrent_engine.clone();
                let item_clone = item.clone();
                let handled_sync = if delete_disk_file {
                    if let Ok(guard) = mgr_te.try_lock() {
                        if guard.is_none() {
                            delete_torrent_files_from_disk(&item_clone);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !handled_sync {
                    self.runtime.spawn(async move {
                        let deleted_by_engine = if let Some(te) = mgr_te.lock().await.as_ref() {
                            te.remove(id, delete_disk_file).await.unwrap_or(false)
                        } else {
                            false
                        };
                        if delete_disk_file && !deleted_by_engine {
                            delete_torrent_files_from_disk(&item_clone);
                        }
                    });
                }
            } else if delete_disk_file {
                if !was_active && delete_non_torrent_files_from_disk(&item) {
                    // Deleted immediately without waiting for active workers.
                } else {
                    let cancel_handles = self.cancel_handles.clone();
                    let item_clone = item.clone();
                    self.runtime.spawn(async move {
                        for _ in 0..100 {
                            if !cancel_handles.read().contains_key(&id) {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                        }
                        for attempt in 0..6 {
                            if delete_non_torrent_files_from_disk(&item_clone) || attempt == 5 {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                        }
                    });
                }
            }
            self.pending_password_prompts
                .write()
                .retain(|p| p.id != id);
            let mut pkgs = self.packages.write();
            for p in pkgs.iter_mut() {
                p.item_ids.retain(|x| *x != id);
            }
            pkgs.retain(|p| !p.item_ids.is_empty());
        }
        self.persist_now();
    }

    pub fn remove_package(&self, pkg_id: PackageId, delete_disk_files: bool) {
        let item_ids: Vec<DownloadId> = {
            let pkgs = self.packages.read();
            pkgs.iter()
                .find(|p| p.id == pkg_id)
                .map(|p| p.item_ids.clone())
                .unwrap_or_default()
        };
        for item_id in item_ids {
            self.remove_download(item_id, delete_disk_files);
        }
        self.pending_password_prompts
            .write()
            .retain(|p| p.id != pkg_id);
        self.packages.write().retain(|p| p.id != pkg_id);
        self.persist_now();
    }

    pub fn set_torrent_files(self: &Arc<Self>, id: DownloadId, included: HashSet<usize>) {
        let mgr_te = self.torrent_engine.clone();
        self.runtime.spawn(async move {
            if let Some(te) = mgr_te.lock().await.as_ref() {
                let _ = te.set_only_files(id, included).await;
            }
        });
    }

    pub fn install_yt_dlp_async(self: &Arc<Self>) {
        let mgr = self.clone();
        let client = self.client.clone();
        self.push_notice("Baixando yt-dlp mais recente...");
        self.runtime.spawn(async move {
            match install_yt_dlp(&client).await {
                Ok(path) => {
                    mgr.settings.write().yt_dlp_path = Some(path.to_string_lossy().to_string());
                    let _ = mgr.save_current_settings();
                    mgr.push_notice(format!("yt-dlp instalado em {}", path.display()));
                }
                Err(e) => {
                    mgr.push_notice(format!("Falha ao instalar yt-dlp: {e}"));
                }
            }
        });
    }

    pub fn install_ffmpeg_async(self: &Arc<Self>) {
        let mgr = self.clone();
        let client = self.client.clone();
        self.push_notice("Baixando FFmpeg essentials (pode levar ~30s)...");
        self.runtime.spawn(async move {
            match install_ffmpeg_essentials(&client).await {
                Ok(path) => {
                    mgr.settings.write().ffmpeg_path = Some(path.to_string_lossy().to_string());
                    let _ = mgr.save_current_settings();
                    mgr.push_notice(format!("FFmpeg instalado em {}", path.display()));
                }
                Err(e) => {
                    mgr.push_notice(format!("Falha ao instalar FFmpeg: {e}"));
                }
            }
        });
    }

    pub async fn probe_media(&self, url: &str) -> Result<MediaProbeResult> {
        let tools = detect_media_tools(&self.settings.read());
        let yt_dlp = tools
            .yt_dlp_path
            .ok_or_else(|| anyhow!("yt-dlp não encontrado. Instale com 1 clique na aba Mídia ou Configurações."))?;
        probe_media_url(&yt_dlp, url, &HashMap::new()).await
    }

    pub async fn probe_video_for_compression(&self, path: &Path) -> Result<VideoProbeInfo> {
        let tools = detect_media_tools(&self.settings.read());
        probe_video_file(path, tools.ffmpeg_path.as_deref()).await
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_video_compression(
        self: &Arc<Self>,
        probe: VideoProbeInfo,
        target_ratio: f32,
        codec: VideoCompressionCodec,
        auto_downscale: bool,
        replace_original: bool,
        source_download_id: Option<DownloadId>,
    ) -> Result<Uuid> {
        let tools = detect_media_tools(&self.settings.read());
        let ffmpeg_path = tools.ffmpeg_path.ok_or_else(|| {
            anyhow!("FFmpeg não encontrado. Instale com 1 clique na barra de ferramentas antes de comprimir.")
        })?;

        let plan = plan_video_compression(&probe, target_ratio, codec, auto_downscale);
        let output_path = if replace_original {
            let parent = probe.path.parent().unwrap_or_else(|| Path::new("."));
            let stem = probe
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("video");
            parent.join(format!("{stem}.mp4"))
        } else {
            unique_compressed_output_path(&probe.path)
        };

        let job_id = Uuid::new_v4();
        let snapshot = CompressionJobSnapshot {
            id: job_id,
            input_path: probe.path.clone(),
            output_path: output_path.clone(),
            input_filename: probe.filename.clone(),
            original_size_bytes: probe.file_size_bytes,
            target_size_bytes: plan.estimated_bytes,
            final_size_bytes: None,
            progress: 0.0,
            speed_x: None,
            eta_seconds: None,
            status: CompressionJobStatus::Preparing,
            plan_summary: plan.quality_note.clone(),
        };

        let cancel = Arc::new(AtomicBool::new(false));
        self.compression_cancels.write().insert(job_id, cancel.clone());
        {
            let mut jobs = self.compression_jobs.write();
            jobs.insert(0, snapshot);
            if jobs.len() > 24 {
                jobs.retain(|j| j.status.is_active());
            }
        }
        self.request_ui_repaint();

        let mgr = self.clone();
        self.runtime.spawn(async move {
            let mgr_cb = mgr.clone();
            let last_repaint = Arc::new(parking_lot::Mutex::new(std::time::Instant::now()));
            let res = compress_video_file(
                &ffmpeg_path,
                job_id,
                &probe.path,
                &output_path,
                &probe,
                &plan,
                replace_original,
                cancel.clone(),
                move |status, progress, speed_x, eta_secs| {
                    {
                        let mut jobs = mgr_cb.compression_jobs.write();
                        if let Some(j) = jobs.iter_mut().find(|x| x.id == job_id) {
                            j.status = status;
                            j.progress = progress.clamp(0.0, 1.0);
                            j.speed_x = speed_x;
                            j.eta_seconds = eta_secs;
                        }
                    }
                    let mut last = last_repaint.lock();
                    if last.elapsed().as_millis() >= 120 {
                        *last = std::time::Instant::now();
                        mgr_cb.request_ui_repaint();
                    }
                },
            )
            .await;

            mgr.compression_cancels.write().remove(&job_id);
            match res {
                Ok((final_path, final_size)) => {
                    {
                        let mut jobs = mgr.compression_jobs.write();
                        if let Some(j) = jobs.iter_mut().find(|x| x.id == job_id) {
                            j.output_path = final_path.clone();
                            j.final_size_bytes = Some(final_size);
                            j.status = CompressionJobStatus::Completed;
                            j.progress = 1.0;
                            j.speed_x = None;
                            j.eta_seconds = Some(0);
                        }
                    }
                    if replace_original {
                        if let Some(dl_id) = source_download_id {
                            let mut dls = mgr.downloads.write();
                            if let Some(item) = dls.iter_mut().find(|d| d.id == dl_id) {
                                item.file_path = final_path.clone();
                                if let Some(name) = final_path.file_name().and_then(|n| n.to_str()) {
                                    item.filename = name.to_string();
                                }
                                item.total_bytes = Some(final_size);
                                item.downloaded_bytes = final_size;
                            }
                            drop(dls);
                            mgr.persist_now();
                        }
                    }
                    let saved_pct = if probe.file_size_bytes > 0 && final_size < probe.file_size_bytes {
                        (((probe.file_size_bytes - final_size) as f64 / probe.file_size_bytes as f64) * 100.0).round() as u64
                    } else {
                        0
                    };
                    mgr.push_notice(format!(
                        "Vídeo comprimido (-{}%): {}",
                        saved_pct,
                        final_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("video.mp4")
                    ));
                    mgr.request_ui_repaint();
                }
                Err(e) => {
                    let cancelled = cancel.load(Ordering::Relaxed);
                    {
                        let mut jobs = mgr.compression_jobs.write();
                        if let Some(j) = jobs.iter_mut().find(|x| x.id == job_id) {
                            j.status = if cancelled {
                                CompressionJobStatus::Cancelled
                            } else {
                                CompressionJobStatus::Failed(e.to_string())
                            };
                            j.speed_x = None;
                            j.eta_seconds = None;
                        }
                    }
                    if cancelled {
                        mgr.push_notice(format!("Compressão cancelada: {}", probe.filename));
                    } else {
                        mgr.push_error_notice(format!("Falha ao comprimir {}: {e}", probe.filename));
                    }
                    mgr.request_ui_repaint();
                }
            }
        });

        Ok(job_id)
    }

    pub fn cancel_video_compression(&self, job_id: Uuid) {
        if let Some(flag) = self.compression_cancels.read().get(&job_id) {
            flag.store(true, Ordering::Relaxed);
        }
        self.request_ui_repaint();
    }

    pub fn dismiss_compression_job(&self, job_id: Uuid) {
        self.compression_jobs
            .write()
            .retain(|j| j.id != job_id || j.status.is_active());
        self.request_ui_repaint();
    }
}

pub fn default_torrent_dir_from_settings(settings: &AppSettings) -> PathBuf {
    if settings.categorize_into_subfolders {
        settings.default_download_dir.join("Torrents")
    } else {
        settings.default_download_dir.clone()
    }
}

pub fn is_torrent_request(url: &str, filename_hint: Option<&str>) -> bool {
    let trimmed = url.trim();
    if trimmed
        .get(..7)
        .map(|p| p.eq_ignore_ascii_case("magnet:"))
        .unwrap_or(false)
    {
        return true;
    }
    let path_part = trimmed.split(['?', '#']).next().unwrap_or("");
    if path_part.to_ascii_lowercase().ends_with(".torrent") {
        return true;
    }
    filename_hint
        .map(|f| f.trim().to_ascii_lowercase().ends_with(".torrent"))
        .unwrap_or(false)
}

fn select_queued_downloads_to_start(dls: &[DownloadItem], max_concurrent: usize) -> Vec<DownloadId> {
    const MAX_ACTIVE_PER_HOST: usize = 2;
    let max_concurrent = max_concurrent.max(1);
    let mut active_total = 0usize;
    let mut active_per_host: HashMap<String, usize> = HashMap::new();
    for d in dls.iter() {
        if matches!(
            d.status,
            DownloadStatus::Probing | DownloadStatus::Downloading | DownloadStatus::Merging
        ) {
            active_total += 1;
            if d.kind == DownloadKind::Http {
                if let Some(host) = url::Url::parse(&d.url)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
                {
                    *active_per_host.entry(host).or_insert(0) += 1;
                }
            }
        }
    }
    if active_total >= max_concurrent {
        return Vec::new();
    }
    let mut remaining_slots = max_concurrent - active_total;
    let mut queued: Vec<&DownloadItem> = dls
        .iter()
        .filter(|d| d.status == DownloadStatus::Queued)
        .collect();
    queued.sort_by(|a, b| b.priority.cmp(&a.priority));

    let mut chosen = Vec::new();
    for d in queued {
        if remaining_slots == 0 {
            break;
        }
        if d.kind == DownloadKind::Http {
            if let Some(host) = url::Url::parse(&d.url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            {
                let count = active_per_host.get(&host).copied().unwrap_or(0);
                if count >= MAX_ACTIVE_PER_HOST {
                    continue;
                }
                active_per_host.insert(host, count + 1);
            }
        }
        chosen.push(d.id);
        remaining_slots -= 1;
    }
    chosen
}

fn merge_completed_download_item(live_slot: &DownloadItem, mut updated: DownloadItem) -> DownloadItem {
    updated.auto_extract = live_slot.auto_extract;
    updated.extractor_override = live_slot.extractor_override.clone();
    updated.extract_password = live_slot.extract_password.clone();
    updated.package_id = live_slot.package_id;
    updated.part_index = live_slot.part_index;
    updated.part_total = live_slot.part_total;
    updated.extracted_to = live_slot.extracted_to.clone();
    updated.extraction_status = live_slot.extraction_status.clone();
    updated.extraction_progress = live_slot.extraction_progress;
    updated.priority = live_slot.priority;
    updated
}

pub fn is_valid_grabbed_link(link: &GrabbedLink) -> bool {
    link.part_number.is_some()
        && link.online == Some(true)
        && link.size_bytes.is_some_and(|s| s > 0)
}

pub fn should_keep_grabbed_link(link: &GrabbedLink, ignore_non_part: bool) -> bool {
    !ignore_non_part || link.part_number.is_some()
}

pub fn normalize_browser_page_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Ok(mut parsed) = url::Url::parse(trimmed) {
        parsed.set_fragment(None);
        let mut s = parsed.to_string();
        if s.ends_with('/') && parsed.path().len() > 1 && parsed.query().is_none() {
            s.pop();
        }
        return s;
    }
    trimmed
        .split('#')
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

pub fn browser_page_url_matches(link_url: &str, reported_page_url: &str) -> bool {
    let a = normalize_browser_page_url(link_url);
    let b = normalize_browser_page_url(reported_page_url);
    if a == b {
        return true;
    }
    if let (Ok(ua), Ok(ub)) = (url::Url::parse(&a), url::Url::parse(&b)) {
        if let (Some(ha), Some(hb)) = (ua.host_str(), ub.host_str()) {
            if is_vikingfile_host(ha) && is_vikingfile_host(hb) {
                let pa = ua.path().trim_end_matches('/');
                let pb = ub.path().trim_end_matches('/');
                return !pa.is_empty() && pa == pb && ua.query() == ub.query();
            }
            if is_fileditch_host(ha) && is_fileditch_host(hb) {
                let pa = ua.path().trim_end_matches('/');
                let pb = ub.path().trim_end_matches('/');
                let qa: Vec<(String, String)> = ua
                    .query_pairs()
                    .filter(|(k, _)| k != "fdg")
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                let qb: Vec<(String, String)> = ub
                    .query_pairs()
                    .filter(|(k, _)| k != "fdg")
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                return !pa.is_empty() && pa == pb && qa == qb;
            }
        }
    }
    false
}

pub fn is_browser_resolvable_link(link: &GrabbedLink) -> bool {
    if link.online == Some(true) {
        return false;
    }
    if let Ok(parsed) = url::Url::parse(&link.url) {
        if let Some(host) = parsed.host_str() {
            if is_vikingfile_host(host) {
                return true;
            }
        }
    }
    if matches!(link.source_site.as_deref(), Some("VikingFile")) {
        return true;
    }
    link.error.as_deref().is_some_and(|e| {
        let low = e.to_ascii_lowercase();
        low.contains("turnstile")
            || low.contains("cloudflare")
            || low.contains("captcha")
            || low.contains("verificação")
            || low.contains("resolver no vortex")
            || low.contains("resolver no navegador")
            || low.contains("wasm")
    })
}

fn grabbed_link_sort_tier(link: &GrabbedLink) -> u8 {
    let has_size = link.size_bytes.is_some_and(|s| s > 0);
    match (has_size, link.online) {
        (true, Some(false)) => 1,
        (true, _) => 0,
        (false, Some(true)) => 2,
        (false, None) => 3,
        (false, Some(false)) => 4,
    }
}

fn sort_grabbed_links(links: &mut [GrabbedLink]) {
    links.sort_by_key(grabbed_link_sort_tier);
}

fn safe_relative_path(raw: &str) -> Option<PathBuf> {
    let p = Path::new(raw);
    let mut clean = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if clean.as_os_str().is_empty() {
        None
    } else {
        Some(clean)
    }
}

fn try_remove_file_if_present(path: &Path) -> bool {
    if std::fs::symlink_metadata(path).is_err() {
        return true;
    }
    std::fs::remove_file(path).is_ok()
}

fn delete_non_torrent_files_from_disk(item: &DownloadItem) -> bool {
    let mut all_ok = true;
    all_ok &= try_remove_file_if_present(&item.file_path);
    let vortex_part = part_file_path(&item.file_path);
    all_ok &= try_remove_file_if_present(&vortex_part);
    if item.kind == DownloadKind::Media {
        let yt_part = PathBuf::from(format!("{}.part", item.file_path.display()));
        let ytdl = PathBuf::from(format!("{}.ytdl", item.file_path.display()));
        all_ok &= try_remove_file_if_present(&yt_part);
        all_ok &= try_remove_file_if_present(&ytdl);
    }
    all_ok
}

fn prune_empty_parents(start_dir: Option<&Path>, stop_at: &Path) {
    let mut cur = start_dir;
    while let Some(dir) = cur {
        if dir == stop_at || !dir.starts_with(stop_at) {
            break;
        }
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

fn delete_torrent_files_from_disk(item: &DownloadItem) {
    if let Some(ref ti) = item.torrent_info {
        if !ti.files.is_empty() {
            for f in &ti.files {
                if let Some(rel) = safe_relative_path(&f.path) {
                    let target = item.save_dir.join(&rel);
                    let _ = try_remove_file_if_present(&target);
                    prune_empty_parents(target.parent(), &item.save_dir);
                }
            }
            if item.file_path == item.save_dir && item.save_dir.is_dir() {
                let _ = std::fs::remove_dir(&item.save_dir);
            }
            return;
        }
    }

    if item.file_path != item.save_dir {
        if item.file_path.is_file() {
            let _ = std::fs::remove_file(&item.file_path);
        } else if item.file_path.is_dir() {
            let _ = std::fs::remove_dir_all(&item.file_path);
        }
        let vortex_part = part_file_path(&item.file_path);
        let _ = try_remove_file_if_present(&vortex_part);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_link(name: &str, size_bytes: Option<u64>, online: Option<bool>) -> GrabbedLink {
        GrabbedLink {
            id: Uuid::new_v4(),
            url: format!("https://example.com/{name}"),
            filename: name.to_string(),
            size_bytes,
            online,
            probing: false,
            supports_ranges: false,
            category: FileCategory::Archive,
            package_key: None,
            part_number: None,
            part_total: None,
            source_site: None,
            selected: true,
            headers: HashMap::new(),
            error: None,
        }
    }

    #[test]
    fn sorts_sized_links_to_top_preserving_order() {
        let mut links = vec![
            mk_link("no_size_online", None, Some(true)),
            mk_link("offline_no_size", None, Some(false)),
            mk_link("part1_sized", Some(5_000_000_000), Some(true)),
            mk_link("zero_size_online", Some(0), Some(true)),
            mk_link("part2_sized", Some(3_380_000_000), Some(true)),
        ];
        sort_grabbed_links(&mut links);
        let names: Vec<&str> = links.iter().map(|l| l.filename.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "part1_sized",
                "part2_sized",
                "no_size_online",
                "zero_size_online",
                "offline_no_size",
            ]
        );
    }

    #[test]
    fn valid_grabbed_link_requires_part_online_and_nonzero_size() {
        let mut valid = mk_link("game.part1.rar", Some(5_000_000_000), Some(true));
        valid.part_number = Some(1);
        assert!(is_valid_grabbed_link(&valid));

        let no_part = mk_link("Fix_Repair.rar", Some(29_000_000), Some(true));
        assert!(!is_valid_grabbed_link(&no_part));

        let mut offline = valid.clone();
        offline.online = Some(false);
        assert!(!is_valid_grabbed_link(&offline));

        let mut unknown_online = valid.clone();
        unknown_online.online = None;
        assert!(!is_valid_grabbed_link(&unknown_online));

        let mut no_size = valid.clone();
        no_size.size_bytes = None;
        assert!(!is_valid_grabbed_link(&no_size));

        let mut zero_size = valid.clone();
        zero_size.size_bytes = Some(0);
        assert!(!is_valid_grabbed_link(&zero_size));
    }

    #[test]
    fn merge_completed_download_item_preserves_mid_download_extractor_and_password_edits() {
        let now = Utc::now();
        let initial = DownloadItem {
            id: Uuid::new_v4(),
            package_id: None,
            part_index: None,
            part_total: None,
            kind: DownloadKind::Http,
            status: DownloadStatus::Downloading,
            url: "https://example.com/archive.zip".into(),
            filename: "archive.zip".into(),
            save_dir: PathBuf::from("C:/dl"),
            file_path: PathBuf::from("C:/dl/archive.zip"),
            category: FileCategory::Archive,
            total_bytes: Some(1000),
            downloaded_bytes: 0,
            uploaded_bytes: 0,
            speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: 4,
            supports_ranges: true,
            segments: Vec::new(),
            etag: None,
            last_modified: None,
            headers: HashMap::new(),
            checksum_expected: None,
            checksum_verified: None,
            media_info: None,
            torrent_info: None,
            auto_extract: true,
            extractor_override: Some("nanazip".into()),
            extract_password: None,
            extracted_to: None,
            extraction_status: None,
            extraction_progress: None,
            created_at: now,
            completed_at: None,
            error: None,
            priority: 0,
        };

        let mut live_slot = initial.clone();
        live_slot.extractor_override = Some("windows_native".into());
        live_slot.extract_password = Some("secret123".into());
        live_slot.priority = 2;

        let mut worker_finished = initial;
        worker_finished.status = DownloadStatus::Completed;
        worker_finished.downloaded_bytes = 1000;
        worker_finished.completed_at = Some(now);

        let merged = merge_completed_download_item(&live_slot, worker_finished);
        assert_eq!(merged.status, DownloadStatus::Completed);
        assert_eq!(merged.downloaded_bytes, 1000);
        assert_eq!(merged.extractor_override.as_deref(), Some("windows_native"));
        assert_eq!(merged.extract_password.as_deref(), Some("secret123"));
        assert_eq!(merged.priority, 2);
        assert!(merged.auto_extract);
    }

    #[test]
    fn detects_torrent_requests_including_query_strings_and_filename_hints() {
        assert!(is_torrent_request("magnet:?xt=urn:btih:1234", None));
        assert!(is_torrent_request("  MAGNET:?xt=urn:btih:1234", None));
        assert!(is_torrent_request(
            "https://itorrents.org/torrent/ABC.torrent?title=Demo",
            None
        ));
        assert!(is_torrent_request(
            "https://tracker.example/download.php?id=42",
            Some("release.TORRENT")
        ));
        assert!(!is_torrent_request("https://example.com/archive.zip", None));
    }

    #[test]
    fn default_torrent_dir_respects_categorize_setting() {
        let mut s = AppSettings::default();
        s.default_download_dir = PathBuf::from("D:/Downloads");
        s.categorize_into_subfolders = true;
        assert_eq!(default_torrent_dir_from_settings(&s), PathBuf::from("D:/Downloads/Torrents"));
        s.categorize_into_subfolders = false;
        assert_eq!(default_torrent_dir_from_settings(&s), PathBuf::from("D:/Downloads"));
    }

    #[test]
    fn delete_non_torrent_files_removes_main_and_part_files() {
        let dir = std::env::temp_dir().join(format!("vortex_del_http_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("archive.zip");
        let part_path = PathBuf::from(format!("{}.vortexpart", file_path.display()));
        let unrelated = dir.join("keep_me.txt");
        std::fs::write(&file_path, b"data").unwrap();
        std::fs::write(&part_path, b"part").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        let item = DownloadItem {
            id: Uuid::new_v4(),
            package_id: None,
            part_index: None,
            part_total: None,
            kind: DownloadKind::Http,
            status: DownloadStatus::Completed,
            url: "https://example.com/archive.zip".into(),
            filename: "archive.zip".into(),
            save_dir: dir.clone(),
            file_path: file_path.clone(),
            category: FileCategory::Archive,
            total_bytes: Some(4),
            downloaded_bytes: 4,
            uploaded_bytes: 0,
            speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: 1,
            supports_ranges: true,
            segments: Vec::new(),
            etag: None,
            last_modified: None,
            headers: HashMap::new(),
            checksum_expected: None,
            checksum_verified: None,
            media_info: None,
            torrent_info: None,
            auto_extract: false,
            extractor_override: None,
            extract_password: None,
            extracted_to: None,
            extraction_status: None,
            extraction_progress: None,
            created_at: Utc::now(),
            completed_at: None,
            error: None,
            priority: 0,
        };

        assert!(delete_non_torrent_files_from_disk(&item));
        assert!(!file_path.exists());
        assert!(!part_path.exists());
        assert!(unrelated.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_torrent_files_handles_multifile_and_rejects_traversal() {
        let root = std::env::temp_dir().join(format!("vortex_del_tor_{}", Uuid::new_v4()));
        let save_dir = root.join("GamePack");
        let nested_dir = save_dir.join("bin");
        let outside = root.join("outside_secret.txt");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(save_dir.join("setup.exe"), b"exe").unwrap();
        std::fs::write(nested_dir.join("data.bin"), b"bin").unwrap();
        std::fs::write(&outside, b"safe").unwrap();

        let item = DownloadItem {
            id: Uuid::new_v4(),
            package_id: None,
            part_index: None,
            part_total: None,
            kind: DownloadKind::Torrent,
            status: DownloadStatus::Completed,
            url: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into(),
            filename: "GamePack".into(),
            save_dir: save_dir.clone(),
            file_path: save_dir.clone(),
            category: FileCategory::Torrent,
            total_bytes: Some(6),
            downloaded_bytes: 6,
            uploaded_bytes: 0,
            speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: 1,
            supports_ranges: false,
            segments: Vec::new(),
            etag: None,
            last_modified: None,
            headers: HashMap::new(),
            checksum_expected: None,
            checksum_verified: None,
            media_info: None,
            torrent_info: Some(TorrentDownloadMeta {
                info_hash: "0123456789abcdef0123456789abcdef01234567".into(),
                magnet_or_path: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into(),
                rqbit_id: None,
                peers_connected: 0,
                peers_seen: 0,
                ratio: 0.0,
                files: vec![
                    crate::model::TorrentFileEntry {
                        index: 0,
                        path: "setup.exe".into(),
                        size_bytes: 3,
                        downloaded_bytes: 3,
                        included: true,
                    },
                    crate::model::TorrentFileEntry {
                        index: 1,
                        path: "bin/data.bin".into(),
                        size_bytes: 3,
                        downloaded_bytes: 3,
                        included: true,
                    },
                    crate::model::TorrentFileEntry {
                        index: 2,
                        path: "../outside_secret.txt".into(),
                        size_bytes: 4,
                        downloaded_bytes: 4,
                        included: true,
                    },
                ],
            }),
            auto_extract: false,
            extractor_override: None,
            extract_password: None,
            extracted_to: None,
            extraction_status: None,
            extraction_progress: None,
            created_at: Utc::now(),
            completed_at: None,
            error: None,
            priority: 0,
        };

        delete_torrent_files_from_disk(&item);
        assert!(!save_dir.exists());
        assert!(outside.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn normalize_browser_page_url_strips_fragment_and_preserves_case() {
        assert_eq!(
            normalize_browser_page_url("https://VikingFile.com/f/8obvHhvQET/#vortex_resolve"),
            "https://vikingfile.com/f/8obvHhvQET"
        );
        assert_eq!(
            normalize_browser_page_url("https://vik1ngfile.site/f/AbCdEf12"),
            "https://vik1ngfile.site/f/AbCdEf12"
        );
    }

    #[test]
    fn browser_resolvable_link_matches_vikingfile_and_cloudflare_errors() {
        let mut vf = mk_link("game.part1.rar", None, Some(false));
        vf.url = "https://vikingfile.com/f/8obvHhvQET".into();
        vf.source_site = Some("VikingFile".into());
        assert!(is_browser_resolvable_link(&vf));

        vf.online = Some(true);
        assert!(!is_browser_resolvable_link(&vf));

        let mut cf = mk_link("archive.rar", None, Some(false));
        cf.error = Some("FileDitch protegido por Cloudflare".into());
        assert!(is_browser_resolvable_link(&cf));

        let mut fd_pow = mk_link("game.part1.rar", Some(5_368_709_120), Some(false));
        fd_pow.url = "https://itsfileditchfiles.st/alpha43/abc/game.part1.rar".into();
        fd_pow.source_site = Some("FileDitch".into());
        fd_pow.error = Some(
            "FileDitch exige verificação automática no navegador — clique em 'Resolver no Vortex' ou 'Resolver no Navegador'".into(),
        );
        assert!(is_browser_resolvable_link(&fd_pow));

        let mut fd_dead = mk_link("dead.part1.rar", None, Some(false));
        fd_dead.url = "https://itsfileditchfiles.st/alpha43/dead/dead.part1.rar".into();
        fd_dead.source_site = Some("FileDitch".into());
        fd_dead.error = Some("Arquivo removido ou expirado no FileDitch".into());
        assert!(!is_browser_resolvable_link(&fd_dead));

        assert!(browser_page_url_matches(
            "https://itsfileditchfiles.st/alpha43/abc/game.part1.rar",
            "https://itsfileditchfiles.st/alpha43/abc/game.part1.rar?fdg=1#vortex_resolve"
        ));

        let regular_dead = mk_link("dead.rar", None, Some(false));
        assert!(!is_browser_resolvable_link(&regular_dead));
    }

    #[tokio::test]
    async fn browser_resolution_queue_advances_and_updates_link() {
        let mgr = DownloadManager::new(tokio::runtime::Handle::current());
        let mut l1 = mk_link("game.part1.rar", None, Some(false));
        l1.url = "https://vikingfile.com/f/PartOne11".into();
        l1.source_site = Some("VikingFile".into());
        l1.selected = false;

        let mut l2 = mk_link("game.part2.rar", None, Some(false));
        l2.url = "https://vikingfile.com/f/PartTwo22".into();
        l2.source_site = Some("VikingFile".into());
        l2.selected = false;

        *mgr.grabbed_links.write() = vec![l1.clone(), l2.clone()];

        let first = mgr.queue_browser_resolutions(vec![l1.url.clone(), l2.url.clone()]);
        assert_eq!(
            first.as_deref(),
            Some("https://vikingfile.com/f/PartOne11#vortex_resolve")
        );

        let (pending, q_len) = mgr.pending_browser_urls();
        assert_eq!(q_len, 1);
        assert_eq!(pending.len(), 2);

        let mut hdrs = HashMap::new();
        hdrs.insert("User-Agent".into(), "Chrome/131".into());
        let out = mgr.apply_browser_resolution(
            "https://vikingfile.com/f/PartOne11#vortex_resolve".into(),
            Some("https://cdn.vikingfile.com/dl/token123/game.part1.rar".into()),
            None,
            None,
            hdrs,
        );
        assert!(out.matched);
        assert_eq!(out.remaining, 0);
        assert_eq!(
            out.next_url.as_deref(),
            Some("https://vikingfile.com/f/PartTwo22#vortex_resolve")
        );

        let links = mgr.grabbed_links.read();
        let updated1 = links.iter().find(|l| l.id == l1.id).unwrap();
        assert_eq!(
            updated1.url,
            "https://cdn.vikingfile.com/dl/token123/game.part1.rar"
        );
        assert_eq!(updated1.online, Some(true));
        assert!(!updated1.selected);
        assert_eq!(updated1.error, None);
        assert_eq!(
            updated1.headers.get("User-Agent").map(String::as_str),
            Some("Chrome/131")
        );
    }

    #[tokio::test]
    async fn browser_resolution_matches_vikingfile_mirror_redirect() {
        let mgr = DownloadManager::new(tokio::runtime::Handle::current());
        let mut l1 = mk_link("fix.rar", None, Some(false));
        l1.url = "https://vikingfile.com/f/a19cAItZvb".into();
        l1.source_site = Some("VikingFile".into());
        l1.selected = false;
        *mgr.grabbed_links.write() = vec![l1.clone()];

        let _ = mgr.queue_browser_resolutions(vec![l1.url.clone()]);
        let out = mgr.apply_browser_resolution(
            "https://vik1ngf1le.online/f/a19cAItZvb#vortex_resolve".into(),
            Some("https://cdn.vik1ngf1le.online/dl/xyz/fix.rar".into()),
            None,
            None,
            HashMap::new(),
        );
        assert!(out.matched);
        let links = mgr.grabbed_links.read();
        let updated = links.iter().find(|l| l.id == l1.id).unwrap();
        assert_eq!(updated.url, "https://cdn.vik1ngf1le.online/dl/xyz/fix.rar");
        assert_eq!(updated.online, Some(true));
        assert!(!updated.selected);
    }

    #[test]
    fn grabber_filter_ignores_non_part_links_when_enabled() {
        let mut part_link = mk_link("game.part1.rar", Some(1024), Some(true));
        part_link.part_number = Some(1);
        let non_part_link = mk_link("Fix_Repair.rar", Some(1024), Some(true));

        assert!(should_keep_grabbed_link(&part_link, true));
        assert!(!should_keep_grabbed_link(&non_part_link, true));
        assert!(should_keep_grabbed_link(&part_link, false));
        assert!(should_keep_grabbed_link(&non_part_link, false));
    }

    #[tokio::test]
    async fn queue_selection_prefers_higher_priority_and_clamps_adjustments() {
        let mgr = DownloadManager::new(tokio::runtime::Handle::current());
        let mk_dl = |name: &str, url: &str, priority: i32| DownloadItem {
            id: Uuid::new_v4(),
            package_id: None,
            part_index: None,
            part_total: None,
            kind: DownloadKind::Http,
            status: DownloadStatus::Queued,
            url: url.into(),
            filename: name.into(),
            save_dir: PathBuf::from("."),
            file_path: PathBuf::from(name),
            category: FileCategory::Other,
            total_bytes: None,
            downloaded_bytes: 0,
            uploaded_bytes: 0,
            speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: 1,
            supports_ranges: true,
            segments: Vec::new(),
            etag: None,
            last_modified: None,
            headers: HashMap::new(),
            checksum_expected: None,
            checksum_verified: None,
            media_info: None,
            torrent_info: None,
            auto_extract: false,
            extractor_override: None,
            extract_password: None,
            extracted_to: None,
            extraction_status: None,
            extraction_progress: None,
            created_at: Utc::now(),
            completed_at: None,
            error: None,
            priority,
        };

        let low = mk_dl("low.bin", "https://a.example/low.bin", -1);
        let normal = mk_dl("normal.bin", "https://b.example/normal.bin", 0);
        let high = mk_dl("high.bin", "https://c.example/high.bin", 2);
        let picked = select_queued_downloads_to_start(&[low.clone(), normal.clone(), high.clone()], 2);
        assert_eq!(picked, vec![high.id, normal.id]);

        *mgr.downloads.write() = vec![normal.clone()];
        assert_eq!(mgr.adjust_download_priority(normal.id, 1), Some(1));
        assert_eq!(mgr.adjust_download_priority(normal.id, 5), Some(2));
        assert_eq!(mgr.adjust_download_priority(normal.id, -10), Some(-2));
        assert_eq!(mgr.downloads.read()[0].priority, -2);
    }

    #[tokio::test]
    async fn duplicate_downloads_reserve_unique_destinations_and_part_files() {
        let dir = std::env::temp_dir().join(format!("vortex_mgr_dedup_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let mgr = DownloadManager::new(tokio::runtime::Handle::current());
        {
            let mut s = mgr.settings.write();
            s.default_download_dir = dir.clone();
            s.categorize_into_subfolders = false;
            s.auto_start_downloads = false;
        }
        mgr.downloads.write().clear();

        let name = "Super Smash Bros. Ultimate [01006A800016E000][v0](axekin.com).zip";
        let id1 = mgr
            .add_http_download(NewDownloadRequest {
                url: "https://example.com/a/ssbu.zip".into(),
                filename: Some(name.into()),
                save_dir: Some(dir.clone()),
                connections: Some(4),
                headers: HashMap::new(),
                checksum: None,
                auto_extract: Some(false),
                extractor_override: None,
                extract_password: None,
                start_immediately: Some(false),
            })
            .unwrap();
        let id2 = mgr
            .add_http_download(NewDownloadRequest {
                url: "https://example.com/b/ssbu.zip".into(),
                filename: Some(name.into()),
                save_dir: Some(dir.clone()),
                connections: Some(4),
                headers: HashMap::new(),
                checksum: None,
                auto_extract: Some(false),
                extractor_override: None,
                extract_password: None,
                start_immediately: Some(false),
            })
            .unwrap();

        let dls = mgr.downloads.read();
        let item1 = dls.iter().find(|d| d.id == id1).unwrap().clone();
        let item2 = dls.iter().find(|d| d.id == id2).unwrap().clone();
        drop(dls);

        assert_eq!(item1.filename, name);
        assert_eq!(
            item2.filename,
            "Super Smash Bros. Ultimate [01006A800016E000][v0](axekin.com) (1).zip"
        );
        assert_ne!(item1.file_path, item2.file_path);
        assert_ne!(
            part_file_path(&item1.file_path),
            part_file_path(&item2.file_path)
        );

        // Resuming item1 with its own .vortexpart keeps item1's path instead of renaming itself.
        std::fs::write(part_file_path(&item1.file_path), b"partial").unwrap();
        let kept = mgr.reserve_destination_for_item(item1.id, &dir, name, true);
        assert_eq!(kept, item1.file_path);

        // Late Content-Disposition resolution on two items with generic fallback names still deduplicates.
        let id3 = mgr
            .add_http_download(NewDownloadRequest {
                url: "https://example.com/dl/1".into(),
                filename: Some("download.bin".into()),
                save_dir: Some(dir.clone()),
                connections: Some(4),
                headers: HashMap::new(),
                checksum: None,
                auto_extract: Some(false),
                extractor_override: None,
                extract_password: None,
                start_immediately: Some(false),
            })
            .unwrap();
        let id4 = mgr
            .add_http_download(NewDownloadRequest {
                url: "https://example.com/dl/2".into(),
                filename: Some("download.bin".into()),
                save_dir: Some(dir.clone()),
                connections: Some(4),
                headers: HashMap::new(),
                checksum: None,
                auto_extract: Some(false),
                extractor_override: None,
                extract_password: None,
                start_immediately: Some(false),
            })
            .unwrap();
        let p3 = mgr.reserve_destination_for_item(id3, &dir, "late-file.zip", false);
        let p4 = mgr.reserve_destination_for_item(id4, &dir, "late-file.zip", false);
        assert_eq!(
            p3.file_name().and_then(|n| n.to_str()),
            Some("late-file.zip")
        );
        assert_eq!(
            p4.file_name().and_then(|n| n.to_str()),
            Some("late-file (1).zip")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
