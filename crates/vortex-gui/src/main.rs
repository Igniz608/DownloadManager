#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod format;
mod theme;

use app::{TrayControls, VortexApp};
use std::sync::Arc;
use tray_icon::menu::{Menu, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use vortex_core::ipc_server::spawn_ipc_server;
use vortex_core::manager::DownloadManager;

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

fn main() -> eframe::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

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
        Box::new(move |cc| Ok(Box::new(VortexApp::new(cc, manager, rt_handle, tray)))),
    )
}
