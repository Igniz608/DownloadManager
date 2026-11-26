use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[derive(Debug, Deserialize)]
struct NativeRequest {
    action: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    payload: Value,
}

fn default_port() -> u16 {
    17654
}

#[derive(Debug, Serialize)]
struct NativeResponse {
    ok: bool,
    status: u16,
    data: Value,
}

fn read_frame() -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match io::stdin().read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > 16 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid native message length",
        ));
    }
    let mut buf = vec![0u8; len];
    io::stdin().read_exact(&mut buf)?;
    Ok(Some(buf))
}

fn write_frame(val: &NativeResponse) -> io::Result<()> {
    let json = serde_json::to_vec(val).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    let len = (json.len() as u32).to_le_bytes();
    let mut out = io::stdout().lock();
    out.write_all(&len)?;
    out.write_all(&json)?;
    out.flush()
}

#[cfg(windows)]
fn clear_std_handle_inheritance() {
    const HANDLE_FLAG_INHERIT: u32 = 0x00000001;
    const STD_INPUT_HANDLE: u32 = (-10i32) as u32;
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const STD_ERROR_HANDLE: u32 = (-12i32) as u32;

    unsafe extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> isize;
        fn SetHandleInformation(hObject: isize, dwMask: u32, dwFlags: u32) -> i32;
    }

    unsafe {
        for std_id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(std_id);
            if h != 0 && h != -1 {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

fn is_gui_running(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(250)).is_ok()
}

fn ensure_gui_running(port: u16) -> bool {
    if is_gui_running(port) {
        return true;
    }

    let addr = format!("127.0.0.1:{port}");

    if let Ok(cur_exe) = std::env::current_exe() {
        if let Some(dir) = cur_exe.parent() {
            let gui = dir.join("vortex-gui.exe");
            if gui.exists() {
                #[cfg(windows)]
                clear_std_handle_inheritance();

                let spawn_gui = |flags: u32| {
                    let mut cmd = Command::new(&gui);
                    cmd.stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                    #[cfg(windows)]
                    cmd.creation_flags(flags);
                    cmd.spawn()
                };

                #[cfg(windows)]
                {
                    const DETACHED_PROCESS: u32 = 0x00000008;
                    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
                    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;
                    let base_flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
                    if spawn_gui(base_flags | CREATE_BREAKAWAY_FROM_JOB).is_err() {
                        let _ = spawn_gui(base_flags);
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = spawn_gui(0);
                }

                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(8) {
                    if TcpStream::connect_timeout(
                        &addr.parse().unwrap(),
                        Duration::from_millis(250),
                    )
                    .is_ok()
                    {
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    false
}

fn forward_http(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    read_timeout: Duration,
) -> NativeResponse {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = match TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(2))
    {
        Ok(s) => s,
        Err(e) => {
            return NativeResponse {
                ok: false,
                status: 503,
                data: serde_json::json!({ "error": format!("VortexDL offline: {e}") }),
            };
        }
    };
    let _ = stream.set_read_timeout(Some(read_timeout));

    let body_bytes = body.unwrap_or("").as_bytes();
    let req = if method == "POST" {
        format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body_bytes.len()
        )
    } else {
        format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
    };

    if stream.write_all(req.as_bytes()).is_err()
        || (!body_bytes.is_empty() && stream.write_all(body_bytes).is_err())
    {
        return NativeResponse {
            ok: false,
            status: 502,
            data: serde_json::json!({ "error": "Failed to send IPC request" }),
        };
    }

    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    let (head, resp_body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(500);

    let data: Value = serde_json::from_str(resp_body.trim())
        .unwrap_or_else(|_| serde_json::json!({ "raw": resp_body.trim() }));
    NativeResponse {
        ok: (200..300).contains(&status),
        status,
        data,
    }
}

fn find_in_path(names: &[&str]) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_yt_dlp() -> Option<std::path::PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let settings_path = std::path::PathBuf::from(appdata)
            .join("VortexDL")
            .join("settings.json");
        if let Ok(raw) = std::fs::read_to_string(&settings_path) {
            if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                if let Some(p) = val.get("yt_dlp_path").and_then(|v| v.as_str()) {
                    let pb = std::path::PathBuf::from(p);
                    if pb.is_file() {
                        return Some(pb);
                    }
                }
            }
        }
    }

    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let managed = std::path::PathBuf::from(local)
            .join("VortexDL")
            .join("bin")
            .join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
        if managed.is_file() {
            return Some(managed);
        }
    }

    if let Ok(cur_exe) = std::env::current_exe() {
        if let Some(dir) = cur_exe.parent() {
            let sibling = dir.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }

    if cfg!(windows) {
        find_in_path(&["yt-dlp.exe", "yt-dlp"])
    } else {
        find_in_path(&["yt-dlp"])
    }
}

fn detect_js_runtime() -> Option<&'static str> {
    let candidates: &[(&str, &[&str])] = if cfg!(windows) {
        &[
            ("deno", &["deno.exe", "deno"]),
            ("node", &["node.exe", "node"]),
            ("bun", &["bun.exe", "bun"]),
            ("qjs", &["qjs.exe", "qjs"]),
        ]
    } else {
        &[
            ("deno", &["deno"]),
            ("node", &["node"]),
            ("bun", &["bun"]),
            ("qjs", &["qjs"]),
        ]
    };
    for (name, bins) in candidates {
        if find_in_path(bins).is_some() {
            return Some(*name);
        }
    }
    None
}

fn is_youtube_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let without_scheme = lower
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&lower);
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtu.be"
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com")
}

