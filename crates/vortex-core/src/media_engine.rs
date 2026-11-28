use crate::extractor::find_in_path;
use crate::http_engine::{
    DestinationResolver, DownloadCancelHandle, apply_custom_headers, unique_destination_path,
};
use crate::model::{
    AppSettings, CompressionJobStatus, DownloadItem, DownloadStatus, FileCategory,
    MediaFormatOption, MediaPlaylistEntry, MediaProbeResult, VideoCompressionCodec,
    VideoCompressionPlan, VideoProbeInfo,
};
use crate::package_grabber::sanitize_filename;
use crate::state::bin_dir;
use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct MediaToolsStatus {
    pub yt_dlp_path: Option<String>,
    pub ffmpeg_path: Option<String>,
}

pub fn detect_media_tools(settings: &AppSettings) -> MediaToolsStatus {
    let yt_dlp_path = settings
        .yt_dlp_path
        .clone()
        .filter(|p| Path::new(p).exists() || find_in_path(p).is_some())
        .or_else(|| find_binary_everywhere(&["yt-dlp.exe", "yt-dlp"]));

    let ffmpeg_path = settings
        .ffmpeg_path
        .clone()
        .filter(|p| Path::new(p).exists() || find_in_path(p).is_some())
        .or_else(|| find_binary_everywhere(&["ffmpeg.exe", "ffmpeg"]));

    MediaToolsStatus {
        yt_dlp_path,
        ffmpeg_path,
    }
}

fn find_binary_everywhere(names: &[&str]) -> Option<String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    find_binary_with_dirs(names, &bin_dir(), exe_dir.as_deref())
}

fn find_binary_with_dirs(
    names: &[&str],
    user_bin_dir: &Path,
    exe_dir: Option<&Path>,
) -> Option<String> {
    for name in names {
        let p = user_bin_dir.join(name);
        if p.exists() {
            return Some(p.to_string_lossy().to_string());
        }
    }
    if let Some(dir) = exe_dir {
        for sub in [dir.join("bin"), dir.to_path_buf()] {
            for name in names {
                let p = sub.join(name);
                if p.exists() {
                    return Some(p.to_string_lossy().to_string());
                }
            }
        }
    }
    for name in names {
        if let Some(found) = find_in_path(name) {
            return Some(found);
        }
    }
    if let Some(home) = dirs::home_dir() {
        for sub in ["scoop\\shims", ".local\\bin"] {
            for name in names {
                let p = home.join(sub).join(name);
                if p.exists() {
                    return Some(p.to_string_lossy().to_string());
                }
            }
        }
    }
    None
}

