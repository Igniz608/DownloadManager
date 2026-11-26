use crate::extractor::{
    ExtractionPasswordRequired, ProgressCallback, detect_default_extractors,
    extract_archive_with_progress, is_extractable_archive, is_password_error_text,
    select_best_extractor,
};
use crate::http_engine::{
    BandwidthLimiter, DownloadCancelHandle, HtmlPageDetected, ProgressSnapshot, build_http_client,
    run_http_download, run_http_download_with_options,
};
use crate::media_engine::{
    MediaToolsStatus, detect_media_tools, detect_media_url, install_ffmpeg_essentials,
    install_yt_dlp, probe_media_url, probe_single_media_url, run_media_download,
};
use crate::model::{
    AppSettings, DownloadId, DownloadItem, DownloadKind, DownloadPackage, DownloadStatus,
    FileCategory, GrabbedLink, MediaDownloadMeta, MediaProbeResult, NewDownloadRequest,
    NewMediaRequest, NoticeEntry, PackageId, PasswordPromptRequest,
};
use crate::package_grabber::{
    detect_multipart, extract_urls_from_text, fallback_filename_from_url, find_missing_parts,
    probe_grabbed_links, resolve_indirect_http_url, sanitize_filename,
};
use crate::state::{PersistedState, load_settings, load_state, save_settings, save_state};
use crate::torrent_engine::TorrentEngine;
use anyhow::{Result, anyhow};
use chrono::Utc;
use parking_lot::RwLock;
use reqwest::Client;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
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
}

pub struct DownloadManager {
    pub settings: Arc<RwLock<AppSettings>>,
    pub downloads: Arc<RwLock<Vec<DownloadItem>>>,
    pub packages: Arc<RwLock<Vec<DownloadPackage>>>,
    pub grabbed_links: Arc<RwLock<Vec<GrabbedLink>>>,
    grabber_probe_count: Arc<AtomicUsize>,
    focus_grabber_requested: Arc<AtomicBool>,
    pub speed_history: Arc<RwLock<VecDeque<SpeedSample>>>,
    pub notifications: Arc<RwLock<VecDeque<NoticeEntry>>>,
    pub pending_password_prompts: Arc<RwLock<Vec<PasswordPromptRequest>>>,
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
            grabber_probe_count: Arc::new(AtomicUsize::new(0)),
            focus_grabber_requested: Arc::new(AtomicBool::new(false)),
            speed_history: Arc::new(RwLock::new(hist)),
            notifications: Arc::new(RwLock::new(VecDeque::with_capacity(20))),
            pending_password_prompts: Arc::new(RwLock::new(Vec::new())),
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