fn run_ytdlp_probe_headless(
    yt_dlp: &std::path::Path,
    url: &str,
    headers: Option<&serde_json::Map<String, Value>>,
    single_video: bool,
    include_cookie: bool,
) -> Result<Value, String> {
    let mut cmd = Command::new(yt_dlp);
    #[cfg(windows)]
    {
        clear_std_handle_inheritance();
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    if let Some(rt) = detect_js_runtime() {
        cmd.args(["--js-runtimes", rt]);
    }
    if single_video {
        cmd.arg("--no-playlist");
    }
    cmd.args(["--dump-single-json", "--no-warnings", "--ignore-errors"]);

    if let Some(hdrs) = headers {
        for (k, v) in hdrs {
            if !include_cookie && k.eq_ignore_ascii_case("cookie") {
                continue;
            }
            if let Some(val) = v.as_str() {
                if !val.trim().is_empty() {
                    cmd.args(["--add-header", &format!("{k}: {val}")]);
                }
            }
        }
    }

    cmd.arg(url);
    cmd.stdin(Stdio::null());

    let out = cmd
        .output()
        .map_err(|e| format!("Falha ao executar yt-dlp: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("yt-dlp saiu com código {}", out.status)
        } else {
            stderr
        });
    }

    let raw = String::from_utf8_lossy(&out.stdout);
    let trimmed = raw
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .unwrap_or_else(|| raw.trim());
    let parsed: Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("JSON inválido do yt-dlp: {e}"))?;

    let title = parsed
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Media")
        .to_string();
    let formats = parsed
        .get("formats")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| {
            let format_id = f.get("format_id")?.as_str()?.to_string();
            let ext = f
                .get("ext")
                .and_then(|v| v.as_str())
                .unwrap_or("mp4")
                .to_string();
            let width = f.get("width").and_then(|v| v.as_u64()).map(|v| v as u32);
            let height = f.get("height").and_then(|v| v.as_u64()).map(|v| v as u32);
            let resolution = f
                .get("resolution")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| height.map(|h| format!("{h}p")));
            let vcodec = f
                .get("vcodec")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let acodec = f
                .get("acodec")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let filesize = f
                .get("filesize")
                .and_then(|v| v.as_u64())
                .or_else(|| f.get("filesize_approx").and_then(|v| v.as_u64()));
            Some(serde_json::json!({
                "format_id": format_id,
                "ext": ext,
                "resolution": resolution,
                "width": width,
                "height": height,
                "vcodec": vcodec,
                "acodec": acodec,
                "filesize": filesize,
            }))
        })
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "title": title,
        "formats": formats,
    }))
}

