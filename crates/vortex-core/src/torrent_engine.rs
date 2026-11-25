use crate::model::{
    DownloadItem, DownloadKind, DownloadStatus, FileCategory, TorrentDownloadMeta,
    TorrentFileEntry,
};
use anyhow::{Result, anyhow};
use chrono::Utc;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

pub struct TorrentEngine {
    session: Arc<Session>,
    handles: Arc<Mutex<Vec<(Uuid, Arc<ManagedTorrent>)>>>,
}

impl TorrentEngine {
    pub async fn new(default_output_folder: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&default_output_folder).await?;
        let opts = SessionOptions {
            fastresume: true,
            ..Default::default()
        };
        let session = Session::new_with_opts(default_output_folder, opts).await?;
        Ok(Self {
            session,
            handles: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub async fn add_magnet_or_file(
        &self,
        source: &str,
        output_folder: PathBuf,
        paused: bool,
    ) -> Result<DownloadItem> {
        tokio::fs::create_dir_all(&output_folder).await?;
        let add = if source.starts_with("magnet:")
            || source.starts_with("http://")
            || source.starts_with("https://")
        {
            AddTorrent::from_url(source)
        } else {
            AddTorrent::from_local_filename(source)?
        };

        let opts = AddTorrentOptions {
            paused,
            overwrite: true,
            output_folder: Some(output_folder.to_string_lossy().to_string()),
            ..Default::default()
        };

        let resp = self.session.add_torrent(add, Some(opts)).await?;
        let handle = match resp {
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => h,
            AddTorrentResponse::ListOnly(_) => return Err(anyhow!("Resposta list-only inesperada")),
        };

        let id = Uuid::new_v4();
        let name = handle
            .name()
            .unwrap_or_else(|| {
                if source.starts_with("magnet:") {
                    "Torrent (carregando metadados...)".to_string()
                } else {
                    std::path::Path::new(source)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Torrent")
                        .to_string()
                }
            });

        let info_hash = format!("{:?}", handle.info_hash());
        let stats = handle.stats();

        let files = extract_torrent_files(&handle, &stats.file_progress);
        let total = if stats.total_bytes > 0 {
            Some(stats.total_bytes)
        } else {
            None
        };

        self.handles.lock().push((id, handle.clone()));

        Ok(DownloadItem {
            id,
            package_id: None,
            part_index: None,
            part_total: None,
            kind: DownloadKind::Torrent,
            status: if paused {
                DownloadStatus::Paused
            } else {
                DownloadStatus::Downloading
            },
            url: source.to_string(),
            filename: name.clone(),
            save_dir: output_folder.clone(),
            file_path: output_folder.join(&name),
            category: FileCategory::Torrent,
            total_bytes: total,
            downloaded_bytes: stats.progress_bytes,
            uploaded_bytes: stats.uploaded_bytes,
            speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: 0,
            supports_ranges: false,
            segments: Vec::new(),
            etag: None,
            last_modified: None,
            headers: Default::default(),
            checksum_expected: None,
            checksum_verified: None,
            media_info: None,
            torrent_info: Some(TorrentDownloadMeta {
                info_hash,
                magnet_or_path: source.to_string(),
                rqbit_id: Some(handle.id()),
                peers_connected: 0,
                peers_seen: 0,
                ratio: 0.0,
                files,
            }),
            auto_extract: false,
            extractor_override: None,
            extract_password: None,
            extracted_to: None,
            extraction_status: None,
            created_at: Utc::now(),
            completed_at: None,
            error: None,
        })
    }

    pub async fn pause(&self, id: Uuid) -> Result<()> {
        let handle = self
            .handles
            .lock()
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, h)| h.clone());
        if let Some(h) = handle {
            self.session.pause(&h).await?;
        }
        Ok(())
    }

