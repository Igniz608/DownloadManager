use crate::extractor::find_in_path;
use crate::http_engine::{DownloadCancelHandle, apply_custom_headers};
use crate::model::{
    AppSettings, DownloadItem, DownloadStatus, FileCategory, MediaFormatOption,
    MediaPlaylistEntry, MediaProbeResult,
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
    let local_bin = bin_dir();
    for name in names {
        let p = local_bin.join(name);
        if p.exists() {
            return Some(p.to_string_lossy().to_string());
        }
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

pub async fn install_yt_dlp(client: &Client) -> Result<PathBuf> {
    let dir = bin_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let dest = dir.join("yt-dlp.exe");
    let url = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
    let bytes = client.get(url).send().await?.error_for_status()?.bytes().await?;
    let tmp = dir.join("yt-dlp.exe.tmp");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, &dest).await?;
    Ok(dest)
}

pub async fn install_ffmpeg_essentials(client: &Client) -> Result<PathBuf> {
    let dir = bin_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let url = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
    let bytes = client.get(url).send().await?.error_for_status()?.bytes().await?;
    let zip_path = dir.join("ffmpeg-essentials.zip");
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
    if let Some(src) = found_ffmpeg {
        tokio::fs::copy(&src, &ffmpeg_dest).await?;
    } else {
        return Err(anyhow!("ffmpeg.exe não encontrado dentro do pacote baixado"));
    }
    if let Some(src) = found_ffprobe {
        let _ = tokio::fs::copy(&src, dir.join("ffprobe.exe")).await;
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
) -> Result<std::process::Output> {
    let mut cmd = Command::new(yt_dlp_path);
    crate::extractor::configure_silent_command(&mut cmd);
    cmd.args(["-J", "--no-warnings", "--flat-playlist", "--playlist-end", "100"]);
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
    let include_cookies = !is_youtube_url(url);
    let mut output = run_yt_dlp_probe(yt_dlp_path, url, headers, include_cookies).await?;
    if !output.status.success() && include_cookies && has_cookie_header(headers) {
        output = run_yt_dlp_probe(yt_dlp_path, url, headers, false).await?;
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
    mut item: DownloadItem,
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
    tokio::fs::create_dir_all(&item.save_dir).await?;
    let is_hls = item
        .media_info
        .as_ref()
        .map(|m| m.is_hls)
        .unwrap_or_else(|| item.url.to_lowercase().contains(".m3u8"));

    if is_hls && yt_dlp_path.is_none() {
        return run_native_hls_download(client, item, ffmpeg_path, cancel, on_progress).await;
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
            return run_native_hls_download(client, item, ffmpeg_path, cancel, on_progress).await;
        }
        return Err(res.unwrap_err());
    }

    if is_hls {
        return run_native_hls_download(client, item, ffmpeg_path, cancel, on_progress).await;
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
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let mut g = stderr_clone.lock();
            if g.len() < 4096 {
                g.push_str(&line);
                g.push('\n');
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

    let mut lines = BufReader::new(stdout).lines();
    let mut final_path: Option<PathBuf> = None;
    let mut completed_streams_bytes: u64 = 0;
    let mut current_stream_total: Option<u64> = None;

    loop {
        if cancel.is_stopped() {
            let _ = child.kill().await;
            break;
        }
        let line_opt = tokio::select! {
            res = lines.next_line() => res?,
            _ = cancel.notify.notified() => {
                let _ = child.kill().await;
                break;
            }
        };
        let Some(line) = line_opt else {
            break;
        };

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

    if let Some(pb) = final_path {
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
    let ts_path = item.save_dir.join(format!("{base_name}.ts"));
    let mp4_path = item.save_dir.join(format!("{base_name}.mp4"));

    on_progress(
        Some(DownloadStatus::Downloading),
        Some(format!("{base_name}.mp4")),
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
}
