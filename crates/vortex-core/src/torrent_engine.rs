use crate::model::{
    DownloadItem, DownloadKind, DownloadStatus, FileCategory, TorrentDownloadMeta,
    TorrentFileEntry,
};
use anyhow::{Result, anyhow};
use chrono::Utc;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ListOnlyResponse, ManagedTorrent, Session,
    SessionOptions,
};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

pub struct TorrentEngine {
    session: Arc<Session>,
    handles: Arc<Mutex<Vec<(Uuid, Arc<ManagedTorrent>)>>>,
}

#[derive(Debug, Clone)]
pub struct TorrentAttachResult {
    pub filename: String,
    pub save_dir: PathBuf,
    pub file_path: PathBuf,
    pub status: DownloadStatus,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
    pub info_hash: String,
    pub rqbit_id: usize,
    pub files: Vec<TorrentFileEntry>,
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

    pub fn has_handle(&self, id: Uuid) -> bool {
        self.handles.lock().iter().any(|(i, _)| *i == id)
    }

    pub async fn add_magnet_or_file_with_id(
        &self,
        id: Uuid,
        source: &str,
        bytes_override: Option<bytes::Bytes>,
        output_folder: PathBuf,
        paused: bool,
    ) -> Result<TorrentAttachResult> {
        tokio::fs::create_dir_all(&output_folder).await?;
        let list_add = make_add_torrent(source, bytes_override)?;
        let list_opts = AddTorrentOptions {
            list_only: true,
            ..Default::default()
        };

        let list = match self.session.add_torrent(list_add, Some(list_opts)).await? {
            AddTorrentResponse::ListOnly(resp) => resp,
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => {
                return self.register_handle(id, source, h, paused);
            }
        };

        let final_output_folder = resolve_output_folder_for_list(&output_folder, source, &list);
        tokio::fs::create_dir_all(&final_output_folder).await?;

        let opts = AddTorrentOptions {
            paused,
            overwrite: true,
            output_folder: Some(final_output_folder.to_string_lossy().to_string()),
            only_files: list.only_files,
            initial_peers: if list.seen_peers.is_empty() {
                None
            } else {
                Some(list.seen_peers)
            },
            ..Default::default()
        };

        let resp = self
            .session
            .add_torrent(AddTorrent::TorrentFileBytes(list.torrent_bytes), Some(opts))
            .await?;
        let handle = match resp {
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => h,
            AddTorrentResponse::ListOnly(_) => return Err(anyhow!("Resposta list-only inesperada")),
        };

        self.register_handle(id, source, handle, paused)
    }

    pub async fn add_magnet_or_file(
        &self,
        source: &str,
        output_folder: PathBuf,
        paused: bool,
    ) -> Result<DownloadItem> {
        let id = Uuid::new_v4();
        let attached = self
            .add_magnet_or_file_with_id(id, source, None, output_folder, paused)
            .await?;

        Ok(DownloadItem {
            id,
            package_id: None,
            part_index: None,
            part_total: None,
            kind: DownloadKind::Torrent,
            status: attached.status,
            url: source.to_string(),
            filename: attached.filename,
            save_dir: attached.save_dir,
            file_path: attached.file_path,
            category: FileCategory::Torrent,
            total_bytes: attached.total_bytes,
            downloaded_bytes: attached.downloaded_bytes,
            uploaded_bytes: attached.uploaded_bytes,
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
                info_hash: attached.info_hash,
                magnet_or_path: source.to_string(),
                rqbit_id: Some(attached.rqbit_id),
                peers_connected: 0,
                peers_seen: 0,
                ratio: 0.0,
                files: attached.files,
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
        })
    }