fn build_tool_download_client(fallback: &Client) -> Client {
    Client::builder()
        .user_agent(concat!("VortexDL/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .unwrap_or_else(|_| fallback.clone())
}

async fn download_bytes_with_retries(
    client: &Client,
    urls: &[&str],
    min_bytes: usize,
) -> Result<bytes::Bytes> {
    let mut last_err = anyhow!("Nenhuma URL de download informada");
    for url in urls {
        for attempt in 1..=2 {
            match client.get(*url).send().await {
                Ok(resp) => match resp.error_for_status() {
                    Ok(ok_resp) => match ok_resp.bytes().await {
                        Ok(bytes) if bytes.len() >= min_bytes => return Ok(bytes),
                        Ok(bytes) => {
                            last_err = anyhow!(
                                "Resposta incompleta de {url} ({} bytes)",
                                bytes.len()
                            );
                        }
                        Err(e) => last_err = anyhow!("Erro ao baixar {url} (tentativa {attempt}): {e}"),
                    },
                    Err(e) => last_err = anyhow!("HTTP erro em {url} (tentativa {attempt}): {e}"),
                },
                Err(e) => last_err = anyhow!("Falha de rede em {url} (tentativa {attempt}): {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        }
    }
    Err(last_err)
}

pub async fn install_yt_dlp(client: &Client) -> Result<PathBuf> {
    let dir = bin_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let dest = dir.join("yt-dlp.exe");
    let dl_client = build_tool_download_client(client);
    let bytes = download_bytes_with_retries(
        &dl_client,
        &["https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe"],
        512 * 1024,
    )
    .await?;
    let tmp = dir.join("yt-dlp.exe.tmp");
    tokio::fs::write(&tmp, &bytes).await?;
    let _ = tokio::fs::remove_file(&dest).await;
    tokio::fs::rename(&tmp, &dest).await?;
    Ok(dest)
}

pub async fn install_ffmpeg_essentials(client: &Client) -> Result<PathBuf> {
    let dir = bin_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let dl_client = build_tool_download_client(client);
    let bytes = download_bytes_with_retries(
        &dl_client,
        &[
            "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip",
            "https://github.com/GyanD/codexffmpeg/releases/latest/download/ffmpeg-release-essentials.zip",
            "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip",
        ],
        5 * 1024 * 1024,
    )
    .await?;

    let zip_path = dir.join("ffmpeg-essentials.zip.tmp");
    tokio::fs::write(&zip_path, &bytes).await?;

    let extract_dir = dir.join("ffmpeg_tmp");
    let _ = tokio::fs::remove_dir_all(&extract_dir).await;
    tokio::fs::create_dir_all(&extract_dir).await?;

    let tar_bin = crate::extractor::find_windows_tar().unwrap_or_else(|| "tar.exe".to_string());
    let mut tar_cmd = Command::new(tar_bin);
    crate::extractor::configure_silent_command(&mut tar_cmd);
    let out = tar_cmd
        .args([
            "-xf",
            &zip_path.to_string_lossy(),
            "-C",
            &extract_dir.to_string_lossy(),
        ])
        .output()
        .await?;

    if !out.status.success() {
        let _ = tokio::fs::remove_file(&zip_path).await;
        let _ = tokio::fs::remove_dir_all(&extract_dir).await;
        return Err(anyhow!(
            "Falha ao extrair ffmpeg: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    let mut found_ffmpeg = None;
    let mut found_ffprobe = None;
    for entry in walkdir_shallow(&extract_dir) {
        if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
            if name.eq_ignore_ascii_case("ffmpeg.exe") {
                found_ffmpeg = Some(entry);
            } else if name.eq_ignore_ascii_case("ffprobe.exe") {
                found_ffprobe = Some(entry);
            }
        }
    }

    let ffmpeg_dest = dir.join("ffmpeg.exe");
    let ffmpeg_tmp = dir.join("ffmpeg.exe.tmp");
    if let Some(src) = found_ffmpeg {
        tokio::fs::copy(&src, &ffmpeg_tmp).await?;
        let _ = tokio::fs::remove_file(&ffmpeg_dest).await;
        tokio::fs::rename(&ffmpeg_tmp, &ffmpeg_dest).await?;
    } else {
        let _ = tokio::fs::remove_file(&zip_path).await;
        let _ = tokio::fs::remove_dir_all(&extract_dir).await;
        return Err(anyhow!("ffmpeg.exe não encontrado dentro do pacote baixado"));
    }
    if let Some(src) = found_ffprobe {
        let ffprobe_dest = dir.join("ffprobe.exe");
        let ffprobe_tmp = dir.join("ffprobe.exe.tmp");
        if tokio::fs::copy(&src, &ffprobe_tmp).await.is_ok() {
            let _ = tokio::fs::remove_file(&ffprobe_dest).await;
            let _ = tokio::fs::rename(&ffprobe_tmp, &ffprobe_dest).await;
        }
    }

    let _ = tokio::fs::remove_file(&zip_path).await;
    let _ = tokio::fs::remove_dir_all(&extract_dir).await;
    Ok(ffmpeg_dest)
}

fn walkdir_shallow(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p);
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaUrlMatch {
    pub is_hls: bool,
    pub is_dash: bool,
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn has_direct_non_stream_extension(path_lower: &str) -> bool {
    let last_seg = path_lower.rsplit('/').next().unwrap_or("");
    let Some((_, ext)) = last_seg.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext,
        "zip"
            | "rar"
            | "7z"
            | "tar"
            | "gz"
            | "tgz"
            | "xz"
            | "bz2"
            | "zst"
            | "iso"
            | "img"
            | "dmg"
            | "bin"
            | "exe"
            | "msi"
            | "msix"
            | "apk"
            | "deb"
            | "rpm"
            | "pdf"
            | "epub"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "mp4"
            | "mkv"
            | "webm"
            | "avi"
            | "mov"
            | "flv"
            | "mp3"
            | "flac"
            | "wav"
            | "aac"
            | "ogg"
            | "m4a"
            | "png"
            | "jpg"
            | "jpeg"
            | "webp"
            | "gif"
            | "svg"
            | "json"
            | "xml"
            | "txt"
            | "csv"
            | "torrent"
    )
}

pub fn detect_media_url(raw_url: &str) -> Option<MediaUrlMatch> {
    let Ok(parsed) = url::Url::parse(raw_url.trim()) else {
        return None;
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    let path_lower = parsed.path().to_ascii_lowercase();

    if path_lower.ends_with(".m3u8") {
        return Some(MediaUrlMatch {
            is_hls: true,
            is_dash: false,
        });
    }
    if path_lower.ends_with(".mpd") {
        return Some(MediaUrlMatch {
            is_hls: false,
            is_dash: true,
        });
    }

    if has_direct_non_stream_extension(&path_lower) {
        return None;
    }

    let is_media_platform = is_youtube_url(raw_url)
        || ((host_matches(&host, "x.com") || host_matches(&host, "twitter.com"))
            && (path_lower.contains("/status/")
                || path_lower.contains("/i/broadcasts/")
                || path_lower.contains("/i/spaces/")))
        || host_matches(&host, "vxtwitter.com")
        || host_matches(&host, "fxtwitter.com")
        || host_matches(&host, "fixupx.com")
        || host_matches(&host, "tiktok.com")
        || (host_matches(&host, "instagram.com")
            && (path_lower.starts_with("/reel")
                || path_lower.starts_with("/p/")
                || path_lower.starts_with("/tv/")
                || path_lower.starts_with("/stories/")))
        || host_matches(&host, "twitch.tv")
        || host_matches(&host, "vimeo.com")
        || (host_matches(&host, "reddit.com")
            && (path_lower.contains("/comments/") || path_lower.starts_with("/s/")))
        || host_matches(&host, "v.redd.it")
        || host_matches(&host, "fb.watch")
        || (host_matches(&host, "facebook.com")
            && (path_lower.starts_with("/watch")
                || path_lower.contains("/videos/")
                || path_lower.starts_with("/reel")
                || path_lower.starts_with("/share/v/")
                || path_lower.starts_with("/share/r/")))
        || host_matches(&host, "kick.com")
        || host_matches(&host, "rumble.com")
        || host_matches(&host, "dailymotion.com")
        || host_matches(&host, "dai.ly")
        || host_matches(&host, "streamable.com")
        || host_matches(&host, "bilibili.com")
        || host_matches(&host, "b23.tv")
        || host_matches(&host, "odysee.com")
        || host_matches(&host, "soundcloud.com")
        || host_matches(&host, "bandcamp.com")
        || (host_matches(&host, "loom.com") && path_lower.starts_with("/share/"))
        || (host_matches(&host, "bsky.app") && path_lower.contains("/post/"))
        || ((host_matches(&host, "threads.net") || host_matches(&host, "threads.com"))
            && path_lower.contains("/post/"))
        || host_matches(&host, "redgifs.com");

    if is_media_platform {
        Some(MediaUrlMatch {
            is_hls: false,
            is_dash: false,
        })
    } else {
        None
    }
}

fn is_youtube_url(raw_url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw_url.trim()) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let h = host.to_ascii_lowercase();
    h == "youtube.com"
        || h.ends_with(".youtube.com")
        || h == "youtu.be"
        || h.ends_with(".youtu.be")
        || h == "youtube-nocookie.com"
        || h.ends_with(".youtube-nocookie.com")
}

fn has_cookie_header(headers: &HashMap<String, String>) -> bool {
    headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("cookie") && !v.trim().is_empty())
}

fn append_yt_dlp_headers(
    cmd: &mut Command,
    headers: &HashMap<String, String>,
    include_cookies: bool,
) {
    for (k, v) in headers {
        if v.trim().is_empty() {
            continue;
        }
        if !include_cookies && k.eq_ignore_ascii_case("cookie") {
            continue;
        }
        cmd.arg("--add-header").arg(format!("{k}:{v}"));
    }
}

fn detect_js_runtime() -> Option<&'static str> {
    let path_var = std::env::var_os("PATH")?;
    let candidates: &[(&str, &[&str])] = if cfg!(windows) {
        &[
            ("deno", &["deno.exe", "deno.cmd"]),
            ("node", &["node.exe"]),
            ("bun", &["bun.exe"]),
            ("qjs", &["qjs.exe"]),
        ]
    } else {
        &[
            ("deno", &["deno"]),
            ("node", &["node"]),
            ("bun", &["bun"]),
            ("qjs", &["qjs"]),
        ]
    };
    for (runtime, names) in candidates {
        for dir in std::env::split_paths(&path_var) {
            for bin in *names {
                if dir.join(bin).is_file() {
                    return Some(*runtime);
                }
            }
        }
    }
    None
}

async fn run_yt_dlp_probe(
    yt_dlp_path: &str,
    url: &str,
    headers: &HashMap<String, String>,
    include_cookies: bool,
    single_video: bool,
) -> Result<std::process::Output> {
    let mut cmd = Command::new(yt_dlp_path);
    crate::extractor::configure_silent_command(&mut cmd);
    cmd.env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .args(["-J", "--no-warnings"]);
    if single_video {
        cmd.arg("--no-playlist");
    } else {
        cmd.args(["--flat-playlist", "--playlist-end", "100"]);
    }
    if let Some(rt) = detect_js_runtime() {
        cmd.args(["--js-runtimes", rt]);
    }
    append_yt_dlp_headers(&mut cmd, headers, include_cookies);
    cmd.arg(url);
    cmd.output().await.context("Falha ao executar yt-dlp")
}

pub async fn probe_media_url(
    yt_dlp_path: &str,
    url: &str,
    headers: &HashMap<String, String>,
) -> Result<MediaProbeResult> {
    probe_media_url_impl(yt_dlp_path, url, headers, false).await
}

pub async fn probe_single_media_url(
    yt_dlp_path: &str,
    url: &str,
    headers: &HashMap<String, String>,
) -> Result<MediaProbeResult> {
    probe_media_url_impl(yt_dlp_path, url, headers, true).await
}

async fn probe_media_url_impl(
    yt_dlp_path: &str,
    url: &str,
    headers: &HashMap<String, String>,
    single_video: bool,
) -> Result<MediaProbeResult> {
    let include_cookies = !is_youtube_url(url);
    let mut output = run_yt_dlp_probe(yt_dlp_path, url, headers, include_cookies, single_video).await?;
    if !output.status.success() && include_cookies && has_cookie_header(headers) {
        output = run_yt_dlp_probe(yt_dlp_path, url, headers, false, single_video).await?;
    }
    if !output.status.success() {
        return Err(anyhow!(
            "yt-dlp erro: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let json: Value = serde_json::from_slice(&output.stdout)?;
    let title = json["title"]
        .as_str()
        .unwrap_or("Mídia sem título")
        .to_string();
    let uploader = json["uploader"].as_str().map(str::to_string);
    let duration_secs = json["duration"].as_f64();
    let thumbnail = json["thumbnail"].as_str().map(str::to_string);
    let extractor = json["extractor_key"]
        .as_str()
        .or_else(|| json["extractor"].as_str())
        .unwrap_or("yt-dlp")
        .to_string();

    let mut playlist_entries = Vec::new();
    if let Some(entries) = json["entries"].as_array() {
        for e in entries {
            let entry_url = e["url"]
                .as_str()
                .or_else(|| e["webpage_url"].as_str())
                .unwrap_or("")
                .to_string();
            if !entry_url.is_empty() {
                playlist_entries.push(MediaPlaylistEntry {
                    title: e["title"].as_str().unwrap_or("Item").to_string(),
                    url: entry_url,
                    duration_secs: e["duration"].as_f64(),
                });
            }
        }
    }

    let mut formats = Vec::new();
    if let Some(arr) = json["formats"].as_array() {
        for f in arr {
            let format_id = match f["format_id"].as_str() {
                Some(id) => id.to_string(),
                None => continue,
            };
            let ext = f["ext"].as_str().unwrap_or("mp4").to_string();
            let vcodec = f["vcodec"].as_str().map(str::to_string);
            let acodec = f["acodec"].as_str().map(str::to_string);
            let is_audio_only = vcodec.as_deref() == Some("none");
            let width = f["width"].as_u64();
            let height = f["height"].as_u64();
            let resolution = match (width, height) {
                (Some(w), Some(h)) => format!("{w}x{h}"),
                (_, Some(h)) => format!("{h}p"),
                _ => {
                    if is_audio_only {
                        "Áudio".to_string()
                    } else {
                        f["resolution"]
                            .as_str()
                            .unwrap_or("Desconhecido")
                            .to_string()
                    }
                }
            };
            let fps = f["fps"].as_f64();
            let filesize = f["filesize"].as_u64().or_else(|| f["filesize_approx"].as_u64());
            let note = f["format_note"]
                .as_str()
                .unwrap_or("")
                .to_string();
            formats.push(MediaFormatOption {
                format_id,
                ext,
                resolution,
                fps,
                vcodec,
                acodec,
                filesize,
                note,
                is_audio_only,
            });
        }
    }

    formats.reverse();

    Ok(MediaProbeResult {
        url: url.to_string(),
        title,
        uploader,
        duration_secs,
        thumbnail,
        is_playlist: !playlist_entries.is_empty(),
        playlist_count: playlist_entries.len(),
        playlist_entries,
        formats,
        extractor,
    })
}

pub async fn run_media_download<F>(
    client: Client,
    item: DownloadItem,
    yt_dlp_path: Option<String>,
    ffmpeg_path: Option<String>,
    cancel: Arc<DownloadCancelHandle>,
    on_progress: F,
) -> Result<DownloadItem>
where
    F: Fn(Option<DownloadStatus>, Option<String>, Option<PathBuf>, u64, Option<u64>, u64, Option<u64>)
        + Send
        + Sync
        + 'static,
{
    run_media_download_with_resolver(
        client,
        item,
        yt_dlp_path,
        ffmpeg_path,
        cancel,
        None,
        on_progress,
    )
    .await
}

pub async fn run_media_download_with_resolver<F>(
    client: Client,
    mut item: DownloadItem,
    yt_dlp_path: Option<String>,
    ffmpeg_path: Option<String>,
    cancel: Arc<DownloadCancelHandle>,
    resolve_destination: Option<DestinationResolver>,
    on_progress: F,
) -> Result<DownloadItem>
where
    F: Fn(Option<DownloadStatus>, Option<String>, Option<PathBuf>, u64, Option<u64>, u64, Option<u64>)
        + Send
        + Sync
        + 'static,
{
    tokio::fs::create_dir_all(&item.save_dir).await?;
    let is_hls = item
        .media_info
        .as_ref()
        .map(|m| m.is_hls)
        .unwrap_or_else(|| item.url.to_lowercase().contains(".m3u8"));

    if is_hls && yt_dlp_path.is_none() {
        return run_native_hls_download(
            client,
            item,
            ffmpeg_path,
            cancel,
            resolve_destination,
            on_progress,
        )
        .await;
    }

    if let Some(yt_dlp) = yt_dlp_path {
        let res = run_yt_dlp_download(
            &yt_dlp,
            ffmpeg_path.as_deref(),
            &mut item,
            &cancel,
            &on_progress,
        )
        .await;
        // Check pause/cancel before treating early exit as completion
        if cancel.paused.load(Ordering::SeqCst) {
            item.status = DownloadStatus::Paused;
            item.speed_bps = 0;
            return Ok(item);
        }
        if cancel.cancelled.load(Ordering::SeqCst) {
            return Err(anyhow!("Download cancelado"));
        }
        if res.is_ok() {
            return Ok(item);
        }
        if is_hls {
            return run_native_hls_download(
                client,
                item,
                ffmpeg_path,
                cancel,
                resolve_destination,
                on_progress,
            )
            .await;
        }
        return Err(res.unwrap_err());
    }

    if is_hls {
        return run_native_hls_download(
            client,
            item,
            ffmpeg_path,
            cancel,
            resolve_destination,
            on_progress,
        )
        .await;
    }

    Err(anyhow!(
        "yt-dlp não está instalado. Clique em 'Instalar yt-dlp / FFmpeg' nas configurações ou na aba Mídia."
    ))
}

async fn run_yt_dlp_download<F>(
    yt_dlp_path: &str,
    ffmpeg_path: Option<&str>,
    item: &mut DownloadItem,
    cancel: &Arc<DownloadCancelHandle>,
    on_progress: &F,
) -> Result<()>
where
    F: Fn(Option<DownloadStatus>, Option<String>, Option<PathBuf>, u64, Option<u64>, u64, Option<u64>)
        + Send
        + Sync,
{
    let include_cookies = !is_youtube_url(&item.url);
    let res = run_yt_dlp_download_attempt(
        yt_dlp_path,
        ffmpeg_path,
        item,
        cancel,
        on_progress,
        include_cookies,
    )
    .await;
    if res.is_err() && !cancel.is_stopped() && include_cookies && has_cookie_header(&item.headers) {
        return run_yt_dlp_download_attempt(
            yt_dlp_path,
            ffmpeg_path,
            item,
            cancel,
            on_progress,
            false,
        )
        .await;
    }
    res
}

async fn run_yt_dlp_download_attempt<F>(
    yt_dlp_path: &str,
    ffmpeg_path: Option<&str>,
    item: &mut DownloadItem,
    cancel: &Arc<DownloadCancelHandle>,
    on_progress: &F,
    include_cookies: bool,
) -> Result<()>
where
    F: Fn(Option<DownloadStatus>, Option<String>, Option<PathBuf>, u64, Option<u64>, u64, Option<u64>)
        + Send
        + Sync,
{
    on_progress(
        Some(DownloadStatus::Probing),
        None,
        None,
        item.downloaded_bytes,
        item.total_bytes,
        0,
        None,
    );

    let output_template = item
        .save_dir
        .join("%(title).120B [%(id)s].%(ext)s")
        .to_string_lossy()
        .to_string();

    let mut cmd = Command::new(yt_dlp_path);
    crate::extractor::configure_silent_command(&mut cmd);
    cmd.env("PYTHONUNBUFFERED", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .arg("--newline")
        .arg("--no-quiet")
        .arg("--progress")
        .arg("--no-playlist")
        .arg("--no-warnings")
        .arg("--continue")
        .arg("--windows-filenames")
        .arg("-o")
        .arg(&output_template)
        .arg("--print")
        .arg("after_move:VORTEX_FINAL_PATH:%(filepath)s");

    if let Some(rt) = detect_js_runtime() {
        cmd.args(["--js-runtimes", rt]);
    }

    if let Some(ffmpeg) = ffmpeg_path {
        if let Some(dir) = Path::new(ffmpeg).parent() {
            cmd.arg("--ffmpeg-location").arg(dir);
        }
    }

    if let Some(ref media) = item.media_info {
        if media.audio_only {
            cmd.args(["-x", "--audio-format", "mp3", "--audio-quality", "0"]);
        } else if let Some(ref fmt) = media.format_id {
            if fmt.contains('+') {
                cmd.arg("-f").arg(fmt);
            } else {
                cmd.arg("-f").arg(format!("{fmt}+bestaudio/best"));
            }
            cmd.args(["--merge-output-format", "mp4"]);
        } else {
            cmd.args(["-f", "bestvideo+bestaudio/best", "--merge-output-format", "mp4"]);
        }
        if media.embed_subtitles {
            cmd.args(["--write-subs", "--embed-subs", "--sub-langs", "all,-live_chat"]);
        }
    } else {
        cmd.args(["-f", "bestvideo+bestaudio/best", "--merge-output-format", "mp4"]);
    }

    append_yt_dlp_headers(&mut cmd, &item.headers, include_cookies);

    cmd.arg(&item.url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW

    let mut child = cmd.spawn().context("Falha ao iniciar yt-dlp")?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("Sem stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("Sem stderr"))?;

    let stderr_buf = Arc::new(parking_lot::Mutex::new(String::new()));
    let stderr_clone = stderr_buf.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buf = Vec::with_capacity(512);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim_end_matches(['\r', '\n']);
                    let mut g = stderr_clone.lock();
                    if g.len() < 4096 {
                        g.push_str(line);
                        g.push('\n');
                    }
                }
            }
        }
    });

    on_progress(
        Some(DownloadStatus::Downloading),
        None,
        None,
        item.downloaded_bytes,
        item.total_bytes,
        0,
        None,
    );

    let mut stdout_reader = BufReader::new(stdout);
    let mut line_buf = Vec::with_capacity(1024);
    let mut final_path: Option<PathBuf> = None;
    let mut fallback_path: Option<PathBuf> = None;
    let mut completed_streams_bytes: u64 = 0;
    let mut current_stream_total: Option<u64> = None;

    loop {
        if cancel.is_stopped() {
            let _ = child.kill().await;
            break;
        }
        line_buf.clear();
        let read_res = tokio::select! {
            res = stdout_reader.read_until(b'\n', &mut line_buf) => res?,
            _ = cancel.notify.notified() => {
                let _ = child.kill().await;
                break;
            }
        };
        if read_res == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&line_buf)
            .trim_end_matches(['\r', '\n'])
            .to_string();

        if let Some(p) = line.strip_prefix("VORTEX_FINAL_PATH:") {
            let pb = PathBuf::from(p.trim());
            final_path = Some(pb.clone());
            if let Some(name) = pb.file_name().and_then(|n| n.to_str()) {
                item.filename = name.to_string();
            }
            item.file_path = pb;
            item.category = FileCategory::from_filename(&item.filename);
            continue;
        }

        if let Some(dest) = line.strip_prefix("[download] Destination:") {
            if let Some(prev_tot) = current_stream_total.take() {
                completed_streams_bytes = completed_streams_bytes.saturating_add(prev_tot);
            }
            let pb = PathBuf::from(dest.trim());
            if !is_ytdlp_intermediate_stream(&pb) || fallback_path.is_none() {
                fallback_path = Some(pb.clone());
            }
            if let Some(name) = pb.file_name().and_then(|n| n.to_str()) {
                item.filename = name.to_string();
                on_progress(
                    None,
                    Some(item.filename.clone()),
                    Some(pb),
                    item.downloaded_bytes,
                    item.total_bytes,
                    0,
                    None,
                );
            }
            continue;
        }

        if let Some(pb) = parse_ytdlp_already_downloaded(&line) {
            if !is_ytdlp_intermediate_stream(&pb) || fallback_path.is_none() {
                fallback_path = Some(pb.clone());
            }
            if let Some(name) = pb.file_name().and_then(|n| n.to_str()) {
                item.filename = name.to_string();
                item.category = FileCategory::from_filename(&item.filename);
                on_progress(
                    None,
                    Some(item.filename.clone()),
                    Some(pb),
                    item.downloaded_bytes,
                    item.total_bytes,
                    0,
                    None,
                );
            }
            continue;
        }

        if let Some(pb) = parse_ytdlp_merger_path(&line).or_else(|| parse_ytdlp_extract_audio_path(&line)) {
            fallback_path = Some(pb.clone());
            if let Some(name) = pb.file_name().and_then(|n| n.to_str()) {
                item.filename = name.to_string();
                item.category = FileCategory::from_filename(&item.filename);
            }
            item.file_path = pb.clone();
            on_progress(
                Some(DownloadStatus::Merging),
                Some(item.filename.clone()),
                Some(pb),
                item.downloaded_bytes,
                item.total_bytes,
                0,
                None,
            );
            continue;
        }

        if line.contains("[Merger]") || line.contains("[ExtractAudio]") {
            on_progress(
                Some(DownloadStatus::Merging),
                None,
                None,
                item.downloaded_bytes,
                item.total_bytes,
                0,
                None,
            );
            continue;
        }

        if let Some((pct, total_est, speed_bps, eta)) = parse_ytdlp_progress(&line) {
            if total_est.is_some() {
                current_stream_total = total_est;
            }
            if let Some(stream_tot) = current_stream_total {
                let stream_dl = ((pct / 100.0).clamp(0.0, 1.0) * stream_tot as f64) as u64;
                let total = completed_streams_bytes.saturating_add(stream_tot);
                let downloaded = completed_streams_bytes.saturating_add(stream_dl);
                item.total_bytes = Some(total);
                item.downloaded_bytes = downloaded;
            }
            item.speed_bps = speed_bps;
            item.eta_seconds = eta;
            on_progress(
                Some(DownloadStatus::Downloading),
                None,
                None,
                item.downloaded_bytes,
                item.total_bytes,
                speed_bps,
                eta,
            );
        }
    }

    if cancel.is_stopped() {
        return Ok(());
    }

    let status = child.wait().await?;
    if !status.success() {
        let err_txt = stderr_buf.lock().clone();
        return Err(anyhow!("yt-dlp falhou ({status}): {}", err_txt.trim()));
    }

    if let Some(pb) = resolve_ytdlp_final_path(
        final_path.as_deref(),
        fallback_path.as_deref(),
        Some(item.file_path.as_path()),
        &item.save_dir,
        &item.url,
    ) {
        if let Some(name) = pb.file_name().and_then(|n| n.to_str()) {
            item.filename = name.to_string();
        }
        item.category = FileCategory::from_filename(&item.filename);
        item.file_path = pb.clone();
        if let Ok(meta) = tokio::fs::metadata(&pb).await {
            item.total_bytes = Some(meta.len());
            item.downloaded_bytes = meta.len();
        }
    }
    item.status = DownloadStatus::Completed;
    item.speed_bps = 0;
    item.eta_seconds = Some(0);
    item.completed_at = Some(chrono::Utc::now());
    Ok(())
}

fn parse_ytdlp_already_downloaded(line: &str) -> Option<PathBuf> {
    let rest = line.strip_prefix("[download] ")?;
    let path = rest.strip_suffix(" has already been downloaded")?;
    let trimmed = path.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn parse_ytdlp_merger_path(line: &str) -> Option<PathBuf> {
    let rest = line.strip_prefix("[Merger] Merging formats into ")?;
    let trimmed = rest.trim().trim_matches('"');
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn parse_ytdlp_extract_audio_path(line: &str) -> Option<PathBuf> {
    let rest = line.strip_prefix("[ExtractAudio] Destination:")?;
    let trimmed = rest.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn is_ytdlp_intermediate_stream(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let Some((_, tail)) = stem.rsplit_once(".f") else {
        return false;
    };
    !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit())
}

fn extract_bracket_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let open = name.rfind('[')?;
    let close = name[open + 1..].find(']')?;
    let id = &name[open + 1..open + 1 + close];
    (!id.is_empty()).then(|| id.to_string())
}

fn extract_url_media_id(url: &str) -> Option<String> {
    if let Ok(parsed) = reqwest::Url::parse(url) {
        for (k, v) in parsed.query_pairs() {
            if k == "v" && !v.is_empty() {
                return Some(v.into_owned());
            }
        }
        if let Some(host) = parsed.host_str() {
            if host.contains("youtu.be") {
                if let Some(seg) = parsed.path_segments().and_then(|mut s| s.next()) {
                    if !seg.is_empty() {
                        return Some(seg.to_string());
                    }
                }
            }
            if host.contains("youtube.com") {
                let segs: Vec<&str> = parsed.path_segments()?.collect();
                if segs.len() >= 2 && matches!(segs[0], "shorts" | "embed" | "live") && !segs[1].is_empty() {
                    return Some(segs[1].to_string());
                }
            }
        }
    }
    None
}

fn resolve_ytdlp_final_path(
    final_path: Option<&Path>,
    fallback_path: Option<&Path>,
    current_path: Option<&Path>,
    save_dir: &Path,
    url: &str,
) -> Option<PathBuf> {
    for candidate in [final_path, fallback_path, current_path].into_iter().flatten() {
        if !candidate.as_os_str().is_empty() && candidate.is_file() {
            return Some(candidate.to_path_buf());
        }
    }

    let id_token = final_path
        .and_then(extract_bracket_id)
        .or_else(|| fallback_path.and_then(extract_bracket_id))
        .or_else(|| current_path.and_then(extract_bracket_id))
        .or_else(|| extract_url_media_id(url))?;
    let needle = format!("[{id_token}]");

    let entries = std::fs::read_dir(save_dir).ok()?;
    let mut best: Option<(bool, std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".part") || lower.ends_with(".ytdl") || lower.ends_with(".tmp") {
            continue;
        }
        if !name.contains(&needle) {
            continue;
        }
        let is_final = !is_ytdlp_intermediate_stream(&path);
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let replace = match &best {
            None => true,
            Some((best_final, best_mod, _)) => (is_final, modified) > (*best_final, *best_mod),
        };
        if replace {
            best = Some((is_final, modified, path));
        }
    }
    best.map(|(_, _, p)| p)
}

fn parse_ytdlp_progress(line: &str) -> Option<(f64, Option<u64>, u64, Option<u64>)> {
    if !line.starts_with("[download]") || !line.contains('%') {
        return None;
    }
    let pct_idx = line.find('%')?;
    let before_pct = &line[..pct_idx];
    let pct_str = before_pct.split_whitespace().last()?;
    let pct: f64 = pct_str.parse().ok()?;

    let mut total_bytes = None;
    if let Some(of_idx) = line.find(" of ") {
        let after_of = line[of_idx + 4..].trim_start().trim_start_matches('~').trim_start();
        let size_token = after_of.split_whitespace().next().unwrap_or("");
        total_bytes = parse_human_bytes(size_token);
    }

    let mut speed_bps = 0u64;
    if let Some(at_idx) = line.find(" at ") {
        let after_at = line[at_idx + 4..].trim_start();
        let spd_token = after_at.split_whitespace().next().unwrap_or("");
        if let Some(b) = parse_human_bytes(spd_token.trim_end_matches("/s")) {
            speed_bps = b;
        }
    }

    let mut eta = None;
    if let Some(eta_idx) = line.find("ETA ") {
        let token = line[eta_idx + 4..].split_whitespace().next().unwrap_or("");
        eta = parse_eta_hms(token);
    }

    Some((pct, total_bytes, speed_bps, eta))
}

fn parse_human_bytes(token: &str) -> Option<u64> {
    let clean = token.trim();
    if clean.is_empty() || clean == "Unknown" || clean == "N/A" {
        return None;
    }
    let split_pos = clean
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(clean.len());
    let num: f64 = clean[..split_pos].parse().ok()?;
    let unit = clean[split_pos..].trim().to_lowercase();
    let mult = match unit.as_str() {
        "b" | "" => 1.0,
        "kib" | "kb" => 1024.0,
        "mib" | "mb" => 1024.0 * 1024.0,
        "gib" | "gb" => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    Some((num * mult) as u64)
}

fn parse_eta_hms(token: &str) -> Option<u64> {
    let parts: Vec<&str> = token.split(':').collect();
    match parts.len() {
        2 => {
            let m: u64 = parts[0].parse().ok()?;
            let s: u64 = parts[1].parse().ok()?;
            Some(m * 60 + s)
        }
        3 => {
            let h: u64 = parts[0].parse().ok()?;
            let m: u64 = parts[1].parse().ok()?;
            let s: u64 = parts[2].parse().ok()?;
            Some(h * 3600 + m * 60 + s)
        }
        _ => None,
    }
}

/// Native Rust HLS (.m3u8) parser + multi-connection segment downloader.
/// Works independently of yt-dlp for raw sniffed .m3u8 streams from any site!
async fn run_native_hls_download<F>(
    client: Client,
    mut item: DownloadItem,
    ffmpeg_path: Option<String>,
    cancel: Arc<DownloadCancelHandle>,
    resolve_destination: Option<DestinationResolver>,
    on_progress: F,
) -> Result<DownloadItem>
where
    F: Fn(Option<DownloadStatus>, Option<String>, Option<PathBuf>, u64, Option<u64>, u64, Option<u64>)
        + Send
        + Sync,
{
    on_progress(
        Some(DownloadStatus::Probing),
        None,
        None,
        0,
        None,
        0,
        None,
    );

    let playlist_text = apply_custom_headers(client.get(&item.url), &item.headers)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let base_url = url::Url::parse(&item.url)?;
    let media_playlist_url = select_best_hls_variant(&base_url, &playlist_text);

    let (final_playlist_url, media_text) = if let Some(variant_url) = media_playlist_url {
        let txt = apply_custom_headers(client.get(variant_url.as_str()), &item.headers)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        (variant_url, txt)
    } else {
        (base_url, playlist_text)
    };

    let segment_urls = parse_hls_segments(&final_playlist_url, &media_text);
    if segment_urls.is_empty() {
        return Err(anyhow!("Nenhum segmento de mídia encontrado no manifest HLS"));
    }

    let base_name = sanitize_filename(
        item.media_info
            .as_ref()
            .and_then(|m| m.title.as_deref())
            .unwrap_or("stream_video"),
    );
    let desired_mp4_name = format!("{base_name}.mp4");
    let mp4_path = match resolve_destination.as_ref() {
        Some(resolver) => resolver(item.id, &item.save_dir, &desired_mp4_name, false),
        None => unique_destination_path(&item.save_dir, &desired_mp4_name),
    };
    let final_mp4_name = mp4_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or(desired_mp4_name);
    let ts_candidate_name = mp4_path
        .with_extension("ts")
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{base_name}.ts"));
    let ts_path = unique_destination_path(&item.save_dir, &ts_candidate_name);

    on_progress(
        Some(DownloadStatus::Downloading),
        Some(final_mp4_name),
        Some(mp4_path.clone()),
        0,
        None,
        0,
        None,
    );

    let total_segs = segment_urls.len();
    let downloaded_bytes = Arc::new(AtomicU64::new(0));
    let completed_segs = Arc::new(AtomicU64::new(0));
    let done_flag = Arc::new(AtomicBool::new(false));

    let seg_data: Arc<parking_lot::Mutex<Vec<Option<Vec<u8>>>>> =
        Arc::new(parking_lot::Mutex::new(vec![None; total_segs]));
    let next_idx = Arc::new(AtomicU64::new(0));
    let err_slot: Arc<parking_lot::Mutex<Option<String>>> = Arc::new(parking_lot::Mutex::new(None));

    let concurrency = 6usize;
    let mut workers = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let client = client.clone();
        let headers = item.headers.clone();
        let seg_urls = segment_urls.clone();
        let seg_data = seg_data.clone();
        let next_idx = next_idx.clone();
        let downloaded_bytes = downloaded_bytes.clone();
        let completed_segs = completed_segs.clone();
        let cancel = cancel.clone();
        let err_slot = err_slot.clone();

        workers.push(tokio::spawn(async move {
            loop {
                if cancel.is_stopped() || err_slot.lock().is_some() {
                    break;
                }
                let idx = next_idx.fetch_add(1, Ordering::Relaxed) as usize;
                if idx >= seg_urls.len() {
                    break;
                }
                let seg_url = &seg_urls[idx];
                let mut ok = false;
                for _ in 0..3 {
                    if let Ok(resp) = apply_custom_headers(client.get(seg_url.as_str()), &headers)
                        .send()
                        .await
                    {
                        if resp.status().is_success() {
                            if let Ok(bytes) = resp.bytes().await {
                                downloaded_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                                seg_data.lock()[idx] = Some(bytes.to_vec());
                                completed_segs.fetch_add(1, Ordering::Relaxed);
                                ok = true;
                                break;
                            }
                        }
                    }
                }
                if !ok && !cancel.is_stopped() {
                    *err_slot.lock() = Some(format!("Falha ao baixar segmento HLS #{idx}"));
                    break;
                }
            }
        }));
    }

    let start_t = std::time::Instant::now();
    let mut prev_bytes = 0u64;
    while completed_segs.load(Ordering::Relaxed) < total_segs as u64
        && !cancel.is_stopped()
        && err_slot.lock().is_none()
    {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let cur = downloaded_bytes.load(Ordering::Relaxed);
        let segs_done = completed_segs.load(Ordering::Relaxed).max(1);
        let est_total = (cur as f64 * (total_segs as f64 / segs_done as f64)) as u64;
        let speed = ((cur.saturating_sub(prev_bytes)) as f64 / 0.3) as u64;
        prev_bytes = cur;
        let _ = start_t;
        on_progress(
            Some(DownloadStatus::Downloading),
            None,
            None,
            cur,
            Some(est_total.max(cur)),
            speed,
            None,
        );
    }

    for w in workers {
        let _ = w.await;
    }
    done_flag.store(true, Ordering::Relaxed);

    if cancel.paused.load(Ordering::SeqCst) {
        item.status = DownloadStatus::Paused;
        return Ok(item);
    }
    if let Some(err) = err_slot.lock().take() {
        return Err(anyhow!(err));
    }

    on_progress(
        Some(DownloadStatus::Merging),
        None,
        None,
        downloaded_bytes.load(Ordering::Relaxed),
        Some(downloaded_bytes.load(Ordering::Relaxed)),
        0,
        None,
    );

    {
        let mut out_file = std::fs::File::create(&ts_path)?;
        let guard = seg_data.lock();
        for part in guard.iter().flatten() {
            out_file.write_all(part)?;
        }
        out_file.flush()?;
    }

    let final_output = if let Some(ffmpeg) = ffmpeg_path {
        let mp4_path = if mp4_path.exists() {
            let desired = mp4_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("stream_video.mp4");
            match resolve_destination.as_ref() {
                Some(resolver) => resolver(item.id, &item.save_dir, desired, false),
                None => unique_destination_path(&item.save_dir, desired),
            }
        } else {
            mp4_path
        };
        let mut ff_cmd = Command::new(&ffmpeg);
        crate::extractor::configure_silent_command(&mut ff_cmd);
        let out = ff_cmd
            .args([
                "-y",
                "-i",
                &ts_path.to_string_lossy(),
                "-c",
                "copy",
                "-bsf:a",
                "aac_adtstoasc",
                &mp4_path.to_string_lossy(),
            ])
            .output()
            .await;
        if let Ok(o) = out {
            if o.status.success() {
                let _ = tokio::fs::remove_file(&ts_path).await;
                mp4_path
            } else {
                ts_path
            }
        } else {
            ts_path
        }
    } else {
        ts_path
    };

    let size = tokio::fs::metadata(&final_output).await?.len();
    item.file_path = final_output.clone();
    item.filename = final_output
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video.mp4")
        .to_string();
    item.category = FileCategory::Video;
    item.total_bytes = Some(size);
    item.downloaded_bytes = size;
    item.speed_bps = 0;
    item.eta_seconds = Some(0);
    item.status = DownloadStatus::Completed;
    item.completed_at = Some(chrono::Utc::now());
    Ok(item)
}

fn select_best_hls_variant(base: &url::Url, playlist: &str) -> Option<url::Url> {
    let mut best_bw = 0u64;
    let mut best_url = None;
    let lines: Vec<&str> = playlist.lines().map(str::trim).collect();
    for i in 0..lines.len() {
        if lines[i].starts_with("#EXT-X-STREAM-INF:") {
            let bw = lines[i]
                .split(',')
                .find_map(|p| p.trim().strip_prefix("BANDWIDTH="))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(1);
            if let Some(next_line) = lines.get(i + 1) {
                if !next_line.is_empty() && !next_line.starts_with('#') && bw >= best_bw {
                    if let Ok(u) = base.join(next_line) {
                        best_bw = bw;
                        best_url = Some(u);
                    }
                }
            }
        }
    }
    best_url
}

fn parse_hls_segments(base: &url::Url, playlist: &str) -> Vec<url::Url> {
    let mut segs = Vec::new();
    for line in playlist.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Ok(u) = base.join(trimmed) {
            segs.push(u);
        }
    }
    segs
}

pub fn resolve_ffprobe_path(ffmpeg_path: Option<&str>) -> Option<String> {
    if let Some(ff) = ffmpeg_path {
        let p = Path::new(ff);
        if let Some(parent) = p.parent() {
            for name in ["ffprobe.exe", "ffprobe"] {
                let candidate = parent.join(name);
                if candidate.exists() {
                    return Some(candidate.to_string_lossy().to_string());
                }
            }
        }
    }
    find_binary_everywhere(&["ffprobe.exe", "ffprobe"])
}

fn parse_ffprobe_rate(rate: &str) -> Option<f64> {
    let trimmed = rate.trim();
    if trimmed.is_empty() || trimmed == "0/0" {
        return None;
    }
    if let Some((num, den)) = trimmed.split_once('/') {
        let n: f64 = num.trim().parse().ok()?;
        let d: f64 = den.trim().parse().ok()?;
        if d > 0.0 && n > 0.0 {
            return Some(n / d);
        }
        return None;
    }
    let val: f64 = trimmed.parse().ok()?;
    if val > 0.0 { Some(val) } else { None }
}

pub async fn probe_video_file(path: &Path, ffmpeg_path: Option<&str>) -> Result<VideoProbeInfo> {
    if !path.exists() {
        return Err(anyhow!("Arquivo não encontrado: {}", path.display()));
    }
    let meta = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("Não foi possível ler metadados de {}", path.display()))?;
    let file_size_bytes = meta.len();
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video.mp4")
        .to_string();

    if let Some(ffprobe) = resolve_ffprobe_path(ffmpeg_path) {
        let mut cmd = Command::new(&ffprobe);
        crate::extractor::configure_silent_command(&mut cmd);
        let out = cmd
            .args([
                "-v",
                "quiet",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
                &path.to_string_lossy(),
            ])
            .output()
            .await
            .context("Falha ao executar ffprobe")?;

        if out.status.success() {
            if let Ok(json) = serde_json::from_slice::<Value>(&out.stdout) {
                let streams = json["streams"].as_array();
                let video_stream = streams.and_then(|arr| {
                    arr.iter()
                        .find(|s| s["codec_type"].as_str() == Some("video"))
                });
                let audio_stream = streams.and_then(|arr| {
                    arr.iter()
                        .find(|s| s["codec_type"].as_str() == Some("audio"))
                });

                let duration_secs = json["format"]["duration"]
                    .as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .or_else(|| {
                        video_stream
                            .and_then(|s| s["duration"].as_str())
                            .and_then(|s| s.parse::<f64>().ok())
                    })
                    .unwrap_or(0.0);

                let width = video_stream
                    .and_then(|s| s["width"].as_u64())
                    .unwrap_or(0) as u32;
                let height = video_stream
                    .and_then(|s| s["height"].as_u64())
                    .unwrap_or(0) as u32;
                let fps = video_stream
                    .and_then(|s| {
                        s["avg_frame_rate"]
                            .as_str()
                            .and_then(parse_ffprobe_rate)
                            .or_else(|| s["r_frame_rate"].as_str().and_then(parse_ffprobe_rate))
                    })
                    .unwrap_or(30.0)
                    .clamp(1.0, 240.0);

                let video_codec = video_stream
                    .and_then(|s| s["codec_name"].as_str())
                    .unwrap_or("desconhecido")
                    .to_string();

                let total_bitrate_bps = json["format"]["bit_rate"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|&b| b > 0)
                    .unwrap_or_else(|| {
                        if duration_secs > 0.2 {
                            ((file_size_bytes as f64 * 8.0) / duration_secs) as u64
                        } else {
                            0
                        }
                    });

                let has_audio = audio_stream.is_some();
                let audio_codec = audio_stream
                    .and_then(|s| s["codec_name"].as_str())
                    .map(|s| s.to_string());
                let audio_bitrate_bps = audio_stream
                    .and_then(|s| s["bit_rate"].as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|&b| b > 0);

                return Ok(VideoProbeInfo {
                    path: path.to_path_buf(),
                    filename,
                    file_size_bytes,
                    duration_secs,
                    width,
                    height,
                    fps,
                    video_codec,
                    total_bitrate_bps,
                    has_audio,
                    audio_codec,
                    audio_bitrate_bps,
                });
            }
        }
    }

    // Fallback: inspect via `ffmpeg -i` stderr when ffprobe is unavailable
    let ffmpeg = ffmpeg_path
        .map(|s| s.to_string())
        .or_else(|| find_binary_everywhere(&["ffmpeg.exe", "ffmpeg"]))
        .ok_or_else(|| anyhow!("FFmpeg / ffprobe não encontrado. Instale na barra superior da aba Mídia."))?;

    let mut cmd = Command::new(&ffmpeg);
    crate::extractor::configure_silent_command(&mut cmd);
    let out = cmd.args(["-i", &path.to_string_lossy()]).output().await?;
    let stderr = String::from_utf8_lossy(&out.stderr);

    let duration_secs = regex::Regex::new(r"Duration:\s*(\d+):(\d+):(\d+(?:\.\d+)?)")
        .ok()
        .and_then(|re| re.captures(&stderr))
        .map(|c| {
            let h: f64 = c[1].parse().unwrap_or(0.0);
            let m: f64 = c[2].parse().unwrap_or(0.0);
            let s: f64 = c[3].parse().unwrap_or(0.0);
            h * 3600.0 + m * 60.0 + s
        })
        .unwrap_or(0.0);

    let (width, height) = regex::Regex::new(r"Video:.*?,\s*(\d{2,5})x(\d{2,5})")
        .ok()
        .and_then(|re| re.captures(&stderr))
        .map(|c| (c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0)))
        .unwrap_or((0, 0));

    let total_bitrate_bps = if duration_secs > 0.2 {
        ((file_size_bytes as f64 * 8.0) / duration_secs) as u64
    } else {
        0
    };

    Ok(VideoProbeInfo {
        path: path.to_path_buf(),
        filename,
        file_size_bytes,
        duration_secs,
        width,
        height,
        fps: 30.0,
        video_codec: "video".to_string(),
        total_bitrate_bps,
        has_audio: stderr.contains("Audio:"),
        audio_codec: None,
        audio_bitrate_bps: None,
    })
}

fn round_even(v: u32) -> u32 {
    let clamped = v.max(2);
    if clamped % 2 == 0 {
        clamped
    } else {
        clamped - 1
    }
}

fn scale_keeping_aspect_by_short_side(src_w: u32, src_h: u32, target_short: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0);
    }
    let src_short = src_w.min(src_h);
    if target_short >= src_short {
        return (round_even(src_w), round_even(src_h));
    }
    if src_w >= src_h {
        let out_h = round_even(target_short);
        let out_w = round_even(((src_w as f64) * (out_h as f64) / (src_h as f64)).round() as u32);
        (out_w, out_h)
    } else {
        let out_w = round_even(target_short);
        let out_h = round_even(((src_h as f64) * (out_w as f64) / (src_w as f64)).round() as u32);
        (out_w, out_h)
    }
}

