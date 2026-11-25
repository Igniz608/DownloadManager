use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::{Duration, Instant};

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

fn ensure_gui_running(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    if TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(250)).is_ok() {
        return true;
    }

    if let Ok(cur_exe) = std::env::current_exe() {
        if let Some(dir) = cur_exe.parent() {
            let gui = dir.join("vortex-gui.exe");
            if gui.exists() {
                let _ = Command::new(gui).spawn();
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(5) {
                    if TcpStream::connect_timeout(
                        &addr.parse().unwrap(),
                        Duration::from_millis(250),
                    )
                    .is_ok()
                    {
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
        }
    }
    false
}

fn forward_http(port: u16, method: &str, path: &str, body: Option<&str>) -> NativeResponse {
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
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

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

        ensure_gui_running(req.port);

        let body_str = serde_json::to_string(&req.payload).unwrap_or_else(|_| "{}".into());
        let resp = match req.action.as_str() {
            "ping" => forward_http(req.port, "GET", "/api/ping", None),
            "status" => forward_http(req.port, "GET", "/api/status", None),
            "download" => forward_http(req.port, "POST", "/api/downloads", Some(&body_str)),
            "grab" => forward_http(req.port, "POST", "/api/grab", Some(&body_str)),
            "media" => forward_http(req.port, "POST", "/api/media", Some(&body_str)),
            "torrent" => forward_http(req.port, "POST", "/api/torrent", Some(&body_str)),
            "pause_all" => forward_http(req.port, "POST", "/api/pause-all", Some("{}")),
            "resume_all" => forward_http(req.port, "POST", "/api/resume-all", Some("{}")),
            other => NativeResponse {
                ok: false,
                status: 400,
                data: serde_json::json!({ "error": format!("Unknown action: {other}") }),
            },
        };
        let _ = write_frame(&resp);
    }
}
