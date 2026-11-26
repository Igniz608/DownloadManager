use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

pub type DownloadId = Uuid;
pub type PackageId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadKind {
    Http,
    Media,
    Torrent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadStatus {
    Queued,
    Probing,
    Downloading,
    Paused,
    Merging,
    Extracting,
    Seeding,
    Completed,
    Failed(String),
}

impl DownloadStatus {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Probing | Self::Downloading | Self::Merging | Self::Extracting | Self::Seeding
        )
    }

    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Completed | Self::Seeding)
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Queued => "Na fila",
            Self::Probing => "Analisando",
            Self::Downloading => "Baixando",
            Self::Paused => "Pausado",
            Self::Merging => "Mesclando",
            Self::Extracting => "Extraindo",
            Self::Seeding => "Semeando",
            Self::Completed => "Concluído",
            Self::Failed(_) => "Erro",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileCategory {
    Video,
    Audio,
    Archive,
    Program,
    Document,
    Image,
    Torrent,
    Other,
}

impl FileCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Video => "Vídeos",
            Self::Audio => "Áudio",
            Self::Archive => "Compactados",
            Self::Program => "Programas",
            Self::Document => "Documentos",
            Self::Image => "Imagens",
            Self::Torrent => "Torrents",
            Self::Other => "Outros",
        }
    }

    pub fn subfolder(self) -> &'static str {
        match self {
            Self::Video => "Videos",
            Self::Audio => "Audio",
            Self::Archive => "Archives",
            Self::Program => "Programs",
            Self::Document => "Documents",
            Self::Image => "Images",
            Self::Torrent => "Torrents",
            Self::Other => "Other",
        }
    }

    pub fn from_filename(name: &str) -> Self {
        let lower = name.to_lowercase();
        if lower.ends_with(".torrent") {
            return Self::Torrent;
        }
        let ext = lower.rsplit('.').next().unwrap_or("");
        match ext {
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "wmv" | "m4v" | "ts" | "m3u8"
            | "mpd" => Self::Video,
            "mp3" | "flac" | "wav" | "aac" | "ogg" | "opus" | "m4a" | "wma" => Self::Audio,
            "zip" | "rar" | "7z" | "tar" | "gz" | "xz" | "bz2" | "zst" | "iso" | "cab"
            | "tgz" => Self::Archive,
            "exe" | "msi" | "msix" | "appx" | "bat" | "cmd" | "ps1" | "deb" | "rpm"
            | "appimage" | "dmg" => Self::Program,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md"
            | "csv" | "epub" | "json" | "xml" => Self::Document,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "avif"
            | "tiff" => Self::Image,
            _ => {
                // Multi-part archive suffixes like .001, .r00, .z01
                if ext.len() == 3 && ext.chars().all(|c| c.is_ascii_digit()) {
                    return Self::Archive;
                }
                if (ext.starts_with('r') || ext.starts_with('z'))
                    && ext.len() == 3
                    && ext[1..].chars().all(|c| c.is_ascii_digit())
                {
                    return Self::Archive;
                }
                Self::Other
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentState {
    pub id: usize,
    pub start: u64,
    pub end: u64, // inclusive
    pub downloaded: u64,
    pub done: bool,
}

impl SegmentState {
    pub fn total_len(&self) -> u64 {
        if self.end >= self.start {
            self.end - self.start + 1
        } else {
            0
        }
    }

    pub fn remaining(&self) -> u64 {
        self.total_len().saturating_sub(self.downloaded)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaDownloadMeta {
    pub format_id: Option<String>,
    pub quality_label: Option<String>,
    pub title: Option<String>,
    pub duration_secs: Option<f64>,
    pub is_hls: bool,
    pub is_dash: bool,
    pub audio_only: bool,
    pub embed_subtitles: bool,
    pub extractor_used: String, // "yt-dlp", "native-hls", "native-dash", "direct"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFileEntry {
    pub index: usize,
    pub path: String,
    pub size_bytes: u64,
    pub downloaded_bytes: u64,
    pub included: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentDownloadMeta {
    pub info_hash: String,
    pub magnet_or_path: String,
    pub rqbit_id: Option<usize>,
    pub peers_connected: usize,
    pub peers_seen: usize,
    pub ratio: f64,
    pub files: Vec<TorrentFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadItem {
    pub id: DownloadId,
    pub package_id: Option<PackageId>,
    pub part_index: Option<u32>,
    pub part_total: Option<u32>,
    pub kind: DownloadKind,
    pub status: DownloadStatus,
    pub url: String,
    pub filename: String,
    pub save_dir: PathBuf,
    pub file_path: PathBuf,
    pub category: FileCategory,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
    pub speed_bps: u64,
    pub upload_speed_bps: u64,
    pub eta_seconds: Option<u64>,
    pub connections: usize,
    pub supports_ranges: bool,
    pub segments: Vec<SegmentState>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub headers: HashMap<String, String>,
    pub checksum_expected: Option<String>,
    pub checksum_verified: Option<bool>,
    pub media_info: Option<MediaDownloadMeta>,
    pub torrent_info: Option<TorrentDownloadMeta>,
    pub auto_extract: bool,
    pub extractor_override: Option<String>,
    pub extract_password: Option<String>,
    pub extracted_to: Option<PathBuf>,
    pub extraction_status: Option<String>,
    #[serde(default)]
    pub extraction_progress: Option<f32>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

impl DownloadItem {
    pub fn progress_fraction(&self) -> f32 {
        match self.total_bytes {
            Some(total) if total > 0 => (self.downloaded_bytes as f64 / total as f64).clamp(0.0, 1.0) as f32,
            _ => {
                if self.status.is_finished() {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadPackage {
    pub id: PackageId,
    pub name: String,
    pub save_dir: PathBuf,
    pub item_ids: Vec<DownloadId>,
    pub archive_base: Option<String>,
    pub total_parts_expected: Option<u32>,
    pub missing_parts: Vec<u32>,
    pub auto_extract: bool,
    pub extractor_id: Option<String>,
    pub extract_password: Option<String>,
    pub delete_archives_after_extract: bool,
    pub extraction_status: Option<String>,
    #[serde(default)]
    pub extraction_progress: Option<f32>,
    pub extracted_to: Option<PathBuf>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrabbedLink {
    pub id: Uuid,
    pub url: String,
    pub filename: String,
    pub size_bytes: Option<u64>,
    pub online: Option<bool>,
    pub probing: bool,
    pub supports_ranges: bool,
    pub category: FileCategory,
    pub package_key: Option<String>,
    pub part_number: Option<u32>,
    pub part_total: Option<u32>,
    #[serde(default)]
    pub source_site: Option<String>,
    pub selected: bool,
    pub headers: HashMap<String, String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PasswordPromptRequest {
    pub id: Uuid,
    pub package_id: Option<PackageId>,
    pub item_id: DownloadId,
    pub display_name: String,
    pub archive_path: PathBuf,
    pub dest_dir: PathBuf,
    pub extractor_override: Option<String>,
    pub delete_archives: bool,
    pub files_to_delete: Vec<PathBuf>,
    pub wrong_password: bool,
}

#[derive(Debug, Clone)]
pub struct NoticeEntry {
    pub id: Uuid,
    pub message: String,
    pub is_error: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtractorKind {
    NanaZip,
    SevenZip,
    WinRar,
    PeaZip,
    WindowsNative,
    CustomPlugin,
}

impl ExtractorKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::NanaZip => "NanaZip",
            Self::SevenZip => "7-Zip",
            Self::WinRar => "WinRAR",
            Self::PeaZip => "PeaZip",
            Self::WindowsNative => "Windows Nativo (tar / Shell)",
            Self::CustomPlugin => "Plugin Customizado",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractorProfile {
    pub id: String,
    pub name: String,
    pub kind: ExtractorKind,
    pub executable: String,
    pub args_template: String,
    pub password_arg_template: Option<String>,
    pub silent: bool,
    pub detected: bool,
    pub detected_path: Option<String>,
    pub supported_extensions: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    pub preset_id: String,
    pub name: String,
    pub dark_mode: bool,
    pub bg_primary: [u8; 4],
    pub bg_secondary: [u8; 4],
    pub bg_card: [u8; 4],
    pub bg_hover: [u8; 4],
    pub accent: [u8; 4],
    pub accent_hover: [u8; 4],
    pub text_primary: [u8; 4],
    pub text_muted: [u8; 4],
    pub border: [u8; 4],
    pub success: [u8; 4],
    pub warning: [u8; 4],
    pub error: [u8; 4],
    pub corner_radius: f32,
    pub font_scale: f32,
    pub row_height: f32,
    pub compact_mode: bool,
    pub show_segment_bars: bool,
    pub show_speed_graph: bool,
    pub sidebar_width: f32,
}

impl ThemeConfig {
    pub fn presets() -> Vec<Self> {
        vec![
            Self::obsidian(),
            Self::midnight_cyan(),
            Self::catppuccin_mocha(),
            Self::nord(),
            Self::dracula(),
            Self::clean_light(),
        ]
    }

    pub fn obsidian() -> Self {
        Self {
            preset_id: "obsidian".into(),
            name: "Obsidian Violet".into(),
            dark_mode: true,
            bg_primary: [13, 15, 21, 255],
            bg_secondary: [19, 22, 31, 255],
            bg_card: [26, 30, 42, 255],
            bg_hover: [35, 41, 56, 255],
            accent: [124, 92, 255, 255],
            accent_hover: [145, 118, 255, 255],
            text_primary: [240, 243, 250, 255],
            text_muted: [148, 157, 178, 255],
            border: [44, 51, 70, 255],
            success: [52, 211, 153, 255],
            warning: [251, 191, 36, 255],
            error: [248, 113, 113, 255],
            corner_radius: 10.0,
            font_scale: 1.0,
            row_height: 62.0,
            compact_mode: false,
            show_segment_bars: true,
            show_speed_graph: true,
            sidebar_width: 224.0,
        }
    }

    pub fn midnight_cyan() -> Self {
        Self {
            preset_id: "midnight_cyan".into(),
            name: "Midnight Cyan".into(),
            dark_mode: true,
            bg_primary: [10, 18, 26, 255],
            bg_secondary: [15, 25, 36, 255],
            bg_card: [20, 34, 48, 255],
            bg_hover: [29, 48, 68, 255],
            accent: [34, 211, 238, 255],
            accent_hover: [103, 232, 249, 255],
            text_primary: [236, 245, 252, 255],
            text_muted: [142, 166, 188, 255],
            border: [36, 58, 80, 255],
            success: [45, 212, 191, 255],
            warning: [250, 204, 21, 255],
            error: [251, 113, 133, 255],
            ..Self::obsidian()
        }
    }

    pub fn catppuccin_mocha() -> Self {
        Self {
            preset_id: "catppuccin_mocha".into(),
            name: "Catppuccin Mocha".into(),
            dark_mode: true,
            bg_primary: [24, 24, 37, 255],
            bg_secondary: [30, 30, 46, 255],
            bg_card: [49, 50, 68, 255],
            bg_hover: [69, 71, 90, 255],
            accent: [137, 180, 250, 255],
            accent_hover: [180, 190, 254, 255],
            text_primary: [205, 214, 244, 255],
            text_muted: [166, 173, 200, 255],
            border: [69, 71, 90, 255],
            success: [166, 227, 161, 255],
            warning: [249, 226, 175, 255],
            error: [243, 139, 168, 255],
            ..Self::obsidian()
        }
    }

    pub fn nord() -> Self {
        Self {
            preset_id: "nord".into(),
            name: "Nord Frost".into(),
            dark_mode: true,
            bg_primary: [36, 41, 51, 255],
            bg_secondary: [46, 52, 64, 255],
            bg_card: [59, 66, 82, 255],
            bg_hover: [67, 76, 94, 255],
            accent: [136, 192, 208, 255],
            accent_hover: [143, 188, 187, 255],
            text_primary: [236, 239, 244, 255],
            text_muted: [216, 222, 233, 255],
            border: [76, 86, 106, 255],
            success: [163, 190, 140, 255],
            warning: [235, 203, 139, 255],
            error: [191, 97, 106, 255],
            ..Self::obsidian()
        }
    }

    pub fn dracula() -> Self {
        Self {
            preset_id: "dracula".into(),
            name: "Dracula".into(),
            dark_mode: true,
            bg_primary: [30, 31, 41, 255],
            bg_secondary: [40, 42, 54, 255],
            bg_card: [52, 55, 70, 255],
            bg_hover: [68, 71, 90, 255],
            accent: [189, 147, 249, 255],
            accent_hover: [255, 121, 198, 255],
            text_primary: [248, 248, 242, 255],
            text_muted: [162, 174, 210, 255],
            border: [68, 71, 90, 255],
            success: [80, 250, 123, 255],
            warning: [241, 250, 140, 255],
            error: [255, 85, 85, 255],
            ..Self::obsidian()
        }
    }

    pub fn clean_light() -> Self {
        Self {
            preset_id: "clean_light".into(),
            name: "Clean Slate Light".into(),
            dark_mode: false,
            bg_primary: [244, 246, 250, 255],
            bg_secondary: [255, 255, 255, 255],
            bg_card: [255, 255, 255, 255],
            bg_hover: [236, 240, 248, 255],
            accent: [59, 130, 246, 255],
            accent_hover: [37, 99, 235, 255],
            text_primary: [17, 24, 39, 255],
            text_muted: [100, 116, 139, 255],
            border: [220, 226, 236, 255],
            success: [16, 185, 129, 255],
            warning: [217, 119, 6, 255],
            error: [220, 38, 38, 255],
            ..Self::obsidian()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub default_download_dir: PathBuf,
    pub categorize_into_subfolders: bool,
    pub max_concurrent_downloads: usize,
    pub default_connections: usize,
    pub speed_limit_kbps: u64,
    pub clipboard_monitor: bool,
    pub auto_start_downloads: bool,
    pub ipc_port: u16,
    pub ipc_token: String,
    pub yt_dlp_path: Option<String>,
    pub ffmpeg_path: Option<String>,
    pub default_extractor_id: String,
    pub auto_extract_archives: bool,
    pub extract_into_subfolder: bool,
    pub delete_archives_after_extract: bool,
    pub password_list: Vec<String>,
    pub extractors: Vec<ExtractorProfile>,
    pub torrent_seed_after_complete: bool,
    pub torrent_max_ratio: f32,
    pub torrent_port_start: u16,
    pub theme: ThemeConfig,
    pub custom_themes: Vec<ThemeConfig>,
}

impl Default for AppSettings {
    fn default() -> Self {
        let dl_dir = dirs::download_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            default_download_dir: dl_dir.join("VortexDL"),
            categorize_into_subfolders: true,
            max_concurrent_downloads: 4,
            default_connections: 8,
            speed_limit_kbps: 0,
            clipboard_monitor: true,
            auto_start_downloads: true,
            ipc_port: 17654,
            ipc_token: Uuid::new_v4().to_string(),
            yt_dlp_path: None,
            ffmpeg_path: None,
            default_extractor_id: "auto".into(),
            auto_extract_archives: false,
            extract_into_subfolder: true,
            delete_archives_after_extract: false,
            password_list: Vec::new(),
            extractors: Vec::new(),
            torrent_seed_after_complete: false,
            torrent_max_ratio: 1.0,
            torrent_port_start: 42069,
            theme: ThemeConfig::obsidian(),
            custom_themes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFormatOption {
    pub format_id: String,
    pub ext: String,
    pub resolution: String,
    pub fps: Option<f64>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub filesize: Option<u64>,
    pub note: String,
    pub is_audio_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaProbeResult {
    pub url: String,
    pub title: String,
    pub uploader: Option<String>,
    pub duration_secs: Option<f64>,
    pub thumbnail: Option<String>,
    pub is_playlist: bool,
    pub playlist_count: usize,
    pub playlist_entries: Vec<MediaPlaylistEntry>,
    pub formats: Vec<MediaFormatOption>,
    pub extractor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPlaylistEntry {
    pub title: String,
    pub url: String,
    pub duration_secs: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewDownloadRequest {
    pub url: String,
    pub filename: Option<String>,
    pub save_dir: Option<PathBuf>,
    pub connections: Option<usize>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub checksum: Option<String>,
    pub auto_extract: Option<bool>,
    pub extractor_override: Option<String>,
    pub extract_password: Option<String>,
    pub start_immediately: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMediaRequest {
    pub url: String,
    pub save_dir: Option<PathBuf>,
    pub format_id: Option<String>,
    pub quality_label: Option<String>,
    #[serde(default)]
    pub audio_only: bool,
    #[serde(default)]
    pub embed_subtitles: bool,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub title_hint: Option<String>,
    #[serde(default)]
    pub is_hls: bool,
    #[serde(default)]
    pub is_dash: bool,
}