pub fn plan_video_compression(
    probe: &VideoProbeInfo,
    target_ratio: f32,
    codec: VideoCompressionCodec,
    auto_downscale: bool,
) -> VideoCompressionPlan {
    let ratio = target_ratio.clamp(0.10, 0.95);
    let min_bytes = probe.file_size_bytes.min(64 * 1024);
    let target_bytes = ((probe.file_size_bytes as f64) * (ratio as f64))
        .round()
        .max(min_bytes as f64) as u64;

    let t = ((0.95 - ratio) / (0.95 - 0.10)).clamp(0.0, 1.0);
    let crf_fallback = match codec {
        VideoCompressionCodec::Hevc => (18.0 + t * 14.0).round() as u8,
        VideoCompressionCodec::H264 => (17.0 + t * 15.0).round() as u8,
    };

    let two_pass = probe.duration_secs > 0.25 && probe.file_size_bytes > 0;
    const CONTAINER_EFFICIENCY: f64 = 0.982;

    let (video_bitrate_bps, audio_bitrate_bps, copy_audio, estimated_bytes) = if two_pass {
        let usable_total_bps =
            ((target_bytes as f64 * 8.0) / probe.duration_secs) * CONTAINER_EFFICIENCY;

        let (audio_bps, copy_audio) = if !probe.has_audio {
            (0u64, false)
        } else {
            // Prioritize audio clarity: preserve >=128 kbps whenever the budget allows it,
            // and stream-copy existing AAC audio directly whenever it fits comfortably so
            // there is zero generational quality loss.
            let tier_bps: u64 = if usable_total_bps >= 1_400_000.0 {
                192_000
            } else if usable_total_bps >= 700_000.0 {
                160_000
            } else if usable_total_bps >= 300_000.0 {
                128_000
            } else if usable_total_bps >= 180_000.0 {
                96_000
            } else {
                64_000
            };
            let max_audio_share = (usable_total_bps * 0.40) as u64;
            let floor_audio = if usable_total_bps >= 200_000.0 {
                64_000
            } else {
                48_000
            };
            let chosen_audio = if let Some(src_abps) = probe.audio_bitrate_bps {
                tier_bps
                    .min(src_abps.max(floor_audio))
                    .min(max_audio_share.max(floor_audio))
            } else {
                tier_bps.min(max_audio_share.max(floor_audio))
            };

            if let Some(src_abps) = probe.audio_bitrate_bps {
                let is_aac = probe
                    .audio_codec
                    .as_deref()
                    .map(|c| c.eq_ignore_ascii_case("aac"))
                    .unwrap_or(false);
                if is_aac
                    && src_abps <= 192_000
                    && src_abps <= max_audio_share.max(128_000)
                    && usable_total_bps >= (src_abps as f64) + 140_000.0
                {
                    (src_abps, true)
                } else {
                    (chosen_audio, false)
                }
            } else {
                (chosen_audio, false)
            }
        };

        let video_bps = (usable_total_bps.round() as u64)
            .saturating_sub(audio_bps)
            .max(40_000);
        let est_bytes = (((video_bps + audio_bps) as f64 / 8.0) * probe.duration_secs
            / CONTAINER_EFFICIENCY)
            .round() as u64;
        (video_bps, audio_bps, copy_audio, est_bytes)
    } else {
        let fallback_audio = if probe.has_audio { 160_000 } else { 0 };
        (1_500_000, fallback_audio, false, target_bytes)
    };

    let src_w = round_even(probe.width);
    let src_h = round_even(probe.height);
    let fps = probe.fps.clamp(12.0, 120.0);
    let min_bpp = match codec {
        VideoCompressionCodec::Hevc => 0.042,
        VideoCompressionCodec::H264 => 0.068,
    };

    let mut out_w = src_w;
    let mut out_h = src_h;
    if auto_downscale && two_pass && probe.width > 0 && probe.height > 0 {
        let native_bpp = (video_bitrate_bps as f64) / ((src_w as f64) * (src_h as f64) * fps);
        if native_bpp < min_bpp {
            let src_short = src_w.min(src_h);
            let mut chosen = (src_w, src_h);
            let candidates = [2160u32, 1440, 1080, 900, 720, 540, 480, 360];
            let mut matched = false;
            for cand_short in candidates {
                if cand_short >= src_short {
                    continue;
                }
                let (cw, ch) = scale_keeping_aspect_by_short_side(src_w, src_h, cand_short);
                if cw == 0 || ch == 0 {
                    continue;
                }
                let bpp = (video_bitrate_bps as f64) / ((cw as f64) * (ch as f64) * fps);
                chosen = (cw, ch);
                if bpp >= min_bpp {
                    matched = true;
                    break;
                }
            }
            if !matched && src_short > 360 {
                chosen = scale_keeping_aspect_by_short_side(src_w, src_h, 360);
            }
            out_w = chosen.0;
            out_h = chosen.1;
        }
    }

    let downscaled = probe.width > 0 && probe.height > 0 && (out_w != src_w || out_h != src_h);
    let reduction_percent = if probe.file_size_bytes > 0 && estimated_bytes < probe.file_size_bytes {
        (((probe.file_size_bytes - estimated_bytes) as f64 / probe.file_size_bytes as f64) * 100.0)
            .round()
            .clamp(0.0, 99.0) as u8
    } else {
        0
    };

    let quality_note = if !two_pass {
        format!(
            "CRF {} {} (duração não informada; qualidade perceptual adaptativa)",
            crf_fallback,
            codec.short_label()
        )
    } else if downscaled {
        format!(
            "2-Pass {} · Resolução ajustada para {}x{} para evitar pixelização neste tamanho",
            codec.short_label(),
            out_w,
            out_h
        )
    } else if ratio >= 0.70 {
        format!(
            "2-Pass {} · Resolução nativa com bitrate otimizado por cena (fidelidade máxima)",
            codec.short_label()
        )
    } else {
        format!(
            "2-Pass {} · Alocação inteligente de bits por cena para máxima nitidez neste tamanho",
            codec.short_label()
        )
    };

    VideoCompressionPlan {
        target_bytes,
        estimated_bytes,
        reduction_percent,
        video_bitrate_bps,
        audio_bitrate_bps,
        copy_audio,
        out_width: out_w,
        out_height: out_h,
        downscaled,
        codec,
        two_pass,
        crf_fallback,
        quality_note,
    }
}