                // Periodic state save every 5 seconds
                if tick_count % 10 == 0 {
                    mgr.persist_now();
                }
            }
        });
    }

    pub fn pump_queue(self: &Arc<Self>) {
        const MAX_ACTIVE_PER_HOST: usize = 2;
        let max_concurrent = self.settings.read().max_concurrent_downloads.max(1);
        let to_start: Vec<DownloadId> = {
            let dls = self.downloads.read();
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
                return;
            }
            let mut remaining_slots = max_concurrent - active_total;
            let mut chosen = Vec::new();
            for d in dls.iter().filter(|d| d.status == DownloadStatus::Queued) {
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
        };

        for id in to_start {
            self.start_download_task(id);
        }
    }

    pub fn add_http_download(self: &Arc<Self>, req: NewDownloadRequest) -> Result<DownloadId> {
        let url_trimmed = req.url.trim().to_string();
        if url_trimmed.starts_with("magnet:") || url_trimmed.to_lowercase().ends_with(".torrent") {
            let save_dir = req
                .save_dir
                .unwrap_or_else(|| self.settings.read().default_download_dir.join("Torrents"));
            return self.add_torrent_async(&url_trimmed, save_dir);
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
        let filename = req
            .filename
            .map(|f| sanitize_filename(&f))
            .unwrap_or_else(|| fallback_filename_from_url(&url_trimmed));
        let category = FileCategory::from_filename(&filename);
        let base_dir = req.save_dir.unwrap_or_else(|| settings.default_download_dir.clone());
        let save_dir = if settings.categorize_into_subfolders {
            base_dir.join(category.subfolder())
        } else {
            base_dir
        };
        let file_path = save_dir.join(&filename);
        let auto_start = req.start_immediately.unwrap_or(settings.auto_start_downloads);

        let id = Uuid::new_v4();
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
        };

        self.downloads.write().insert(0, item);
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
            filename: title_clean.clone(),
            save_dir: save_dir.clone(),
            file_path: save_dir.join(&title_clean),
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
        };

        self.downloads.write().insert(0, item);
        self.persist_now();
        self.pump_queue();
        Ok(id)
    }

    pub fn add_torrent_async(self: &Arc<Self>, source: &str, save_dir: PathBuf) -> Result<DownloadId> {
        let placeholder_id = Uuid::new_v4();
        let source_owned = source.to_string();
        let mgr = self.clone();

        self.runtime.spawn(async move {
            let te = match mgr.ensure_torrent_engine().await {
                Ok(t) => t,
                Err(e) => {
                    mgr.push_notice(format!("Erro ao iniciar motor BitTorrent: {e}"));
                    return;
                }
            };
            match te.add_magnet_or_file(&source_owned, save_dir, false).await {
                Ok(item) => {
                    mgr.downloads.write().insert(0, item);
                    mgr.persist_now();
                    mgr.push_notice("Torrent adicionado com sucesso.");
                }
                Err(e) => {
                    mgr.push_notice(format!("Falha ao adicionar torrent: {e}"));
                }
            }
        });

        Ok(placeholder_id)
    }

    async fn ensure_torrent_engine(&self) -> Result<Arc<TorrentEngine>> {
        let mut guard = self.torrent_engine.lock().await;
        if let Some(ref existing) = *guard {
            return Ok(existing.clone());
        }
        let dir = self.settings.read().default_download_dir.join("Torrents");
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

    pub fn request_focus_grabber(&self) {
        self.focus_grabber_requested.store(true, Ordering::SeqCst);
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
            let count = links.len();
            {
                let mut g = mgr.grabbed_links.write();
                for mut link in links {
                    if !g.iter().any(|existing| existing.url == link.url) {
                        link.selected = is_valid_grabbed_link(&link);
                        g.push(link);
                    }
                }
                sort_grabbed_links(&mut g);
            }
            mgr.grabber_probe_count.fetch_sub(1, Ordering::Relaxed);
            mgr.push_notice(format!("{count} link(s) analisados no LinkGrabber."));
        });
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
                let item = DownloadItem {
                    id,
                    package_id: Some(pkg_id),
                    part_index: link.part_number,
                    part_total: Some(total_parts),
                    kind: DownloadKind::Http,
                    status: DownloadStatus::Queued,
                    url: link.url,
                    filename: link.filename.clone(),
                    save_dir: pkg_dir.clone(),
                    file_path: pkg_dir.join(&link.filename),
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
                };
                self.downloads.write().insert(0, item);
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

                    let res = run_http_download(
                        client.clone(),
                        item.clone(),
                        cat_subfolders,
                        limiter.clone(),
                        cancel.clone(),
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
                                            resolved_item.filename = clean;
                                            resolved_item.file_path = resolved_item
                                                .save_dir
                                                .join(&resolved_item.filename);
                                        }
                                    }
                                    mgr.replace_item(resolved_item.clone());
                                    let direct_res = run_http_download(
                                        client,
                                        resolved_item,
                                        cat_subfolders,
                                        limiter,
                                        cancel.clone(),
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
                                media_item.file_path = media_item.save_dir.join(&media_item.filename);
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
                                let media_res = run_media_download(
                                    client,
                                    media_item,
                                    tools.yt_dlp_path,
                                    tools.ffmpeg_path,
                                    cancel.clone(),
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
                                let fallback_res = run_http_download_with_options(
                                    client,
                                    item.clone(),
                                    cat_subfolders,
                                    true,
                                    limiter,
                                    cancel.clone(),
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

                    let res = run_media_download(
                        client,
                        item.clone(),
                        tools.yt_dlp_path,
                        tools.ffmpeg_path,
                        cancel.clone(),
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
                if d.status == DownloadStatus::Queued {
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
        let kind = {
            let mut dls = self.downloads.write();
            if let Some(d) = dls.iter_mut().find(|x| x.id == id) {
                d.error = None;
                if matches!(d.status, DownloadStatus::Paused | DownloadStatus::Failed(_)) {
                    d.status = DownloadStatus::Queued;
                }
                Some(d.kind)
            } else {
                None
            }
        };
        if kind == Some(DownloadKind::Torrent) {
            let mgr_te = self.torrent_engine.clone();
            self.runtime.spawn(async move {
                if let Some(te) = mgr_te.lock().await.as_ref() {
                    let _ = te.resume(id).await;
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

    pub fn remove_download(&self, id: DownloadId, delete_disk_file: bool) {
        if let Some(cancel) = self.cancel_handles.read().get(&id) {
            cancel.cancel();
        }
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
                self.runtime.spawn(async move {
                    if let Some(te) = mgr_te.lock().await.as_ref() {
                        let _ = te.remove(id, delete_disk_file).await;
                    }
                });
            } else if delete_disk_file {
                let _ = std::fs::remove_file(&item.file_path);
                let part = PathBuf::from(format!("{}.vortexpart", item.file_path.display()));
                let _ = std::fs::remove_file(part);
            }
            let mut pkgs = self.packages.write();
            for p in pkgs.iter_mut() {
                p.item_ids.retain(|x| *x != id);
            }
            pkgs.retain(|p| !p.item_ids.is_empty());
        }
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
    updated
}

pub fn is_valid_grabbed_link(link: &GrabbedLink) -> bool {
    link.part_number.is_some()
        && link.online == Some(true)
        && link.size_bytes.is_some_and(|s| s > 0)
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
        };

        let mut live_slot = initial.clone();
        live_slot.extractor_override = Some("windows_native".into());
        live_slot.extract_password = Some("secret123".into());

        let mut worker_finished = initial;
        worker_finished.status = DownloadStatus::Completed;
        worker_finished.downloaded_bytes = 1000;
        worker_finished.completed_at = Some(now);

        let merged = merge_completed_download_item(&live_slot, worker_finished);
        assert_eq!(merged.status, DownloadStatus::Completed);
        assert_eq!(merged.downloaded_bytes, 1000);
        assert_eq!(merged.extractor_override.as_deref(), Some("windows_native"));
        assert_eq!(merged.extract_password.as_deref(), Some("secret123"));
        assert!(merged.auto_extract);
    }
}