    pub async fn resume(&self, id: Uuid) -> Result<()> {
        let handle = self
            .handles
            .lock()
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, h)| h.clone());
        if let Some(h) = handle {
            self.session.unpause(&h).await?;
        }
        Ok(())
    }

    pub async fn set_only_files(&self, id: Uuid, indices: HashSet<usize>) -> Result<()> {
        let handle = self
            .handles
            .lock()
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, h)| h.clone());
        if let Some(h) = handle {
            self.session.update_only_files(&h, &indices).await?;
        }
        Ok(())
    }

    pub async fn remove(&self, id: Uuid, delete_files: bool) -> Result<()> {
        let handle = {
            let mut g = self.handles.lock();
            if let Some(pos) = g.iter().position(|(i, _)| *i == id) {
                Some(g.remove(pos).1)
            } else {
                None
            }
        };
        if let Some(h) = handle {
            self.session.delete(h.id().into(), delete_files).await?;
        }
        Ok(())
    }

    pub fn poll_snapshots(&self, seed_after_complete: bool, max_ratio: f32) -> Vec<(Uuid, TorrentPollUpdate)> {
        let list = self.handles.lock().clone();
        let mut updates = Vec::with_capacity(list.len());
        for (id, handle) in list {
            let stats = handle.stats();
            let name = handle.name();
            let (dl_bps, ul_bps, peers_live, peers_seen) = if let Some(ref live) = stats.live {
                (
                    live.download_speed.as_bytes(),
                    live.upload_speed.as_bytes(),
                    live.snapshot.peer_stats.live,
                    live.snapshot.peer_stats.seen,
                )
            } else {
                (0, 0, 0, 0)
            };
            let ratio = if stats.total_bytes > 0 {
                stats.uploaded_bytes as f64 / stats.total_bytes as f64
            } else {
                0.0
            };

            let status = if let Some(ref err) = stats.error {
                DownloadStatus::Failed(err.clone())
            } else if stats.finished {
                if seed_after_complete && (max_ratio <= 0.0 || (ratio as f32) < max_ratio) && !handle.is_paused() {
                    DownloadStatus::Seeding
                } else {
                    DownloadStatus::Completed
                }
            } else if handle.is_paused() {
                DownloadStatus::Paused
            } else {
                DownloadStatus::Downloading
            };

            let files = extract_torrent_files(&handle, &stats.file_progress);
            updates.push((
                id,
                TorrentPollUpdate {
                    name,
                    status,
                    total_bytes: if stats.total_bytes > 0 {
                        Some(stats.total_bytes)
                    } else {
                        None
                    },
                    downloaded_bytes: stats.progress_bytes,
                    uploaded_bytes: stats.uploaded_bytes,
                    speed_bps: dl_bps,
                    upload_speed_bps: ul_bps,
                    peers_connected: peers_live as usize,
                    peers_seen: peers_seen as usize,
                    ratio,
                    files,
                    finished: stats.finished,
                },
            ));
        }
        updates
    }
}

pub struct TorrentPollUpdate {
    pub name: Option<String>,
    pub status: DownloadStatus,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
    pub speed_bps: u64,
    pub upload_speed_bps: u64,
    pub peers_connected: usize,
    pub peers_seen: usize,
    pub ratio: f64,
    pub files: Vec<TorrentFileEntry>,
    pub finished: bool,
}

fn extract_torrent_files(handle: &ManagedTorrent, file_progress: &[u64]) -> Vec<TorrentFileEntry> {
    let only_files = handle.only_files();
    handle
        .with_metadata(|meta| {
            meta.file_infos
                .iter()
                .enumerate()
                .map(|(idx, fi)| TorrentFileEntry {
                    index: idx,
                    path: fi.relative_filename.to_string_lossy().to_string(),
                    size_bytes: fi.len,
                    downloaded_bytes: file_progress.get(idx).copied().unwrap_or(0),
                    included: only_files
                        .as_ref()
                        .map_or(true, |set| set.contains(&idx)),
                })
                .collect()
        })
        .unwrap_or_default()
}