pub fn unique_compressed_output_path(input: &Path) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("video");
    let first = parent.join(format!("{stem}_comprimido.mp4"));
    if !first.exists() {
        return first;
    }
    for i in 2..=999 {
        let cand = parent.join(format!("{stem}_comprimido_{i}.mp4"));
        if !cand.exists() {
            return cand;
        }
    }
    parent.join(format!("{stem}_comprimido_{}.mp4", uuid::Uuid::new_v4()))
}

fn parse_ffmpeg_progress_kv(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once('=')?;
    Some((k.trim(), v.trim()))
}

fn parse_ffmpeg_speed_x(val: &str) -> Option<f32> {
    val.trim().trim_end_matches('x').parse::<f32>().ok()
}

#[allow(clippy::too_many_arguments)]
pub async fn compress_video_file<F>(
    ffmpeg_path: &str,
    job_id: uuid::Uuid,
    input_path: &Path,
    output_path: &Path,
    probe: &VideoProbeInfo,
    plan: &VideoCompressionPlan,
    replace_original: bool,
    cancel: Arc<AtomicBool>,
    on_progress: F,
) -> Result<(PathBuf, u64)>
where
    F: Fn(CompressionJobStatus, f32, Option<f32>, Option<u64>) + Send + Sync + Clone + 'static,
{
    if !input_path.exists() {
        return Err(anyhow!(
            "Arquivo de origem não encontrado: {}",
            input_path.display()
        ));
    }

    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let work_dir = std::env::temp_dir().join(format!("vortex_compress_{job_id}"));
    let _ = tokio::fs::remove_dir_all(&work_dir).await;
    tokio::fs::create_dir_all(&work_dir).await?;

    let tmp_output = output_path.with_extension("vortex_tmp.mp4");
    let _ = tokio::fs::remove_file(&tmp_output).await;

    let run_result = run_ffmpeg_compression_passes(
        ffmpeg_path,
        &work_dir,
        input_path,
        &tmp_output,
        probe,
        plan,
        &cancel,
        &on_progress,
    )
    .await;

    let _ = tokio::fs::remove_dir_all(&work_dir).await;

    if let Err(err) = run_result {
        let _ = tokio::fs::remove_file(&tmp_output).await;
        return Err(err);
    }

    let final_target = if replace_original {
        let backup = input_path.with_extension("vortex_orig.bak");
        let _ = tokio::fs::remove_file(&backup).await;
        if input_path != tmp_output {
            tokio::fs::rename(input_path, &backup).await?;
            match tokio::fs::rename(&tmp_output, output_path).await {
                Ok(()) => {
                    let _ = tokio::fs::remove_file(&backup).await;
                }
                Err(e) => {
                    let _ = tokio::fs::rename(&backup, input_path).await;
                    return Err(e.into());
                }
            }
        }
        output_path.to_path_buf()
    } else {
        if output_path.exists() {
            let _ = tokio::fs::remove_file(output_path).await;
        }
        tokio::fs::rename(&tmp_output, output_path).await?;
        output_path.to_path_buf()
    };

    let final_size = tokio::fs::metadata(&final_target).await?.len();
    on_progress(CompressionJobStatus::Completed, 1.0, None, Some(0));
    Ok((final_target, final_size))
}

