#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod format;
mod theme;

use app::{NavTab, TrayControls, VortexApp};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use tray_icon::menu::{Menu, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use vortex_core::ipc_server::spawn_ipc_server;
use vortex_core::manager::{DownloadManager, is_torrent_request};
use vortex_core::model::NewDownloadRequest;
use vortex_core::state::load_settings;

fn build_tray_icon() -> Option<TrayControls> {
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let dist = (dx * dx + dy * dy).sqrt();
            let idx = ((y * size + x) * 4) as usize;
            if dist <= 14.5 {
                // Violet-to-cyan download arrow emblem
                let in_arrow = (dx.abs() < 3.0 && y >= 7 && y <= 20)
                    || (y >= 16 && y <= 23 && dx.abs() <= (24 - y) as f32);
                if in_arrow {
                    rgba[idx] = 255;
                    rgba[idx + 1] = 255;
                    rgba[idx + 2] = 255;
                    rgba[idx + 3] = 255;
                } else {
                    rgba[idx] = 124;
                    rgba[idx + 1] = 92;
                    rgba[idx + 2] = 255;
                    rgba[idx + 3] = 255;
                }
            }
        }
    }

    let icon = Icon::from_rgba(rgba, size, size).ok()?;
    let menu = Menu::new();
    let show_item = MenuItem::new("Abrir VortexDL", true, None);
    let pause_item = MenuItem::new("Pausar Tudo", true, None);
    let resume_item = MenuItem::new("Retomar Tudo", true, None);
    let quit_item = MenuItem::new("Sair", true, None);

    let show_id = show_item.id().clone();
    let pause_all_id = pause_item.id().clone();
    let resume_all_id = resume_item.id().clone();
    let quit_id = quit_item.id().clone();

    let _ = menu.append_items(&[&show_item, &pause_item, &resume_item, &quit_item]);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("VortexDL — Download Manager")
        .with_icon(icon)
        .build()
        .ok()?;

    Some(TrayControls {
        _tray: tray,
        show_id,
        pause_all_id,
        resume_all_id,
        quit_id,
    })
}

fn forward_arg_to_running_instance(port: u16, arg: &str) -> bool {
    let addr: std::net::SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));

    let (path, body) = if is_torrent_request(arg, None) {
        (
            "/api/torrent",
            serde_json::json!({ "source": arg }).to_string(),
        )
    } else if arg.starts_with("http://") || arg.starts_with("https://") {
        (
            "/api/downloads",
            serde_json::json!({ "url": arg, "start_immediately": true }).to_string(),
        )
    } else {
        return false;
    };

    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(req.as_bytes()).is_err() || stream.write_all(body.as_bytes()).is_err() {
        return false;
    }
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    resp.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code))
}

fn main() -> eframe::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let cli_args: Vec<String> = std::env::args()
        .skip(1)
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect();

    if !cli_args.is_empty() {
        let port = load_settings().ipc_port;
        let forwarded_any = cli_args
            .iter()
            .fold(false, |acc, arg| forward_arg_to_running_instance(port, arg) || acc);
        if forwarded_any {
            return Ok(());
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime");

    let manager: Arc<DownloadManager> = {
        let _guard = runtime.enter();
        let mgr = DownloadManager::new(runtime.handle().clone());
        let port = mgr.settings.read().ipc_port;
        spawn_ipc_server(mgr.clone(), port);
        mgr
    };

    let mut opened_torrent = false;
    for arg in &cli_args {
        if is_torrent_request(arg, None) {
            let dir = manager.default_torrent_dir();
            let _ = manager.add_torrent_async(arg, dir);
            opened_torrent = true;
        } else if arg.starts_with("http://") || arg.starts_with("https://") {
            let _ = manager.add_http_download(NewDownloadRequest {
                url: arg.clone(),
                filename: None,
                save_dir: None,
                connections: None,
                headers: HashMap::new(),
                checksum: None,
                auto_extract: None,
                extractor_override: None,
                extract_password: None,
                start_immediately: Some(true),
            });
        }
    }

    let tray = build_tray_icon();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 760.0])
            .with_min_inner_size([860.0, 680.0])
            .with_title("VortexDL — Next-Gen Rust Download Manager"),
        ..Default::default()
    };

    let rt_handle = runtime.handle().clone();
    eframe::run_native(
        "VortexDL",
        native_options,
        Box::new(move |cc| {
            let mut app = VortexApp::new(cc, manager, rt_handle, tray);
            if opened_torrent {
                app.set_active_tab(NavTab::Torrents);
            }
            Ok(Box::new(app))
        }),
    )
}