fn probe_media_headless(payload: &Value) -> NativeResponse {
    let Some(url) = payload.get("url").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) else {
        return NativeResponse {
            ok: false,
            status: 400,
            data: serde_json::json!({ "ok": false, "message": "URL obrigatória para probe" }),
        };
    };

    let Some(yt_dlp) = find_yt_dlp() else {
        return NativeResponse {
            ok: false,
            status: 503,
            data: serde_json::json!({ "ok": false, "message": "yt-dlp não encontrado" }),
        };
    };

    let single_video = payload
        .get("single_video")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let headers = payload.get("headers").and_then(|v| v.as_object());
    let has_cookie = headers
        .map(|h| h.keys().any(|k| k.eq_ignore_ascii_case("cookie")))
        .unwrap_or(false);
    let skip_cookie = is_youtube_url(url);

    let mut res = run_ytdlp_probe_headless(&yt_dlp, url, headers, single_video, !skip_cookie);
    if res.is_err() && !skip_cookie && has_cookie {
        res = run_ytdlp_probe_headless(&yt_dlp, url, headers, single_video, false);
    }

    match res {
        Ok(result) => NativeResponse {
            ok: true,
            status: 200,
            data: serde_json::json!({ "ok": true, "result": result }),
        },
        Err(err) => NativeResponse {
            ok: false,
            status: 400,
            data: serde_json::json!({ "ok": false, "message": err }),
        },
    }
}

fn main() {
    while let Ok(Some(frame)) = read_frame() {
        let req: NativeRequest = match serde_json::from_slice(&frame) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_frame(&NativeResponse {
                    ok: false,
                    status: 400,
                    data: serde_json::json!({ "error": format!("Invalid JSON: {e}") }),
                });
                continue;
            }
        };

        let body_str = serde_json::to_string(&req.payload).unwrap_or_else(|_| "{}".into());
        let resp = match req.action.as_str() {
            "ping" => forward_http(req.port, "GET", "/api/ping", None, Duration::from_secs(5)),
            "status" => forward_http(req.port, "GET", "/api/status", None, Duration::from_secs(5)),
            "media_probe" => {
                if is_gui_running(req.port) {
                    forward_http(
                        req.port,
                        "POST",
                        "/api/media/probe",
                        Some(&body_str),
                        Duration::from_secs(25),
                    )
                } else {
                    probe_media_headless(&req.payload)
                }
            }
            "download" => {
                ensure_gui_running(req.port);
                forward_http(
                    req.port,
                    "POST",
                    "/api/downloads",
                    Some(&body_str),
                    Duration::from_secs(5),
                )
            }
            "grab" => {
                ensure_gui_running(req.port);
                forward_http(
                    req.port,
                    "POST",
                    "/api/grab",
                    Some(&body_str),
                    Duration::from_secs(5),
                )
            }
            "media" => {
                ensure_gui_running(req.port);
                forward_http(
                    req.port,
                    "POST",
                    "/api/media",
                    Some(&body_str),
                    Duration::from_secs(5),
                )
            }
            "torrent" => {
                ensure_gui_running(req.port);
                forward_http(
                    req.port,
                    "POST",
                    "/api/torrent",
                    Some(&body_str),
                    Duration::from_secs(5),
                )
            }
            "pause_all" => {
                ensure_gui_running(req.port);
                forward_http(
                    req.port,
                    "POST",
                    "/api/pause-all",
                    Some("{}"),
                    Duration::from_secs(5),
                )
            }
            "resume_all" => {
                ensure_gui_running(req.port);
                forward_http(
                    req.port,
                    "POST",
                    "/api/resume-all",
                    Some("{}"),
                    Duration::from_secs(5),
                )
            }
            other => NativeResponse {
                ok: false,
                status: 400,
                data: serde_json::json!({ "error": format!("Unknown action: {other}") }),
            },
        };
        let _ = write_frame(&resp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_host_detection_works() {
        assert!(is_youtube_url("https://www.youtube.com/watch?v=jcPpVsCwvJo"));
        assert!(is_youtube_url("https://youtu.be/jcPpVsCwvJo"));
        assert!(!is_youtube_url("https://vimeo.com/12345"));
    }

    #[test]
    fn headless_probe_rejects_missing_url() {
        let resp = probe_media_headless(&serde_json::json!({}));
        assert!(!resp.ok);
        assert_eq!(resp.status, 400);
    }
}