#[allow(clippy::too_many_arguments)]
async fn run_ffmpeg_compression_passes<F>(
    ffmpeg_path: &str,
    work_dir: &Path,
    input_path: &Path,
    tmp_output: &Path,
    probe: &VideoProbeInfo,
    plan: &VideoCompressionPlan,
    cancel: &Arc<AtomicBool>,
    on_progress: &F,
) -> Result<()>
where
    F: Fn(CompressionJobStatus, f32, Option<f32>, Option<u64>) + Send + Sync + Clone + 'static,
{
    let video_k = (plan.video_bitrate_bps / 1000).max(40);
    let maxrate_k = ((video_k as f64) * 1.45).round() as u64;
    let bufsize_k = ((video_k as f64) * 2.5).round() as u64;
    let vf_arg = if plan.downscaled && plan.out_width > 0 && plan.out_height > 0 {
        Some(format!("scale={}:{}:flags=lanczos", plan.out_width, plan.out_height))
    } else {
        None
    };

    if plan.two_pass {
        on_progress(CompressionJobStatus::Pass1Analyzing, 0.01, None, None);
        let mut p1_args: Vec<String> = vec![
            "-y".into(),
            "-i".into(),
            input_path.to_string_lossy().to_string(),
        ];
        if let Some(ref vf) = vf_arg {
            p1_args.push("-vf".into());
            p1_args.push(vf.clone());
        }
        match plan.codec {
            VideoCompressionCodec::Hevc => {
                p1_args.extend([
                    "-c:v".into(),
                    "libx265".into(),
                    "-preset".into(),
                    "medium".into(),
                    "-b:v".into(),
                    format!("{video_k}k"),
                    "-maxrate".into(),
                    format!("{maxrate_k}k"),
                    "-bufsize".into(),
                    format!("{bufsize_k}k"),
                    "-x265-params".into(),
                    "pass=1:stats=x265_2pass.log:aq-mode=3:psy-rd=2.0:no-info=1".into(),
                ]);
            }
            VideoCompressionCodec::H264 => {
                p1_args.extend([
                    "-c:v".into(),
                    "libx264".into(),
                    "-preset".into(),
                    "slow".into(),
                    "-b:v".into(),
                    format!("{video_k}k"),
                    "-maxrate".into(),
                    format!("{maxrate_k}k"),
                    "-bufsize".into(),
                    format!("{bufsize_k}k"),
                    "-aq-mode".into(),
                    "3".into(),
                    "-pass".into(),
                    "1".into(),
                    "-passlogfile".into(),
                    "x264_2pass".into(),
                ]);
            }
        }
        let null_sink = if cfg!(windows) { "NUL" } else { "/dev/null" };
        p1_args.extend([
            "-an".into(),
            "-f".into(),
            "null".into(),
            "-progress".into(),
            "pipe:1".into(),
            "-nostats".into(),
            null_sink.into(),
        ]);

        run_ffmpeg_single_invocation(
            ffmpeg_path,
            work_dir,
            &p1_args,
            probe.duration_secs,
            0.0,
            0.35,
            CompressionJobStatus::Pass1Analyzing,
            cancel,
            on_progress,
        )
        .await?;

        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("Compressão cancelada pelo usuário."));
        }

        on_progress(CompressionJobStatus::Pass2Encoding, 0.35, None, None);
        let mut p2_args: Vec<String> = vec![
            "-y".into(),
            "-i".into(),
            input_path.to_string_lossy().to_string(),
        ];
        if let Some(ref vf) = vf_arg {
            p2_args.push("-vf".into());
            p2_args.push(vf.clone());
        }
        match plan.codec {
            VideoCompressionCodec::Hevc => {
                p2_args.extend([
                    "-c:v".into(),
                    "libx265".into(),
                    "-preset".into(),
                    "medium".into(),
                    "-b:v".into(),
                    format!("{video_k}k"),
                    "-maxrate".into(),
                    format!("{maxrate_k}k"),
                    "-bufsize".into(),
                    format!("{bufsize_k}k"),
                    "-x265-params".into(),
                    "pass=2:stats=x265_2pass.log:aq-mode=3:psy-rd=2.0:no-info=1".into(),
                    "-tag:v".into(),
                    "hvc1".into(),
                ]);
            }
            VideoCompressionCodec::H264 => {
                p2_args.extend([
                    "-c:v".into(),
                    "libx264".into(),
                    "-preset".into(),
                    "slow".into(),
                    "-b:v".into(),
                    format!("{video_k}k"),
                    "-maxrate".into(),
                    format!("{maxrate_k}k"),
                    "-bufsize".into(),
                    format!("{bufsize_k}k"),
                    "-aq-mode".into(),
                    "3".into(),
                    "-pass".into(),
                    "2".into(),
                    "-passlogfile".into(),
                    "x264_2pass".into(),
                ]);
            }
        }
        append_audio_and_mux_args(&mut p2_args, probe, plan, tmp_output);

        run_ffmpeg_single_invocation(
            ffmpeg_path,
            work_dir,
            &p2_args,
            probe.duration_secs,
            0.35,
            1.0,
            CompressionJobStatus::Pass2Encoding,
            cancel,
            on_progress,
        )
        .await
    } else {
        on_progress(CompressionJobStatus::Encoding, 0.02, None, None);
        let mut args: Vec<String> = vec![
            "-y".into(),
            "-i".into(),
            input_path.to_string_lossy().to_string(),
        ];
        if let Some(ref vf) = vf_arg {
            args.push("-vf".into());
            args.push(vf.clone());
        }
        match plan.codec {
            VideoCompressionCodec::Hevc => {
                args.extend([
                    "-c:v".into(),
                    "libx265".into(),
                    "-preset".into(),
                    "medium".into(),
                    "-crf".into(),
                    plan.crf_fallback.to_string(),
                    "-x265-params".into(),
                    "aq-mode=3:psy-rd=2.0:no-info=1".into(),
                    "-tag:v".into(),
                    "hvc1".into(),
                ]);
            }
            VideoCompressionCodec::H264 => {
                args.extend([
                    "-c:v".into(),
                    "libx264".into(),
                    "-preset".into(),
                    "slow".into(),
                    "-crf".into(),
                    plan.crf_fallback.to_string(),
                    "-aq-mode".into(),
                    "3".into(),
                ]);
            }
        }
        append_audio_and_mux_args(&mut args, probe, plan, tmp_output);

        run_ffmpeg_single_invocation(
            ffmpeg_path,
            work_dir,
            &args,
            probe.duration_secs,
            0.0,
            1.0,
            CompressionJobStatus::Encoding,
            cancel,
            on_progress,
        )
        .await
    }
}