    fn register_handle(
        &self,
        id: Uuid,
        source: &str,
        handle: Arc<ManagedTorrent>,
        paused: bool,
    ) -> Result<TorrentAttachResult> {
        let name = handle
            .name()
            .unwrap_or_else(|| fallback_torrent_name(source));

        let info_hash = format!("{:?}", handle.info_hash());
        let stats = handle.stats();

        let files = extract_torrent_files(&handle, &stats.file_progress);
        let save_dir = handle.output_folder().to_path_buf();
        let file_path = resolve_display_file_path(&handle, &save_dir, &files, &name);
        let total_bytes = if stats.total_bytes > 0 {
            Some(stats.total_bytes)
        } else {
            None
        };

        {
            let mut g = self.handles.lock();
            if let Some(existing) = g.iter_mut().find(|(existing_id, _)| *existing_id == id) {
                existing.1 = handle.clone();
            } else {
                g.push((id, handle.clone()));
            }
        }

        let status = if paused || handle.is_paused() {
            DownloadStatus::Paused
        } else {
            DownloadStatus::Downloading
        };

        Ok(TorrentAttachResult {
            filename: name,
            save_dir,
            file_path,
            status,
            total_bytes,
            downloaded_bytes: stats.progress_bytes,
            uploaded_bytes: stats.uploaded_bytes,
            info_hash,
            rqbit_id: handle.id(),
            files,
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
            let output_folder = handle.output_folder().to_path_buf();
            let display_name = name.clone().unwrap_or_default();
            let file_path = resolve_display_file_path(&handle, &output_folder, &files, &display_name);
            updates.push((
                id,
                TorrentPollUpdate {
                    name,
                    output_folder,
                    file_path,
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
    pub output_folder: PathBuf,
    pub file_path: PathBuf,
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

fn is_magnet_uri(source: &str) -> bool {
    source
        .trim_start()
        .get(..7)
        .map(|p| p.eq_ignore_ascii_case("magnet:"))
        .unwrap_or(false)
}

fn is_http_url(source: &str) -> bool {
    let trimmed = source.trim_start();
    trimmed
        .get(..7)
        .map(|p| p.eq_ignore_ascii_case("http://"))
        .unwrap_or(false)
        || trimmed
            .get(..8)
            .map(|p| p.eq_ignore_ascii_case("https://"))
            .unwrap_or(false)
}

fn make_add_torrent(source: &str, bytes_override: Option<bytes::Bytes>) -> Result<AddTorrent<'_>> {
    if let Some(bytes) = bytes_override {
        return Ok(AddTorrent::TorrentFileBytes(bytes));
    }
    if is_magnet_uri(source) || is_http_url(source) {
        Ok(AddTorrent::from_url(source))
    } else {
        AddTorrent::from_local_filename(source)
    }
}

pub fn extract_magnet_info_hash(source: &str) -> Option<String> {
    if !is_magnet_uri(source) {
        return None;
    }
    let parsed = url::Url::parse(source.trim()).ok()?;
    for (k, v) in parsed.query_pairs() {
        if k.eq_ignore_ascii_case("xt") {
            let lower = v.to_ascii_lowercase();
            if let Some(hash) = lower.strip_prefix("urn:btih:") {
                let clean = hash.trim();
                if !clean.is_empty() {
                    return Some(clean.to_string());
                }
            } else if let Some(hash) = lower.strip_prefix("urn:btmh:") {
                let clean = hash.trim();
                if !clean.is_empty() {
                    return Some(clean.to_string());
                }
            }
        }
    }
    None
}

fn magnet_display_name(source: &str) -> Option<String> {
    if !is_magnet_uri(source) {
        return None;
    }
    let parsed = url::Url::parse(source.trim()).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == "dn")
        .map(|(_, v)| v.into_owned())
        .filter(|s| !s.trim().is_empty())
}

pub fn fallback_torrent_name(source: &str) -> String {
    let trimmed = source.trim();
    if is_magnet_uri(trimmed) {
        if let Some(dn) = magnet_display_name(trimmed) {
            return dn;
        }
        if let Some(hash) = extract_magnet_info_hash(trimmed) {
            let short: String = hash.chars().take(10).collect();
            return format!("Magnet ({short})");
        }
        return "Torrent (carregando metadados...)".to_string();
    }
    if is_http_url(trimmed) {
        if let Ok(parsed) = url::Url::parse(trimmed) {
            if let Some(seg) = parsed.path_segments().and_then(|s| s.last()) {
                let clean = seg.trim_end_matches(".torrent").trim();
                if !clean.is_empty() {
                    return clean.to_string();
                }
            }
        }
        return "Torrent".to_string();
    }
    Path::new(trimmed)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Torrent")
        .to_string()
}

fn sanitize_torrent_folder_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        None
    } else {
        Some(cleaned)
    }
}

fn resolve_torrent_output_dir(
    base_output_folder: &Path,
    is_multi_file: bool,
    preferred_name: Option<&str>,
    fallback_stem: Option<&str>,
    info_hash_hex: &str,
) -> PathBuf {
    if !is_multi_file {
        return base_output_folder.to_path_buf();
    }

    let folder_name = preferred_name
        .and_then(sanitize_torrent_folder_name)
        .or_else(|| fallback_stem.and_then(sanitize_torrent_folder_name))
        .unwrap_or_else(|| {
            let prefix: String = info_hash_hex.chars().take(8).collect();
            format!("torrent-{prefix}")
        });

    let already_in_folder = base_output_folder
        .file_name()
        .and_then(|s| s.to_str())
        .map(|last| last.eq_ignore_ascii_case(&folder_name))
        .unwrap_or(false);

    if already_in_folder {
        base_output_folder.to_path_buf()
    } else {
        base_output_folder.join(folder_name)
    }
}

fn resolve_output_folder_for_list(
    base_output_folder: &Path,
    source: &str,
    list: &ListOnlyResponse,
) -> PathBuf {
    let real_files: Vec<(PathBuf, u64)> = list
        .info
        .iter_file_details()
        .filter(|fd| !fd.attrs().padding)
        .map(|fd| (fd.filename.to_pathbuf(), fd.len))
        .collect();

    let info_name = list.info.name().map(|s| s.into_owned());
    let has_files_list = list.info.info().files.is_some();
    let is_multi_file = real_files.len() >= 2
        || (has_files_list
            && info_name.as_deref() != real_files.first().and_then(|(p, _)| p.to_str()));

    let magnet_or_file_name = magnet_display_name(source).or_else(|| {
        if !source.starts_with("magnet:")
            && !source.starts_with("http://")
            && !source.starts_with("https://")
        {
            Path::new(source)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        } else {
            None
        }
    });

    let longest_stem = real_files
        .iter()
        .max_by_key(|(_, len)| *len)
        .and_then(|(p, _)| p.file_stem().and_then(|s| s.to_str()))
        .map(str::to_string);

    let preferred = info_name.or(magnet_or_file_name);
    let hash_hex = format!("{:?}", list.info_hash);
    resolve_torrent_output_dir(
        base_output_folder,
        is_multi_file,
        preferred.as_deref(),
        longest_stem.as_deref(),
        &hash_hex,
    )
}

fn resolve_display_file_path(
    handle: &ManagedTorrent,
    output_folder: &Path,
    files: &[TorrentFileEntry],
    name: &str,
) -> PathBuf {
    let is_multi_file = handle
        .with_metadata(|meta| {
            let real_files: Vec<&str> = meta
                .file_infos
                .iter()
                .filter(|fi| !fi.attrs.padding)
                .filter_map(|fi| fi.relative_filename.to_str())
                .collect();
            real_files.len() >= 2
                || (meta.info.info().files.is_some()
                    && meta.info.name().as_deref() != real_files.first().copied())
        })
        .unwrap_or(files.len() >= 2);

    if is_multi_file {
        output_folder.to_path_buf()
    } else if let Some(first) = files.first() {
        output_folder.join(&first.path)
    } else if !name.is_empty() {
        output_folder.join(name)
    } else {
        output_folder.to_path_buf()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_windows_illegal_chars_and_trailing_dots() {
        assert_eq!(
            sanitize_torrent_folder_name("Game: Ultimate / Pack* v1.0.. "),
            Some("Game_ Ultimate _ Pack_ v1.0".to_string())
        );
        assert_eq!(sanitize_torrent_folder_name(".."), None);
        assert_eq!(sanitize_torrent_folder_name("   "), None);
    }

    #[test]
    fn resolves_subfolder_only_for_multifile_and_avoids_double_nesting() {
        let base = Path::new("D:/Downloads");
        assert_eq!(
            resolve_torrent_output_dir(base, false, Some("ubuntu.iso"), None, "abcdef12"),
            PathBuf::from("D:/Downloads")
        );
        assert_eq!(
            resolve_torrent_output_dir(base, true, Some("Xbox Game"), None, "abcdef12"),
            PathBuf::from("D:/Downloads/Xbox Game")
        );

        let already_nested = Path::new("D:/Downloads/Xbox Game");
        assert_eq!(
            resolve_torrent_output_dir(already_nested, true, Some("xbox game"), None, "abcdef12"),
            PathBuf::from("D:/Downloads/Xbox Game")
        );
    }

    #[test]
    fn extracts_magnet_display_name_and_hash() {
        let magnet = "magnet:?xt=urn:btih:0123456789ABCDEF0123456789abcdef01234567&dn=My%20Cool%20Game";
        assert_eq!(magnet_display_name(magnet).as_deref(), Some("My Cool Game"));
        assert_eq!(
            extract_magnet_info_hash(magnet).as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(fallback_torrent_name(magnet), "My Cool Game");

        let bare_magnet = "MAGNET:?xt=urn:btih:AABBCCDDEEFF00112233445566778899AABBCCDD";
        assert_eq!(fallback_torrent_name(bare_magnet), "Magnet (aabbccddee)");

        let http_torrent = "https://itorrents.org/torrent/12345.torrent?title=Ubuntu";
        assert_eq!(fallback_torrent_name(http_torrent), "12345");
    }
}