fn append_audio_and_mux_args(
    args: &mut Vec<String>,
    probe: &VideoProbeInfo,
    plan: &VideoCompressionPlan,
    tmp_output: &Path,
) {
    args.extend(["-pix_fmt".into(), "yuv420p".into()]);
    if !probe.has_audio {
        args.push("-an".into());
    } else if plan.copy_audio {
        args.extend(["-c:a".into(), "copy".into()]);
    } else {
        let ak = (plan.audio_bitrate_bps / 1000).clamp(32, 320);
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-ac".into(),
            "2".into(),
            "-b:a".into(),
            format!("{ak}k"),
        ]);
    }
    args.extend([
        "-map_metadata".into(),
        "0".into(),
        "-movflags".into(),
        "+faststart".into(),
        "-f".into(),
        "mp4".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
        tmp_output.to_string_lossy().to_string(),
    ]);
}

#[allow(clippy::too_many_arguments)]
async fn run_ffmpeg_single_invocation<F>(
    ffmpeg_path: &str,
    work_dir: &Path,
    args: &[String],
    duration_secs: f64,
    frac_start: f32,
    frac_end: f32,
    status: CompressionJobStatus,
    cancel: &Arc<AtomicBool>,
    on_progress: &F,
) -> Result<()>
where
    F: Fn(CompressionJobStatus, f32, Option<f32>, Option<u64>) + Send + Sync,
{
    let mut cmd = Command::new(ffmpeg_path);
    crate::extractor::configure_silent_command(&mut cmd);
    cmd.current_dir(work_dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW

    let mut child = cmd.spawn().context("Falha ao iniciar FFmpeg")?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("Sem stdout do FFmpeg"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("Sem stderr do FFmpeg"))?;

    let stderr_tail = Arc::new(parking_lot::Mutex::new(String::new()));
    let stderr_clone = stderr_tail.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let mut g = stderr_clone.lock();
            if g.len() > 6144 {
                let keep_from = g.len().saturating_sub(3072);
                let tail = g[keep_from..].to_string();
                *g = tail;
            }
            g.push_str(&line);
            g.push('\n');
        }
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut cur_speed: Option<f32> = None;
    let mut cur_out_secs: f64 = 0.0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill().await;
            return Err(anyhow!("Compressão cancelada pelo usuário."));
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.kill().await;
                    return Err(anyhow!("Compressão cancelada pelo usuário."));
                }
            }
            res = lines.next_line() => {
                match res {
                    Ok(Some(line)) => {
                        if let Some((k, v)) = parse_ffmpeg_progress_kv(&line) {
                            match k {
                                "out_time_us" | "out_time_ms" => {
                                    if let Ok(us) = v.parse::<f64>() {
                                        if us >= 0.0 {
                                            cur_out_secs = us / 1_000_000.0;
                                        }
                                    }
                                }
                                "speed" => {
                                    cur_speed = parse_ffmpeg_speed_x(v);
                                }
                                "progress" => {
                                    let local_frac = if duration_secs > 0.1 {
                                        (cur_out_secs / duration_secs).clamp(0.0, 1.0) as f32
                                    } else {
                                        0.5
                                    };
                                    let total_frac = frac_start + local_frac * (frac_end - frac_start);
                                    let eta_secs = match cur_speed {
                                        Some(spd) if spd > 0.05 && duration_secs > 0.1 => {
                                            let rem_media = (duration_secs - cur_out_secs).max(0.0);
                                            let rem_pass = rem_media / (spd as f64);
                                            let scale_remaining = if frac_end < 0.99 {
                                                rem_pass + (duration_secs / (spd as f64))
                                            } else {
                                                rem_pass
                                            };
                                            Some(scale_remaining.ceil() as u64)
                                        }
                                        _ => None,
                                    };
                                    on_progress(status.clone(), total_frac, cur_speed, eta_secs);
                                }
                                _ => {}
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Err(anyhow!("Erro ao ler progresso do FFmpeg: {e}")),
                }
            }
        }
    }

    let exit = child.wait().await?;
    if cancel.load(Ordering::Relaxed) {
        return Err(anyhow!("Compressão cancelada pelo usuário."));
    }
    if !exit.success() {
        let err_txt = stderr_tail.lock().clone();
        let last_lines: Vec<&str> = err_txt
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(4)
            .collect();
        let summary = last_lines.into_iter().rev().collect::<Vec<_>>().join(" | ");
        return Err(anyhow!("FFmpeg falhou: {summary}"));
    }

    on_progress(status, frac_end, cur_speed, None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ytdlp_progress_line() {
        let line = "[download]  42.5% of ~  120.00MiB at    5.50MiB/s ETA 00:12";
        let (pct, total, speed, eta) = parse_ytdlp_progress(line).expect("should parse");
        assert!((pct - 42.5).abs() < 0.01);
        assert!(total.is_some());
        assert!(speed > 5_000_000);
        assert_eq!(eta, Some(12));

        let unknown_spd = "[download]  88.5% of   11.04MiB at  Unknown B/s ETA Unknown";
        let (pct2, tot2, spd2, eta2) = parse_ytdlp_progress(unknown_spd).expect("should parse unknown speed");
        assert!((pct2 - 88.5).abs() < 0.01);
        assert!(tot2.unwrap() > 11_000_000);
        assert_eq!(spd2, 0);
        assert_eq!(eta2, None);

        let finished = "[download] 100% of   11.04MiB in 00:00:00 at 26.09MiB/s";
        let (pct3, tot3, spd3, eta3) = parse_ytdlp_progress(finished).expect("should parse finished");
        assert!((pct3 - 100.0).abs() < 0.01);
        assert!(tot3.unwrap() > 11_000_000);
        assert!(spd3 > 25_000_000);
        assert_eq!(eta3, None);
    }

    #[test]
    fn resolves_hls_master_and_segments() {
        let base = url::Url::parse("https://cdn.example.com/hls/master.m3u8").unwrap();
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=4500000\nhigh/index.m3u8\n";
        let best = select_best_hls_variant(&base, master).unwrap();
        assert_eq!(best.as_str(), "https://cdn.example.com/hls/high/index.m3u8");

        let media = "#EXTM3U\n#EXTINF:4.0,\nseg001.ts\n#EXTINF:4.0,\nseg002.ts\n";
        let segs = parse_hls_segments(&best, media);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].as_str(), "https://cdn.example.com/hls/high/seg001.ts");
    }

    #[test]
    fn detects_youtube_urls_and_cookie_headers() {
        assert!(is_youtube_url("https://www.youtube.com/watch?v=jcPpVsCwvJo&list=RDjcPpVsCwvJo"));
        assert!(is_youtube_url("https://youtu.be/jcPpVsCwvJo"));
        assert!(is_youtube_url("https://music.youtube.com/watch?v=abc"));
        assert!(!is_youtube_url("https://vimeo.com/123456"));
        assert!(!is_youtube_url("https://notyoutube.com/watch?v=123"));

        let mut hdrs = HashMap::new();
        hdrs.insert("Referer".to_string(), "https://www.youtube.com/".to_string());
        assert!(!has_cookie_header(&hdrs));
        hdrs.insert("Cookie".to_string(), "SID=abc".to_string());
        assert!(has_cookie_header(&hdrs));
    }

    #[test]
    fn detects_video_platform_and_stream_urls() {
        assert_eq!(
            detect_media_url("https://youtu.be/R7LeRXiYh68?si=6jkrk1582m_PXrRr"),
            Some(MediaUrlMatch {
                is_hls: false,
                is_dash: false
            })
        );
        assert_eq!(
            detect_media_url("https://x.com/user/status/1234567890"),
            Some(MediaUrlMatch {
                is_hls: false,
                is_dash: false
            })
        );
        assert_eq!(
            detect_media_url("https://www.tiktok.com/@creator/video/987654321"),
            Some(MediaUrlMatch {
                is_hls: false,
                is_dash: false
            })
        );
        assert_eq!(
            detect_media_url("https://cdn.example.com/live/master.m3u8?token=1"),
            Some(MediaUrlMatch {
                is_hls: true,
                is_dash: false
            })
        );
        assert_eq!(
            detect_media_url("https://cdn.example.com/file.mp4"),
            None
        );
        assert_eq!(
            detect_media_url("https://github.com/org/repo/archive/main.zip"),
            None
        );
    }

    #[test]
    fn compression_plan_preserves_native_resolution_at_high_ratio() {
        let probe = VideoProbeInfo {
            path: PathBuf::from("clip.mp4"),
            filename: "clip.mp4".into(),
            file_size_bytes: 100 * 1024 * 1024, // 100 MiB
            duration_secs: 60.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            video_codec: "h264".into(),
            total_bitrate_bps: 13_981_013,
            has_audio: true,
            audio_codec: Some("aac".into()),
            audio_bitrate_bps: Some(128_000),
        };

        let plan = plan_video_compression(&probe, 0.75, VideoCompressionCodec::Hevc, true);
        assert!(plan.two_pass);
        assert!(!plan.downscaled);
        assert_eq!(plan.out_width, 1920);
        assert_eq!(plan.out_height, 1080);
        assert!(plan.estimated_bytes <= 78 * 1024 * 1024);
        assert!(plan.estimated_bytes >= 72 * 1024 * 1024);
        assert!(plan.copy_audio);
        assert!(plan.reduction_percent >= 20 && plan.reduction_percent <= 30);
    }

    #[test]
    fn compression_plan_downscales_portrait_and_landscape_gracefully() {
        let portrait = VideoProbeInfo {
            path: PathBuf::from("reel.mp4"),
            filename: "reel.mp4".into(),
            file_size_bytes: 120 * 1024 * 1024,
            duration_secs: 300.0, // 5 min -> ~3.35 Mbps source
            width: 1080,
            height: 1920,
            fps: 60.0,
            video_codec: "h264".into(),
            total_bitrate_bps: 3_355_000,
            has_audio: true,
            audio_codec: Some("aac".into()),
            audio_bitrate_bps: Some(192_000),
        };

        // 15% of 120 MiB over 300s at 60fps: ~500 kbps -> too low for 1080x1920@60fps, should downscale keeping 9:16
        let aggressive = plan_video_compression(&portrait, 0.15, VideoCompressionCodec::H264, true);
        assert!(aggressive.downscaled);
        assert!(aggressive.out_width < 1080);
        assert!(aggressive.out_height < 1920);
        assert!(aggressive.out_height > aggressive.out_width);
        assert_eq!(aggressive.out_width % 2, 0);
        assert_eq!(aggressive.out_height % 2, 0);

        // When auto_downscale is disabled, keeps 1080x1920
        let keep_res = plan_video_compression(&portrait, 0.15, VideoCompressionCodec::H264, false);
        assert!(!keep_res.downscaled);
        assert_eq!(keep_res.out_width, 1080);
        assert_eq!(keep_res.out_height, 1920);
    }

    #[test]
    fn unique_compressed_output_path_handles_collisions() {
        let dir = std::env::temp_dir().join(format!("vortex_test_out_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("demo.mkv");
        std::fs::write(&src, b"x").unwrap();

        let p1 = unique_compressed_output_path(&src);
        assert_eq!(p1, dir.join("demo_comprimido.mp4"));
        std::fs::write(&p1, b"y").unwrap();

        let p2 = unique_compressed_output_path(&src);
        assert_eq!(p2, dir.join("demo_comprimido_2.mp4"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_ffmpeg_progress_and_speed() {
        assert_eq!(
            parse_ffmpeg_progress_kv("out_time_us=12500000"),
            Some(("out_time_us", "12500000"))
        );
        assert_eq!(parse_ffmpeg_speed_x(" 2.45x "), Some(2.45));
        assert_eq!(parse_ffmpeg_speed_x("N/A"), None);
    }

    #[test]
    fn parses_ytdlp_output_paths_and_resolves_unicode_disk_file() {
        assert_eq!(
            parse_ytdlp_already_downloaded(
                "[download] D:\\Videos\\Demo [PNcRsZYsNJc].mp4 has already been downloaded"
            ),
            Some(PathBuf::from("D:\\Videos\\Demo [PNcRsZYsNJc].mp4"))
        );
        assert_eq!(
            parse_ytdlp_merger_path(
                "[Merger] Merging formats into \"D:\\Videos\\Demo [PNcRsZYsNJc].mp4\""
            ),
            Some(PathBuf::from("D:\\Videos\\Demo [PNcRsZYsNJc].mp4"))
        );
        assert_eq!(
            parse_ytdlp_extract_audio_path(
                "[ExtractAudio] Destination: D:\\Videos\\Demo [PNcRsZYsNJc].mp3"
            ),
            Some(PathBuf::from("D:\\Videos\\Demo [PNcRsZYsNJc].mp3"))
        );

        let dir = std::env::temp_dir().join(format!("vortex_yt_resolve_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let intermediate = dir.join("Madeline Goldstein\u{ff1a} My Own Design [PNcRsZYsNJc].f401.mp4");
        let actual = dir.join("Madeline Goldstein\u{ff1a} My Own Design [PNcRsZYsNJc].mp4");
        std::fs::write(&intermediate, b"partial").unwrap();
        std::fs::write(&actual, b"merged-video").unwrap();

        // Simulate cp1252-mangled stdout path missing U+FF1A
        let mangled = dir.join("Madeline Goldstein My Own Design [PNcRsZYsNJc].mp4");
        let resolved = resolve_ytdlp_final_path(
            Some(&mangled),
            None,
            None,
            &dir,
            "https://www.youtube.com/watch?v=PNcRsZYsNJc",
        );
        assert_eq!(resolved.as_deref(), Some(actual.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compression_plan_prioritizes_audio_quality_and_copies_aac_when_fitting() {
        let music_video = VideoProbeInfo {
            path: PathBuf::from("music.mp4"),
            filename: "music.mp4".into(),
            file_size_bytes: 36 * 1024 * 1024,
            duration_secs: 270.0,
            width: 1280,
            height: 720,
            fps: 24.0,
            video_codec: "h264".into(),
            total_bitrate_bps: 1_100_000,
            has_audio: true,
            audio_codec: Some("aac".into()),
            audio_bitrate_bps: Some(128_000),
        };

        // At 40% (-60%) on a 36 MB music video, original 128 kbps AAC should be stream-copied untouched
        let copy_plan = plan_video_compression(&music_video, 0.40, VideoCompressionCodec::Hevc, true);
        assert!(copy_plan.copy_audio);
        assert_eq!(copy_plan.audio_bitrate_bps, 128_000);

        // Even when source audio is Opus (must transcode to AAC), audio stays >= 128 kbps
        let mut opus_video = music_video;
        opus_video.audio_codec = Some("opus".into());
        opus_video.audio_bitrate_bps = Some(160_000);
        let transcode_plan =
            plan_video_compression(&opus_video, 0.40, VideoCompressionCodec::Hevc, true);
        assert!(!transcode_plan.copy_audio);
        assert!(transcode_plan.audio_bitrate_bps >= 128_000);
    }

    #[test]
    fn find_binary_with_dirs_checks_bundled_exe_bin_and_prefers_user_bin() {
        let root = std::env::temp_dir().join(format!("vortex_bin_lookup_{}", uuid::Uuid::new_v4()));
        let user_bin = root.join("user_bin");
        let exe_dir = root.join("app");
        let exe_bin = exe_dir.join("bin");
        std::fs::create_dir_all(&user_bin).unwrap();
        std::fs::create_dir_all(&exe_bin).unwrap();

        let bundled_ff = exe_bin.join("ffmpeg.exe");
        std::fs::write(&bundled_ff, b"bundled").unwrap();
        assert_eq!(
            find_binary_with_dirs(&["ffmpeg.exe"], &user_bin, Some(&exe_dir)).as_deref(),
            Some(bundled_ff.to_string_lossy().as_ref())
        );

        let user_ff = user_bin.join("ffmpeg.exe");
        std::fs::write(&user_ff, b"updated").unwrap();
        assert_eq!(
            find_binary_with_dirs(&["ffmpeg.exe"], &user_bin, Some(&exe_dir)).as_deref(),
            Some(user_ff.to_string_lossy().as_ref())
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
