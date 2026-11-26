use crate::format::{format_bytes, format_duration, format_eta, format_speed};
use crate::theme::{apply_theme, from_c32, to_c32};
use egui::{
    Align, Color32, CornerRadius, Frame, Layout, Margin, RichText, ScrollArea, Sense, Stroke,
    Ui, Vec2,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tray_icon::TrayIcon;
use tray_icon::menu::{MenuEvent, MenuId};
use uuid::Uuid;
use vortex_core::ipc_server::register_browser_native_host;
use vortex_core::manager::{DownloadManager, ManagerSnapshot};
use vortex_core::model::{
    DownloadId, DownloadItem, DownloadKind, DownloadStatus, ExtractorKind, ExtractorProfile,
    FileCategory, MediaProbeResult, NewDownloadRequest, NewMediaRequest, PackageId, ThemeConfig,
};
use vortex_core::package_grabber::extract_urls_from_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavTab {
    All,
    Active,
    Completed,
    Packages,
    Media,
    Torrents,
    LinkGrabber,
    Extractors,
    Extension,
    Themes,
    Settings,
}

pub struct TrayControls {
    pub _tray: TrayIcon,
    pub show_id: MenuId,
    pub pause_all_id: MenuId,
    pub resume_all_id: MenuId,
    pub quit_id: MenuId,
}

#[derive(Debug, Clone)]
enum PasswordEditTarget {
    Download {
        id: DownloadId,
        name: String,
        is_finished: bool,
    },
    Package {
        id: PackageId,
        name: String,
        all_finished: bool,
    },
}

pub struct VortexApp {
    manager: Arc<DownloadManager>,
    runtime: tokio::runtime::Handle,
    tray: Option<TrayControls>,
    active_tab: NavTab,
    search_query: String,
    category_filter: Option<FileCategory>,
    selected_id: Option<Uuid>,
    expanded_packages: HashSet<Uuid>,

    // Clipboard monitor
    last_clipboard: String,
    last_clipboard_check: Instant,
    clipboard_banner_url: Option<String>,

    // Add HTTP Download modal
    show_new_http_modal: bool,
    focus_new_http_url: bool,
    new_http_url: String,
    new_http_filename: String,
    new_http_dir: String,
    new_http_connections: usize,
    new_http_referer: String,
    new_http_cookie: String,
    new_http_checksum: String,
    new_http_auto_extract: bool,
    new_http_extractor_id: String,
    new_http_password: String,

    // Add Torrent modal
    show_new_torrent_modal: bool,
    new_torrent_source: String,
    new_torrent_dir: String,

    // LinkGrabber state
    grabber_input: String,
    grabber_save_dir: String,
    grabber_auto_extract: bool,
    grabber_extractor_id: String,
    grabber_password: String,
    grabber_delete_archives: bool,

    // Media / Video state
    media_url: String,
    media_save_dir: String,
    media_audio_only: bool,
    media_embed_subs: bool,
    media_probing: Arc<parking_lot::Mutex<bool>>,
    media_probe_result: Arc<parking_lot::Mutex<Option<Result<MediaProbeResult, String>>>>,

    // Custom Extractor plugin form
    new_ext_name: String,
    new_ext_exe: String,
    new_ext_args: String,
    new_ext_pw_arg: String,
    new_password_entry: String,

    // Native Messaging registration
    chrome_ext_id_input: String,
    ext_reg_status: Option<String>,

    // Custom Theme save
    custom_theme_name: String,

    // Extraction password prompt & quick password setter
    pw_modal_id: Option<Uuid>,
    pw_modal_input: String,
    pw_modal_save_to_list: bool,
    pw_modal_focus: bool,
    pw_modal_show_chars: bool,
    pw_edit_target: Option<PasswordEditTarget>,
    pw_edit_input: String,
    pw_edit_focus: bool,
    pw_edit_show_chars: bool,

    last_applied_theme_hash: u64,
}

impl VortexApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        manager: Arc<DownloadManager>,
        runtime: tokio::runtime::Handle,
        tray: Option<TrayControls>,
    ) -> Self {
        let settings = manager.settings.read().clone();
        apply_theme(&cc.egui_ctx, &settings.theme);

        Self {
            manager,
            runtime,
            tray,
            active_tab: NavTab::All,
            search_query: String::new(),
            category_filter: None,
            selected_id: None,
            expanded_packages: HashSet::new(),
            last_clipboard: String::new(),
            last_clipboard_check: Instant::now(),
            clipboard_banner_url: None,
            show_new_http_modal: false,
            focus_new_http_url: false,
            new_http_url: String::new(),
            new_http_filename: String::new(),
            new_http_dir: settings.default_download_dir.to_string_lossy().to_string(),
            new_http_connections: settings.default_connections,
            new_http_referer: String::new(),
            new_http_cookie: String::new(),
            new_http_checksum: String::new(),
            new_http_auto_extract: settings.auto_extract_archives,
            new_http_extractor_id: settings.default_extractor_id.clone(),
            new_http_password: String::new(),
            show_new_torrent_modal: false,
            new_torrent_source: String::new(),
            new_torrent_dir: settings
                .default_download_dir
                .join("Torrents")
                .to_string_lossy()
                .to_string(),
            grabber_input: String::new(),
            grabber_save_dir: settings.default_download_dir.to_string_lossy().to_string(),
            grabber_auto_extract: true,
            grabber_extractor_id: settings.default_extractor_id.clone(),
            grabber_password: String::new(),
            grabber_delete_archives: settings.delete_archives_after_extract,
            media_url: String::new(),
            media_save_dir: settings.default_download_dir.to_string_lossy().to_string(),
            media_audio_only: false,
            media_embed_subs: false,
            media_probing: Arc::new(parking_lot::Mutex::new(false)),
            media_probe_result: Arc::new(parking_lot::Mutex::new(None)),
            new_ext_name: String::new(),
            new_ext_exe: String::new(),
            new_ext_args: "x -y -o\"{dest}\" \"{archive}\"".into(),
            new_ext_pw_arg: "-p{password}".into(),
            new_password_entry: String::new(),
            chrome_ext_id_input: String::new(),
            ext_reg_status: None,
            custom_theme_name: "Meu Tema".into(),
            pw_modal_id: None,
            pw_modal_input: String::new(),
            pw_modal_save_to_list: false,
            pw_modal_focus: false,
            pw_modal_show_chars: false,
            pw_edit_target: None,
            pw_edit_input: String::new(),
            pw_edit_focus: false,
            pw_edit_show_chars: false,
            last_applied_theme_hash: 0,
        }
    }

    fn handle_tray_events(&self, ctx: &egui::Context) {
        let Some(ref tray) = self.tray else {
            return;
        };
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == tray.show_id {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            } else if event.id == tray.pause_all_id {
                self.manager.pause_all();
            } else if event.id == tray.resume_all_id {
                self.manager.resume_all();
            } else if event.id == tray.quit_id {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn check_clipboard(&mut self) {
        if !self.manager.settings.read().clipboard_monitor {
            return;
        }
        if self.last_clipboard_check.elapsed() < Duration::from_millis(900) {
            return;
        }
        self.last_clipboard_check = Instant::now();
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if let Ok(text) = cb.get_text() {
                if text != self.last_clipboard {
                    let is_initial = self.last_clipboard.is_empty();
                    self.last_clipboard = text.clone();
                    if !is_initial {
                        let urls = extract_urls_from_text(&text);
                        if urls.len() == 1 && text.trim().len() < 512 {
                            self.clipboard_banner_url = Some(urls[0].clone());
                        } else if urls.len() > 1 {
                            self.clipboard_banner_url = Some(format!("BATCH:{} links", urls.len()));
                        }
                    }
                }
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            let path = file.path();
            let s = path.to_string_lossy().to_string();
            if s.to_lowercase().ends_with(".torrent") {
                let dir = self.manager.settings.read().default_download_dir.join("Torrents");
                let _ = self.manager.add_torrent_async(&s, dir);
                self.active_tab = NavTab::Torrents;
            } else if let Ok(content) = std::fs::read_to_string(path) {
                let urls = extract_urls_from_text(&content);
                if !urls.is_empty() {
                    self.manager.probe_urls_into_grabber(urls, HashMap::new());
                    self.active_tab = NavTab::LinkGrabber;
                }
            }
        }
    }

    fn sync_theme(&mut self, ctx: &egui::Context) {
        let theme = self.manager.settings.read().theme.clone();
        let bytes = serde_json::to_vec(&theme).unwrap_or_default();
        let mut h = 14695981039346656037u64;
        for b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        if h != self.last_applied_theme_hash {
            apply_theme(ctx, &theme);
            self.last_applied_theme_hash = h;
        }
    }
}

impl eframe::App for VortexApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.sync_theme(&ctx);
        self.handle_tray_events(&ctx);
        self.check_clipboard();
        self.handle_dropped_files(&ctx);

        if self.manager.take_focus_grabber_request() {
            self.active_tab = NavTab::LinkGrabber;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            #[cfg(windows)]
            bring_main_window_to_front();
        }

        let snap = self.manager.snapshot();
        let theme = self.manager.settings.read().theme.clone();
        let repaint_ms = if snap.active_count > 0 { 150 } else { 500 };
        ctx.request_repaint_after(Duration::from_millis(repaint_ms));

        self.render_top_bar(ui, &snap, &theme);
        self.render_sidebar(ui, &snap, &theme);

        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(to_c32(theme.bg_primary))
                    .inner_margin(Margin::same(16)),
            )
            .show(ui, |ui| {
                self.render_clipboard_banner(ui, &theme);
                self.render_notices_bar(ui, &snap, &theme);

                match self.active_tab {
                    NavTab::All
                    | NavTab::Active
                    | NavTab::Completed
                    | NavTab::Torrents => {
                        self.render_downloads_view(ui, &snap, &theme);
                    }
                    NavTab::Packages => self.render_packages_view(ui, &snap, &theme),
                    NavTab::Media => self.render_media_view(ui, &snap, &theme),
                    NavTab::LinkGrabber => self.render_link_grabber_view(ui, &snap, &theme),
                    NavTab::Extractors => self.render_extractors_view(ui, &theme),
                    NavTab::Extension => self.render_extension_view(ui, &theme),
                    NavTab::Themes => self.render_themes_view(ui, &theme),
                    NavTab::Settings => self.render_settings_view(ui, &theme),
                }
            });

        self.render_new_http_modal(&ctx, &theme);
        self.render_new_torrent_modal(&ctx, &theme);
        self.render_password_prompt_modal(&ctx, &snap, &theme);
        self.render_set_password_modal(&ctx, &theme);
    }
}

impl VortexApp {
    fn render_top_bar(&mut self, ui: &mut egui::Ui, snap: &ManagerSnapshot, theme: &ThemeConfig) {
        egui::Panel::top("top_bar")
            .frame(
                Frame::new()
                    .fill(to_c32(theme.bg_secondary))
                    .stroke(Stroke::new(1.0, to_c32(theme.border)))
                    .inner_margin(Margin::symmetric(14, 9)),
            )
            .show(ui, |ui| {
                let avail_w = ui.available_width();
                let scale = theme.font_scale.clamp(0.8, 1.4);
                let single_row_needed = 1090.0 * scale;

                if avail_w >= single_row_needed {
                    ui.horizontal(|ui| {
                        self.render_top_bar_left(ui, theme);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            self.render_top_bar_right(ui, snap, theme, true);
                        });
                    });
                } else {
                    ui.vertical(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            self.render_top_bar_left(ui, theme);
                        });
                        ui.horizontal_wrapped(|ui| {
                            self.render_top_bar_right(ui, snap, theme, false);
                        });
                    });
                }
            });
    }

    fn render_top_bar_left(&mut self, ui: &mut egui::Ui, theme: &ThemeConfig) {
        ui.label(
            RichText::new("VORTEX DL")
                .strong()
                .size(16.5)
                .color(to_c32(theme.accent)),
        );
        ui.add_space(8.0);

        if accent_button(ui, "+ Novo Download", theme).clicked() {
            self.show_new_http_modal = true;
            self.focus_new_http_url = true;
        }
        if ui
            .button("LinkGrabber")
            .on_hover_text("LinkGrabber (Multi-Parte)")
            .clicked()
        {
            self.active_tab = NavTab::LinkGrabber;
        }
        if ui
            .button("Mídia")
            .on_hover_text("Vídeos & Mídias")
            .clicked()
        {
            self.active_tab = NavTab::Media;
        }
        if ui
            .button("+ Torrent")
            .on_hover_text("Adicionar Torrent ou Magnet")
            .clicked()
        {
            self.show_new_torrent_modal = true;
        }

        ui.separator();
        if ui
            .button("Pausar")
            .on_hover_text("Pausar todos os downloads")
            .clicked()
        {
            self.manager.pause_all();
        }
        if ui
            .button("Retomar")
            .on_hover_text("Retomar todos os downloads")
            .clicked()
        {
            self.manager.resume_all();
        }
    }

    fn render_top_bar_right(
        &mut self,
        ui: &mut egui::Ui,
        snap: &ManagerSnapshot,
        theme: &ThemeConfig,
        rtl: bool,
    ) {
        let draw_theme_combo = |ui: &mut egui::Ui, manager: &Arc<DownloadManager>| {
            let current_theme_name = theme.name.clone();
            egui::ComboBox::from_id_salt("quick_theme")
                .selected_text(format!("Tema: {current_theme_name}"))
                .width(148.0)
                .show_ui(ui, |ui| {
                    for preset in ThemeConfig::presets() {
                        if ui
                            .selectable_label(preset.preset_id == theme.preset_id, &preset.name)
                            .clicked()
                        {
                            manager.settings.write().theme = preset;
                            let _ = manager.save_current_settings();
                        }
                    }
                    let custom = manager.settings.read().custom_themes.clone();
                    for ct in custom {
                        if ui
                            .selectable_label(ct.name == theme.name, format!("* {}", ct.name))
                            .clicked()
                        {
                            manager.settings.write().theme = ct;
                            let _ = manager.save_current_settings();
                        }
                    }
                });
        };

        let draw_limit_combo = |ui: &mut egui::Ui, manager: &Arc<DownloadManager>| {
            let mut limit_kbps = manager.settings.read().speed_limit_kbps;
            let limit_label = if limit_kbps == 0 {
                "Limite: Ilimitado".to_string()
            } else {
                format!("Limite: {} KB/s", limit_kbps)
            };
            egui::ComboBox::from_id_salt("quick_limit")
                .selected_text(limit_label)
                .width(128.0)
                .show_ui(ui, |ui| {
                    for (lbl, val) in [
                        ("Ilimitado", 0u64),
                        ("512 KB/s", 512),
                        ("2 MB/s", 2048),
                        ("5 MB/s", 5120),
                        ("10 MB/s", 10240),
                        ("25 MB/s", 25600),
                    ] {
                        if ui.selectable_value(&mut limit_kbps, val, lbl).clicked() {
                            manager.settings.write().speed_limit_kbps = limit_kbps;
                            let _ = manager.save_current_settings();
                        }
                    }
                });
        };

        let draw_speed_pill = |ui: &mut egui::Ui| {
            Frame::new()
                .fill(to_c32(theme.bg_card))
                .stroke(Stroke::new(1.0, to_c32(theme.border)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "DL {}",
                                format_speed(snap.total_download_bps)
                            ))
                            .strong()
                            .color(to_c32(theme.success)),
                        );
                        ui.label(
                            RichText::new(format!(
                                "UL {}",
                                format_speed(snap.total_upload_bps)
                            ))
                            .color(to_c32(theme.accent)),
                        );
                    });
                });
        };

        if rtl {
            draw_theme_combo(ui, &self.manager);
            draw_limit_combo(ui, &self.manager);
            draw_speed_pill(ui);
        } else {
            draw_speed_pill(ui);
            draw_limit_combo(ui, &self.manager);
            draw_theme_combo(ui, &self.manager);
        }
    }

    fn render_sidebar(&mut self, ui: &mut egui::Ui, snap: &ManagerSnapshot, theme: &ThemeConfig) {
        let total = snap.downloads.len();
        let active = snap.active_count;
        let completed = snap
            .downloads
            .iter()
            .filter(|d| d.status.is_finished())
            .count();
        let pkg_count = snap.packages.len();
        let media_count = snap
            .downloads
            .iter()
            .filter(|d| d.kind == DownloadKind::Media || d.category == FileCategory::Video)
            .count();
        let torrent_count = snap
            .downloads
            .iter()
            .filter(|d| d.kind == DownloadKind::Torrent)
            .count();
        let grabbed_count = snap.grabbed_links.len();

        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(theme.sidebar_width.clamp(170.0, 320.0))
            .frame(
                Frame::new()
                    .fill(to_c32(theme.bg_secondary))
                    .stroke(Stroke::new(1.0, to_c32(theme.border)))
                    .inner_margin(Margin::same(12)),
            )
            .show(ui, |ui| {
                ui.label(
                    RichText::new("BIBLIOTECA")
                        .small()
                        .strong()
                        .color(to_c32(theme.text_muted)),
                );
                ui.add_space(4.0);

                nav_item(ui, &mut self.active_tab, NavTab::All, "Todos os Downloads", Some(total), theme);
                nav_item(ui, &mut self.active_tab, NavTab::Active, "Ativos", Some(active), theme);
                nav_item(ui, &mut self.active_tab, NavTab::Completed, "Concluídos", Some(completed), theme);
                nav_item(ui, &mut self.active_tab, NavTab::Packages, "Pacotes Multi-Parte", Some(pkg_count), theme);
                nav_item(ui, &mut self.active_tab, NavTab::Media, "Vídeos & Mídias", Some(media_count), theme);
                nav_item(ui, &mut self.active_tab, NavTab::Torrents, "Torrents", Some(torrent_count), theme);
                nav_item(ui, &mut self.active_tab, NavTab::LinkGrabber, "LinkGrabber", Some(grabbed_count), theme);

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(
                    RichText::new("SISTEMA & PLUGINS")
                        .small()
                        .strong()
                        .color(to_c32(theme.text_muted)),
                );
                ui.add_space(4.0);

                nav_item(ui, &mut self.active_tab, NavTab::Extractors, "Extratores & Plugins", None, theme);
                nav_item(ui, &mut self.active_tab, NavTab::Extension, "Extensão Navegador", None, theme);
                nav_item(ui, &mut self.active_tab, NavTab::Themes, "Personalizar Tema", None, theme);
                nav_item(ui, &mut self.active_tab, NavTab::Settings, "Configurações", None, theme);

                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    let ext_count = self
                        .manager
                        .settings
                        .read()
                        .extractors
                        .iter()
                        .filter(|e| e.detected)
                        .count();
                    let yt_ok = snap.media_tools.yt_dlp_path.is_some();
                    let ff_ok = snap.media_tools.ffmpeg_path.is_some();
                    Frame::new()
                        .fill(to_c32(theme.bg_card))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::same(10))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("Motores prontos")
                                    .small()
                                    .strong()
                                    .color(to_c32(theme.text_primary)),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "yt-dlp: {}  |  FFmpeg: {}",
                                    if yt_ok { "OK" } else { "Ausente" },
                                    if ff_ok { "OK" } else { "Ausente" }
                                ))
                                .small()
                                .color(to_c32(theme.text_muted)),
                            );
                            ui.label(
                                RichText::new(format!("Extratores detectados: {ext_count}"))
                                    .small()
                                    .color(to_c32(theme.text_muted)),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "API Local: :{}",
                                    self.manager.settings.read().ipc_port
                                ))
                                .small()
                                .color(to_c32(theme.accent)),
                            );
                        });
                });
            });
    }

    fn render_clipboard_banner(&mut self, ui: &mut Ui, theme: &ThemeConfig) {
        let Some(url_or_batch) = self.clipboard_banner_url.clone() else {
            return;
        };
        Frame::new()
            .fill(to_c32(theme.bg_card))
            .stroke(Stroke::new(1.0, to_c32(theme.accent)))
            .corner_radius(CornerRadius::same(theme.corner_radius as u8))
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Link detectado na área de transferência:")
                            .strong()
                            .color(to_c32(theme.accent)),
                    );
                    let preview = if url_or_batch.len() > 70 {
                        format!("{}...", &url_or_batch[..70])
                    } else {
                        url_or_batch.clone()
                    };
                    ui.label(RichText::new(preview).color(to_c32(theme.text_primary)));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Ignorar").clicked() {
                            self.clipboard_banner_url = None;
                        }
                        if let Some(count_str) = url_or_batch.strip_prefix("BATCH:") {
                            if accent_button(ui, format!("Enviar {count_str} ao LinkGrabber"), theme).clicked() {
                                self.manager.probe_text_into_grabber(self.last_clipboard.clone(), HashMap::new());
                                self.active_tab = NavTab::LinkGrabber;
                                self.clipboard_banner_url = None;
                            }
                        } else {
                            if ui.button("Analisar Mídia").clicked() {
                                self.media_url = url_or_batch.clone();
                                self.active_tab = NavTab::Media;
                                self.clipboard_banner_url = None;
                            }
                            if accent_button(ui, "Baixar Agora", theme).clicked() {
                                let _ = self.manager.add_http_download(NewDownloadRequest {
                                    url: url_or_batch.clone(),
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
                                self.clipboard_banner_url = None;
                            }
                        }
                    });
                });
            });
        ui.add_space(8.0);
    }

    fn render_notices_bar(&mut self, ui: &mut Ui, snap: &ManagerSnapshot, theme: &ThemeConfig) {
        if let Some(latest) = snap.notifications.last() {
            let dot_color = if latest.is_error {
                to_c32(theme.error)
            } else {
                to_c32(theme.accent)
            };
            let text_color = if latest.is_error {
                to_c32(theme.error)
            } else {
                to_c32(theme.text_muted)
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new("•").color(dot_color));
                ui.label(RichText::new(&latest.message).small().color(text_color));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if snap.notifications.len() > 1 && ui.small_button("Limpar").clicked() {
                        self.manager.clear_notices();
                    }
                    if ui
                        .small_button("X")
                        .on_hover_text("Fechar aviso")
                        .clicked()
                    {
                        self.manager.dismiss_notice(latest.id);
                    }
                });
            });
            ui.add_space(4.0);
        }
    }

    fn render_downloads_view(&mut self, ui: &mut Ui, snap: &ManagerSnapshot, theme: &ThemeConfig) {
        if theme.show_speed_graph {
            render_speed_sparkline(ui, snap, theme);
            ui.add_space(10.0);
        }

        // Filter bar
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("Filtrar por nome ou URL...")
                    .desired_width(260.0),
            );
            if ui
                .selectable_label(self.category_filter.is_none(), "Todas")
                .clicked()
            {
                self.category_filter = None;
            }
            for cat in [
                FileCategory::Video,
                FileCategory::Audio,
                FileCategory::Archive,
                FileCategory::Program,
                FileCategory::Document,
                FileCategory::Torrent,
            ] {
                if ui
                    .selectable_label(self.category_filter == Some(cat), cat.label())
                    .clicked()
                {
                    self.category_filter = Some(cat);
                }
            }
        });
        ui.add_space(8.0);

        let q = self.search_query.to_lowercase();
        let items: Vec<&DownloadItem> = snap
            .downloads
            .iter()
            .filter(|d| match self.active_tab {
                NavTab::Active => d.status.is_active() || d.status == DownloadStatus::Queued,
                NavTab::Completed => d.status.is_finished(),
                NavTab::Torrents => d.kind == DownloadKind::Torrent,
                _ => true,
            })
            .filter(|d| self.category_filter.map_or(true, |c| d.category == c))
            .filter(|d| {
                q.is_empty()
                    || d.filename.to_lowercase().contains(&q)
                    || d.url.to_lowercase().contains(&q)
            })
            .collect();

        if items.is_empty() {
            render_empty_state(
                ui,
                "Nenhum download nesta visualização",
                "Clique em '+ Novo Download', arraste um .torrent para a janela ou use o LinkGrabber para pacotes em várias partes.",
                theme,
            );
            return;
        }

        ScrollArea::vertical().show(ui, |ui| {
            for item in items {
                self.render_download_card(ui, item, theme);
                ui.add_space(6.0);
            }
        });
    }

    fn render_download_card(&mut self, ui: &mut Ui, item: &DownloadItem, theme: &ThemeConfig) {
        let is_selected = self.selected_id == Some(item.id);
        let stroke = if is_selected {
            Stroke::new(1.5, to_c32(theme.accent))
        } else {
            Stroke::new(1.0, to_c32(theme.border))
        };

        let (default_ext_id, extractors) = {
            let s = self.manager.settings.read();
            (s.default_extractor_id.clone(), s.extractors.clone())
        };
        let (eff_auto_extract, eff_extractor) = if let Some(pkg_id) = item.package_id {
            self.manager
                .packages
                .read()
                .iter()
                .find(|p| p.id == pkg_id)
                .map(|p| (p.auto_extract, p.extractor_id.clone()))
                .unwrap_or((item.auto_extract, item.extractor_override.clone()))
        } else {
            (item.auto_extract, item.extractor_override.clone())
        };
        let margin_y = if theme.compact_mode { 7 } else { 11 };

        Frame::new()
            .fill(to_c32(theme.bg_card))
            .stroke(stroke)
            .corner_radius(CornerRadius::same(theme.corner_radius as u8))
            .inner_margin(Margin::symmetric(14, margin_y))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Category badge
                    pill(ui, item.category.label(), to_c32(theme.accent), theme);

                    if ui
                        .add(
                            egui::Label::new(
                                RichText::new(&item.filename)
                                    .strong()
                                    .color(to_c32(theme.text_primary)),
                            )
                            .sense(Sense::click()),
                        )
                        .clicked()
                    {
                        self.selected_id = if is_selected { None } else { Some(item.id) };
                    }

                    if let (Some(idx), Some(total)) = (item.part_index, item.part_total) {
                        pill(
                            ui,
                            &format!("Parte {idx}/{total}"),
                            to_c32(theme.warning),
                            theme,
                        );
                    }

                    let status_color = match &item.status {
                        DownloadStatus::Completed | DownloadStatus::Seeding => to_c32(theme.success),
                        DownloadStatus::Failed(_) => to_c32(theme.error),
                        DownloadStatus::Paused => to_c32(theme.warning),
                        _ => to_c32(theme.accent),
                    };
                    pill(ui, item.status.label(), status_color, theme);

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Remover").clicked() {
                            self.manager.remove_download(item.id, false);
                        }
                        if ui.button("Pasta").clicked() {
                            let _ = open::that(&item.save_dir);
                        }
                        if item.status.is_finished() && item.file_path.exists() {
                            if ui.button("Abrir").clicked() {
                                let _ = open::that(&item.file_path);
                            }
                        }
                        let is_archive = (item.category == FileCategory::Archive
                            || vortex_core::extractor::is_extractable_archive(&item.file_path))
                            && (!item.status.is_finished() || item.file_path.exists());
                        if is_archive {
                            let has_pw = item.extract_password.is_some();
                            let pw_btn_label = if has_pw { "Senha (OK)" } else { "Senha" };
                            if ui
                                .button(pw_btn_label)
                                .on_hover_text("Definir ou alterar senha de extração")
                                .clicked()
                            {
                                self.pw_edit_input =
                                    item.extract_password.clone().unwrap_or_default();
                                self.pw_edit_focus = true;
                                self.pw_edit_show_chars = false;
                                self.pw_edit_target = Some(PasswordEditTarget::Download {
                                    id: item.id,
                                    name: item.filename.clone(),
                                    is_finished: item.status.is_finished(),
                                });
                            }

                            let combo_label = if !item.status.is_finished() {
                                if !eff_auto_extract {
                                    "Não extrair".to_string()
                                } else {
                                    extractor_combo_label(
                                        eff_extractor.as_deref(),
                                        &default_ext_id,
                                        &extractors,
                                    )
                                }
                            } else {
                                "Extrair com...".to_string()
                            };

                            let can_interact = item.status != DownloadStatus::Extracting;
                            ui.add_enabled_ui(can_interact, |ui| {
                                egui::ComboBox::from_id_salt(("ext_now", item.id))
                                    .selected_text(combo_label)
                                    .width(128.0)
                                    .show_ui(ui, |ui| {
                                        let auto_selected =
                                            eff_auto_extract && eff_extractor.is_none();
                                        if ui.selectable_label(auto_selected, "Padrão (Auto)").clicked()
                                        {
                                            self.manager.set_extractor_override(item.id, None, true);
                                            if item.status.is_finished() && item.file_path.exists() {
                                                self.manager.trigger_manual_extract(item.id, None);
                                            }
                                        }
                                        for prof in &extractors {
                                            if prof.detected && prof.enabled {
                                                let prof_selected = eff_auto_extract
                                                    && eff_extractor.as_deref()
                                                        == Some(prof.id.as_str());
                                                if ui
                                                    .selectable_label(prof_selected, &prof.name)
                                                    .clicked()
                                                {
                                                    self.manager.set_extractor_override(
                                                        item.id,
                                                        Some(prof.id.clone()),
                                                        true,
                                                    );
                                                    if item.status.is_finished()
                                                        && item.file_path.exists()
                                                    {
                                                        self.manager.trigger_manual_extract(
                                                            item.id,
                                                            Some(prof.id.clone()),
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        if !item.status.is_finished() {
                                            if ui
                                                .selectable_label(!eff_auto_extract, "Não extrair")
                                                .clicked()
                                            {
                                                self.manager
                                                    .set_extractor_override(item.id, None, false);
                                            }
                                        }
                                    });
                            });
                        }
                        if item.status.is_active() || item.status == DownloadStatus::Queued {
                            if ui.button("Pausar").clicked() {
                                self.manager.pause_download(item.id);
                            }
                        } else if matches!(
                            item.status,
                            DownloadStatus::Paused | DownloadStatus::Failed(_)
                        ) {
                            if accent_button(ui, "Retomar", theme).clicked() {
                                self.manager.resume_download(item.id);
                            }
                        }
                    });
                });

                ui.add_space(4.0);
                // Progress bar + live segmented chunks visualization
                let frac = item.progress_fraction();
                let bar = egui::ProgressBar::new(frac)
                    .desired_height(if theme.compact_mode { 5.0 } else { 7.0 })
                    .fill(if matches!(item.status, DownloadStatus::Failed(_)) {
                        to_c32(theme.error)
                    } else if item.status.is_finished() {
                        to_c32(theme.success)
                    } else {
                        to_c32(theme.accent)
                    });
                ui.add(bar);

                if theme.show_segment_bars && !item.segments.is_empty() && !item.status.is_finished() {
                    ui.add_space(3.0);
                    render_segments_strip(ui, item, theme);
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let size_str = match item.total_bytes {
                        Some(tot) => format!(
                            "{} / {} ({:.1}%)",
                            format_bytes(item.downloaded_bytes),
                            format_bytes(tot),
                            frac * 100.0
                        ),
                        None => format_bytes(item.downloaded_bytes),
                    };
                    ui.label(RichText::new(size_str).small().color(to_c32(theme.text_muted)));
                    ui.label(RichText::new("•").small().color(to_c32(theme.text_muted)));
                    ui.label(
                        RichText::new(format_speed(item.speed_bps))
                            .small()
                            .strong()
                            .color(to_c32(theme.success)),
                    );
                    if item.eta_seconds.is_some() && !item.status.is_finished() {
                        ui.label(RichText::new("•").small().color(to_c32(theme.text_muted)));
                        ui.label(
                            RichText::new(format!("ETA {}", format_eta(item.eta_seconds)))
                                .small()
                                .color(to_c32(theme.text_muted)),
                        );
                    }
                    if item.kind == DownloadKind::Http && item.segments.len() > 1 {
                        ui.label(RichText::new("•").small().color(to_c32(theme.text_muted)));
                        ui.label(
                            RichText::new(format!("{} conexões", item.segments.len()))
                                .small()
                                .color(to_c32(theme.text_muted)),
                        );
                    }
                    if let Some(ref ti) = item.torrent_info {
                        ui.label(RichText::new("•").small().color(to_c32(theme.text_muted)));
                        ui.label(
                            RichText::new(format!(
                                "Peers: {} ({} vistos)  UL {}  Ratio: {:.2}",
                                ti.peers_connected,
                                ti.peers_seen,
                                format_speed(item.upload_speed_bps),
                                ti.ratio
                            ))
                            .small()
                            .color(to_c32(theme.accent)),
                        );
                    }
                    if let Some(ref ex_status) = item.extraction_status {
                        let ex_is_err = ex_status.starts_with("Erro")
                            || ex_status.starts_with("Senha");
                        ui.label(RichText::new("•").small().color(to_c32(theme.text_muted)));
                        ui.label(
                            RichText::new(ex_status).small().color(if ex_is_err {
                                to_c32(theme.error)
                            } else {
                                to_c32(theme.success)
                            }),
                        );
                        if ex_status.starts_with("Senha") {
                            if ui.small_button("Informar Senha").clicked() {
                                self.pw_edit_input =
                                    item.extract_password.clone().unwrap_or_default();
                                self.pw_edit_focus = true;
                                self.pw_edit_show_chars = false;
                                self.pw_edit_target = Some(PasswordEditTarget::Download {
                                    id: item.id,
                                    name: item.filename.clone(),
                                    is_finished: item.status.is_finished(),
                                });
                            }
                        }
                        if item.status == DownloadStatus::Extracting {
                            ui.add_space(4.0);
                            if let Some(ex_frac) = item.extraction_progress {
                                let frac = ex_frac.clamp(0.0, 1.0);
                                ui.add(
                                    egui::ProgressBar::new(frac)
                                        .desired_width(96.0)
                                        .desired_height(6.0)
                                        .fill(to_c32(theme.accent)),
                                );
                                ui.label(
                                    RichText::new(format!("{:.0}%", frac * 100.0))
                                        .small()
                                        .color(to_c32(theme.text_muted)),
                                );
                            } else {
                                let pulse =
                                    ((ui.input(|i| i.time) * 1.6).sin() * 0.5 + 0.5) as f32;
                                ui.add(
                                    egui::ProgressBar::new(0.15 + pulse * 0.7)
                                        .desired_width(96.0)
                                        .desired_height(6.0)
                                        .fill(to_c32(theme.accent)),
                                );
                            }
                        }
                    }
                    if let Some(ref err) = item.error {
                        ui.label(RichText::new(err).small().color(to_c32(theme.error)));
                    }
                });

                if is_selected {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.label(
                        RichText::new(format!("URL: {}", item.url))
                            .small()
                            .color(to_c32(theme.text_muted)),
                    );
                    ui.label(
                        RichText::new(format!("Destino: {}", item.file_path.display()))
                            .small()
                            .color(to_c32(theme.text_muted)),
                    );
                    if let Some(ref ti) = item.torrent_info {
                        if !ti.files.is_empty() {
                            ui.add_space(4.0);
                            ui.label(RichText::new("Arquivos do Torrent:").small().strong());
                            let mut changed = false;
                            let mut included_set: HashSet<usize> = ti
                                .files
                                .iter()
                                .filter(|f| f.included)
                                .map(|f| f.index)
                                .collect();
                            for f in ti.files.iter().take(12) {
                                let mut inc = f.included;
                                let pct = if f.size_bytes > 0 {
                                    (f.downloaded_bytes as f64 / f.size_bytes as f64) * 100.0
                                } else {
                                    0.0
                                };
                                if ui
                                    .checkbox(
                                        &mut inc,
                                        format!("{} ({}, {:.0}%)", f.path, format_bytes(f.size_bytes), pct),
                                    )
                                    .changed()
                                {
                                    changed = true;
                                    if inc {
                                        included_set.insert(f.index);
                                    } else {
                                        included_set.remove(&f.index);
                                    }
                                }
                            }
                            if changed {
                                self.manager.set_torrent_files(item.id, included_set);
                            }
                        }
                    }
                }
            });
    }

    fn render_packages_view(&mut self, ui: &mut Ui, snap: &ManagerSnapshot, theme: &ThemeConfig) {
        ui.heading("Pacotes Multi-Parte (Estilo JDownloader 2)");
        ui.label(
            RichText::new(
                "Arquivos divididos (.part1.rar, .7z.001, .zip.001, .z01/.zip) são agrupados em pacotes e extraídos automaticamente apenas quando todas as partes terminam.",
            )
            .small()
            .color(to_c32(theme.text_muted)),
        );
        ui.add_space(10.0);

        if snap.packages.is_empty() {
            render_empty_state(
                ui,
                "Nenhum pacote multi-parte ainda",
                "Cole vários links no LinkGrabber para agrupar automaticamente partes divididas e configurar extração automática.",
                theme,
            );
            return;
        }

        let (default_ext_id, extractors) = {
            let s = self.manager.settings.read();
            (s.default_extractor_id.clone(), s.extractors.clone())
        };

        ScrollArea::vertical().show(ui, |ui| {
            for pkg in &snap.packages {
                let parts: Vec<&DownloadItem> = pkg
                    .item_ids
                    .iter()
                    .filter_map(|id| snap.downloads.iter().find(|d| d.id == *id))
                    .collect();
                let total_bytes: u64 = parts.iter().filter_map(|p| p.total_bytes).sum();
                let dl_bytes: u64 = parts.iter().map(|p| p.downloaded_bytes).sum();
                let speed_bps: u64 = parts.iter().map(|p| p.speed_bps).sum();
                let done_count = parts.iter().filter(|p| p.status.is_finished()).count();
                let frac = if total_bytes > 0 {
                    (dl_bytes as f64 / total_bytes as f64).clamp(0.0, 1.0) as f32
                } else if !parts.is_empty() && done_count == parts.len() {
                    1.0
                } else {
                    0.0
                };
                let expanded = self.expanded_packages.contains(&pkg.id);

                Frame::new()
                    .fill(to_c32(theme.bg_card))
                    .stroke(Stroke::new(1.0, to_c32(theme.border)))
                    .corner_radius(CornerRadius::same(theme.corner_radius as u8))
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .button(if expanded { "[-]" } else { "[+]" })
                                .clicked()
                            {
                                if expanded {
                                    self.expanded_packages.remove(&pkg.id);
                                } else {
                                    self.expanded_packages.insert(pkg.id);
                                }
                            }
                            ui.label(RichText::new(&pkg.name).strong().size(15.0));
                            pill(
                                ui,
                                &format!("{done_count}/{} partes", parts.len()),
                                to_c32(theme.accent),
                                theme,
                            );
                            if !pkg.missing_parts.is_empty() {
                                pill(
                                    ui,
                                    &format!("Faltando partes: {:?}", pkg.missing_parts),
                                    to_c32(theme.error),
                                    theme,
                                );
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("Pasta").clicked() {
                                    let _ = open::that(&pkg.save_dir);
                                }
                                let pkg_all_done = !parts.is_empty()
                                    && parts.iter().all(|p| p.status == DownloadStatus::Completed);
                                let pkg_extracting = pkg
                                    .extraction_status
                                    .as_deref()
                                    .is_some_and(|s| s.starts_with("Extraindo"))
                                    || parts
                                        .iter()
                                        .any(|p| p.status == DownloadStatus::Extracting);
                                if pkg_all_done {
                                    if ui.button("Extrair").clicked() {
                                        self.manager.trigger_package_extract(pkg.id, None);
                                    }
                                }
                                let pkg_combo_label = if !pkg_all_done {
                                    if !pkg.auto_extract {
                                        "Não extrair".to_string()
                                    } else {
                                        extractor_combo_label(
                                            pkg.extractor_id.as_deref(),
                                            &default_ext_id,
                                            &extractors,
                                        )
                                    }
                                } else {
                                    "Extrair com...".to_string()
                                };
                                ui.add_enabled_ui(!pkg_extracting, |ui| {
                                    egui::ComboBox::from_id_salt(("pkg_ext_now", pkg.id))
                                        .selected_text(pkg_combo_label)
                                        .width(128.0)
                                        .show_ui(ui, |ui| {
                                            let auto_selected =
                                                pkg.auto_extract && pkg.extractor_id.is_none();
                                            if ui
                                                .selectable_label(auto_selected, "Padrão (Auto)")
                                                .clicked()
                                            {
                                                self.manager
                                                    .set_package_extractor(pkg.id, None, true);
                                                if pkg_all_done {
                                                    self.manager
                                                        .trigger_package_extract(pkg.id, None);
                                                }
                                            }
                                            for prof in &extractors {
                                                if prof.detected && prof.enabled {
                                                    let prof_selected = pkg.auto_extract
                                                        && pkg.extractor_id.as_deref()
                                                            == Some(prof.id.as_str());
                                                    if ui
                                                        .selectable_label(prof_selected, &prof.name)
                                                        .clicked()
                                                    {
                                                        self.manager.set_package_extractor(
                                                            pkg.id,
                                                            Some(prof.id.clone()),
                                                            true,
                                                        );
                                                        if pkg_all_done {
                                                            self.manager.trigger_package_extract(
                                                                pkg.id, None,
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                            if !pkg_all_done {
                                                if ui
                                                    .selectable_label(
                                                        !pkg.auto_extract,
                                                        "Não extrair",
                                                    )
                                                    .clicked()
                                                {
                                                    self.manager
                                                        .set_package_extractor(pkg.id, None, false);
                                                }
                                            }
                                        });
                                });
                                let pw_label = if pkg.extract_password.is_some() {
                                    "Senha (OK)"
                                } else {
                                    "Senha"
                                };
                                if ui
                                    .button(pw_label)
                                    .on_hover_text("Definir senha para extração deste pacote")
                                    .clicked()
                                {
                                    self.pw_edit_input =
                                        pkg.extract_password.clone().unwrap_or_default();
                                    self.pw_edit_focus = true;
                                    self.pw_edit_show_chars = false;
                                    self.pw_edit_target = Some(PasswordEditTarget::Package {
                                        id: pkg.id,
                                        name: pkg.name.clone(),
                                        all_finished: pkg_all_done,
                                    });
                                }
                                if ui.button("Pausar Pacote").clicked() {
                                    for p in &parts {
                                        self.manager.pause_download(p.id);
                                    }
                                }
                                if accent_button(ui, "Retomar Pacote", theme).clicked() {
                                    for p in &parts {
                                        self.manager.resume_download(p.id);
                                    }
                                }
                            });
                        });

                        ui.add_space(6.0);
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_height(8.0)
                                .fill(to_c32(theme.accent)),
                        );
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{} / {} • {}",
                                    format_bytes(dl_bytes),
                                    format_bytes(total_bytes),
                                    format_speed(speed_bps)
                                ))
                                .small()
                                .color(to_c32(theme.text_muted)),
                            );
                            if let Some(ref st) = pkg.extraction_status {
                                let pkg_ex_err = st.starts_with("Erro") || st.starts_with("Senha");
                                ui.label(
                                    RichText::new(format!("• {st}")).small().color(if pkg_ex_err {
                                        to_c32(theme.error)
                                    } else {
                                        to_c32(theme.success)
                                    }),
                                );
                                if st.starts_with("Senha") {
                                    let pkg_all_done = !parts.is_empty()
                                        && parts
                                            .iter()
                                            .all(|p| p.status == DownloadStatus::Completed);
                                    if ui.small_button("Informar Senha").clicked() {
                                        self.pw_edit_input =
                                            pkg.extract_password.clone().unwrap_or_default();
                                        self.pw_edit_focus = true;
                                        self.pw_edit_show_chars = false;
                                        self.pw_edit_target = Some(PasswordEditTarget::Package {
                                            id: pkg.id,
                                            name: pkg.name.clone(),
                                            all_finished: pkg_all_done,
                                        });
                                    }
                                }
                                if let Some(ex_frac) = pkg.extraction_progress {
                                    let frac = ex_frac.clamp(0.0, 1.0);
                                    ui.add_space(4.0);
                                    ui.add(
                                        egui::ProgressBar::new(frac)
                                            .desired_width(96.0)
                                            .desired_height(6.0)
                                            .fill(to_c32(theme.accent)),
                                    );
                                    ui.label(
                                        RichText::new(format!("{:.0}%", frac * 100.0))
                                            .small()
                                            .color(to_c32(theme.text_muted)),
                                    );
                                }
                            } else if pkg.auto_extract {
                                let ext_id = pkg
                                    .extractor_id
                                    .as_deref()
                                    .unwrap_or(default_ext_id.as_str());
                                let ext_name = extractor_display_name(ext_id, &extractors);
                                ui.label(
                                    RichText::new(format!("• Extração automática ({ext_name})"))
                                        .small()
                                        .color(to_c32(theme.accent)),
                                );
                            }
                        });

                        if expanded {
                            ui.add_space(6.0);
                            ui.separator();
                            for p in parts {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&p.filename).small());
                                    ui.label(
                                        RichText::new(format!(
                                            "{:.0}% - {}",
                                            p.progress_fraction() * 100.0,
                                            p.status.label()
                                        ))
                                        .small()
                                        .color(to_c32(theme.text_muted)),
                                    );
                                });
                            }
                        }
                    });
                ui.add_space(8.0);
            }
        });
    }

    fn render_link_grabber_view(&mut self, ui: &mut Ui, snap: &ManagerSnapshot, theme: &ThemeConfig) {
        ui.heading("LinkGrabber — Captura em Lote & Multi-Parte");
        ui.label(
            RichText::new(
                "Cole texto de páginas, listas de links (.part1.rar, .part2.rar, .7z.001...), páginas de hosters (FileDitch, Pixeldrain, Gofile, Buzzheavier) ou páginas com vários botões DOWNLOAD. O VortexDL resolve links intermediários, verifica disponibilidade e agrupa pacotes automaticamente.",
            )
            .small()
            .color(to_c32(theme.text_muted)),
        );
        ui.add_space(8.0);

        Frame::new()
            .fill(to_c32(theme.bg_card))
            .stroke(Stroke::new(1.0, to_c32(theme.border)))
            .corner_radius(CornerRadius::same(theme.corner_radius as u8))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.grabber_input)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY)
                        .hint_text("Cole aqui múltiplas URLs (ex: arquivo.part1.rar, arquivo.part2.rar, arquivo.part3.rar)..."),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if accent_button(ui, "Analisar Links", theme).clicked() {
                        let txt = self.grabber_input.clone();
                        self.manager.probe_text_into_grabber(txt, HashMap::new());
                    }
                    if ui.button("Colar da Área de Transferência").clicked() {
                        if let Ok(mut cb) = arboard::Clipboard::new() {
                            if let Ok(t) = cb.get_text() {
                                self.grabber_input = t;
                            }
                        }
                    }
                    if ui.button("Limpar Lista").clicked() {
                        self.manager.grabbed_links.write().clear();
                    }
                });
            });

        ui.add_space(10.0);

        // Package options bar
        let extractors = self.manager.settings.read().extractors.clone();
        Frame::new()
            .fill(to_c32(theme.bg_secondary))
            .stroke(Stroke::new(1.0, to_c32(theme.border)))
            .corner_radius(CornerRadius::same(theme.corner_radius as u8))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Destino:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.grabber_save_dir).desired_width(240.0),
                    );
                    if ui.button("Procurar").clicked() {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            self.grabber_save_dir = folder.to_string_lossy().to_string();
                        }
                    }
                    ui.checkbox(&mut self.grabber_auto_extract, "Auto-extrair pacote ao concluir");

                    egui::ComboBox::from_id_salt("grabber_ext")
                        .selected_text(extractor_display_name(&self.grabber_extractor_id, &extractors))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.grabber_extractor_id, "auto".into(), "Auto (Melhor)");
                            for prof in &extractors {
                                if prof.enabled {
                                    let label = if prof.detected {
                                        prof.name.clone()
                                    } else {
                                        format!("{} (não detectado)", prof.name)
                                    };
                                    ui.selectable_value(&mut self.grabber_extractor_id, prof.id.clone(), label);
                                }
                            }
                        });

                    ui.add(
                        egui::TextEdit::singleline(&mut self.grabber_password)
                            .hint_text("Senha do arquivo (opcional)")
                            .desired_width(150.0),
                    );
                    ui.checkbox(&mut self.grabber_delete_archives, "Apagar .part após extrair");
                });
            });

        ui.add_space(8.0);
        let total_count = snap.grabbed_links.len();
        let selected_count = snap.grabbed_links.iter().filter(|l| l.selected).count();
        let valid_count = snap
            .grabbed_links
            .iter()
            .filter(|l| vortex_core::manager::is_valid_grabbed_link(l))
            .count();
        let matches_valid_only = snap
            .grabbed_links
            .iter()
            .all(|l| l.selected == vortex_core::manager::is_valid_grabbed_link(l));
        let offline_selected_count = snap
            .grabbed_links
            .iter()
            .filter(|l| l.selected && l.online == Some(false))
            .count();
        let total_size: u64 = snap
            .grabbed_links
            .iter()
            .filter(|l| l.selected)
            .filter_map(|l| l.size_bytes)
            .sum();

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{total_count} link(s) capturados ({selected_count} selecionados • {})",
                    format_bytes(total_size)
                ))
                .strong(),
            );
            if total_count > 0 {
                ui.add_space(8.0);
                if ui
                    .add_enabled(
                        !matches_valid_only,
                        egui::Button::new(format!("Marcar Válidos ({valid_count})")),
                    )
                    .on_hover_text(
                        "Seleciona apenas links com tag Parte, status Online e tamanho conhecido",
                    )
                    .clicked()
                {
                    let mut g = self.manager.grabbed_links.write();
                    for item in g.iter_mut() {
                        item.selected = vortex_core::manager::is_valid_grabbed_link(item);
                    }
                }
                if ui
                    .add_enabled(selected_count < total_count, egui::Button::new("Marcar Tudo"))
                    .clicked()
                {
                    let mut g = self.manager.grabbed_links.write();
                    for item in g.iter_mut() {
                        item.selected = true;
                    }
                }
                if ui
                    .add_enabled(selected_count > 0, egui::Button::new("Desmarcar Tudo"))
                    .clicked()
                {
                    let mut g = self.manager.grabbed_links.write();
                    for item in g.iter_mut() {
                        item.selected = false;
                    }
                }
                if offline_selected_count > 0
                    && ui
                        .button(format!("Desmarcar Offline ({offline_selected_count})"))
                        .clicked()
                {
                    let mut g = self.manager.grabbed_links.write();
                    for item in g.iter_mut() {
                        if item.online == Some(false) {
                            item.selected = false;
                        }
                    }
                }
            }
            if snap.grabber_probing {
                ui.add_space(8.0);
                ui.spinner();
                ui.label(
                    RichText::new("Analisando e resolvendo páginas intermediárias...")
                        .small()
                        .color(to_c32(theme.accent)),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if selected_count > 0
                    && accent_button(ui, format!("Iniciar Download ({selected_count})"), theme).clicked()
                {
                    let dir = PathBuf::from(&self.grabber_save_dir);
                    let pw = if self.grabber_password.trim().is_empty() {
                        None
                    } else {
                        Some(self.grabber_password.trim().to_string())
                    };
                    self.manager.enqueue_selected_grabbed(
                        Some(dir),
                        self.grabber_auto_extract,
                        Some(self.grabber_extractor_id.clone()),
                        pw,
                        self.grabber_delete_archives,
                    );
                    self.active_tab = NavTab::Packages;
                }
            });
        });

        ui.add_space(6.0);
        ScrollArea::vertical().show(ui, |ui| {
            let mut toggles: Vec<(Uuid, bool)> = Vec::new();
            for link in &snap.grabbed_links {
                Frame::new()
                    .fill(to_c32(theme.bg_card))
                    .stroke(Stroke::new(1.0, to_c32(theme.border)))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let mut sel = link.selected;
                            if ui.checkbox(&mut sel, "").changed() {
                                toggles.push((link.id, sel));
                            }
                            let site_label = link
                                .source_site
                                .as_deref()
                                .map(str::to_string)
                                .or_else(|| {
                                    vortex_core::package_grabber::classify_source_site(&link.url)
                                });
                            if let Some(ref site) = site_label {
                                let (tag, color) = site_badge_style(site, theme);
                                pill(ui, &format!("{tag} • {site}"), color, theme);
                            }
                            ui.label(RichText::new(&link.filename).strong())
                                .on_hover_text(&link.url);
                            if let Some(ref pkg) = link.package_key {
                                pill(ui, &format!("Pacote: {pkg}"), to_c32(theme.accent), theme);
                            }
                            if let Some(pn) = link.part_number {
                                pill(ui, &format!("Parte #{pn}"), to_c32(theme.warning), theme);
                            }
                            match link.online {
                                Some(true) => pill(ui, "Online", to_c32(theme.success), theme),
                                Some(false) => pill(ui, "Offline/Erro", to_c32(theme.error), theme),
                                None => {}
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(
                                        link.size_bytes
                                            .map(format_bytes)
                                            .unwrap_or_else(|| "Tamanho ?".into()),
                                    )
                                    .color(to_c32(theme.text_muted)),
                                );
                            });
                        });
                        if let Some(ref err) = link.error {
                            ui.label(
                                RichText::new(err)
                                    .small()
                                    .color(to_c32(theme.error)),
                            );
                        }
                    });
                ui.add_space(4.0);
            }
            if !toggles.is_empty() {
                let mut g = self.manager.grabbed_links.write();
                for (id, sel) in toggles {
                    if let Some(item) = g.iter_mut().find(|l| l.id == id) {
                        item.selected = sel;
                    }
                }
            }
        });
    }

    fn render_media_view(&mut self, ui: &mut Ui, snap: &ManagerSnapshot, theme: &ThemeConfig) {
        ui.heading("Downloader Universal de Vídeos & Mídias");
        ui.label(
            RichText::new(
                "Baixe vídeos e áudios de qualquer plataforma (YouTube, Twitch, TikTok, Instagram, X, Vimeo, streams .m3u8/.mpd capturados pelo navegador, etc.) com escolha de qualidade e merge automático.",
            )
            .small()
            .color(to_c32(theme.text_muted)),
        );
        ui.add_space(8.0);

        // yt-dlp + FFmpeg quick installer bar
        Frame::new()
            .fill(to_c32(theme.bg_secondary))
            .stroke(Stroke::new(1.0, to_c32(theme.border)))
            .corner_radius(CornerRadius::same(theme.corner_radius as u8))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let yt_ok = snap.media_tools.yt_dlp_path.is_some();
                    let ff_ok = snap.media_tools.ffmpeg_path.is_some();
                    pill(
                        ui,
                        if yt_ok { "yt-dlp: Instalado" } else { "yt-dlp: Não encontrado" },
                        if yt_ok { to_c32(theme.success) } else { to_c32(theme.warning) },
                        theme,
                    );
                    if ui.button(if yt_ok { "Atualizar yt-dlp" } else { "Instalar yt-dlp (1-clique)" }).clicked() {
                        self.manager.install_yt_dlp_async();
                    }
                    ui.add_space(8.0);
                    pill(
                        ui,
                        if ff_ok { "FFmpeg: Instalado" } else { "FFmpeg: Não encontrado" },
                        if ff_ok { to_c32(theme.success) } else { to_c32(theme.warning) },
                        theme,
                    );
                    if ui.button(if ff_ok { "Reinstalar FFmpeg" } else { "Instalar FFmpeg (1-clique)" }).clicked() {
                        self.manager.install_ffmpeg_async();
                    }
                });
            });

        ui.add_space(10.0);

        Frame::new()
            .fill(to_c32(theme.bg_card))
            .stroke(Stroke::new(1.0, to_c32(theme.border)))
            .corner_radius(CornerRadius::same(theme.corner_radius as u8))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("URL da Mídia / Vídeo / Stream (.m3u8):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.media_url)
                            .desired_width(370.0)
                            .hint_text("https://... (página de vídeo, playlist ou stream .m3u8)"),
                    );
                    if ui.button("Colar").clicked() {
                        if let Some(txt) = read_clipboard_text() {
                            self.media_url = txt.trim().to_string();
                        }
                    }
                });
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label("Salvar em:");
                    ui.add(egui::TextEdit::singleline(&mut self.media_save_dir).desired_width(240.0));
                    if ui.button("Procurar").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.media_save_dir = p.to_string_lossy().to_string();
                        }
                    }
                    ui.checkbox(&mut self.media_audio_only, "Somente Áudio (MP3)");
                    ui.checkbox(&mut self.media_embed_subs, "Embutir Legendas");
                });
                ui.add_space(8.0);
                let is_probing = *self.media_probing.lock();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!is_probing, egui::Button::new(if is_probing { "Analisando..." } else { "Analisar Formatos (yt-dlp)" }))
                        .clicked()
                    {
                        let url = self.media_url.trim().to_string();
                        if !url.is_empty() {
                            *self.media_probing.lock() = true;
                            *self.media_probe_result.lock() = None;
                            let mgr = self.manager.clone();
                            let probing = self.media_probing.clone();
                            let res_slot = self.media_probe_result.clone();
                            self.runtime.spawn(async move {
                                let res = mgr.probe_media(&url).await.map_err(|e| e.to_string());
                                *res_slot.lock() = Some(res);
                                *probing.lock() = false;
                            });
                        }
                    }

                    if accent_button(ui, "Baixar Melhor Qualidade Agora", theme).clicked() {
                        let url = self.media_url.trim().to_string();
                        if !url.is_empty() {
                            let is_hls = url.to_lowercase().contains(".m3u8");
                            let _ = self.manager.add_media_download(NewMediaRequest {
                                url,
                                save_dir: Some(PathBuf::from(&self.media_save_dir)),
                                format_id: None,
                                quality_label: Some("Melhor".into()),
                                audio_only: self.media_audio_only,
                                embed_subtitles: self.media_embed_subs,
                                headers: HashMap::new(),
                                title_hint: None,
                                is_hls,
                                is_dash: false,
                            });
                            self.active_tab = NavTab::All;
                        }
                    }
                });
            });

        ui.add_space(10.0);

        let probe_opt = self.media_probe_result.lock().clone();
        if let Some(probe_res) = probe_opt {
            match probe_res {
                Err(err) => {
                    ui.label(RichText::new(format!("Erro ao analisar mídia: {err}")).color(to_c32(theme.error)));
                }
                Ok(info) => {
                    Frame::new()
                        .fill(to_c32(theme.bg_card))
                        .stroke(Stroke::new(1.0, to_c32(theme.border)))
                        .corner_radius(CornerRadius::same(theme.corner_radius as u8))
                        .inner_margin(Margin::same(14))
                        .show(ui, |ui| {
                            ui.label(RichText::new(&info.title).strong().size(16.0));
                            ui.label(
                                RichText::new(format!(
                                    "Extrator: {}  •  Duração: {}  •  {}",
                                    info.extractor,
                                    format_duration(info.duration_secs),
                                    info.uploader.as_deref().unwrap_or("")
                                ))
                                .small()
                                .color(to_c32(theme.text_muted)),
                            );
                            ui.add_space(8.0);

                            if info.is_playlist {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "Playlist detectada ({} itens)",
                                            info.playlist_count
                                        ))
                                        .strong()
                                        .color(to_c32(theme.accent)),
                                    );
                                    if accent_button(ui, "Baixar Toda a Playlist", theme).clicked() {
                                        for entry in &info.playlist_entries {
                                            let _ = self.manager.add_media_download(NewMediaRequest {
                                                url: entry.url.clone(),
                                                save_dir: Some(PathBuf::from(&self.media_save_dir)),
                                                format_id: None,
                                                quality_label: Some("Melhor".into()),
                                                audio_only: self.media_audio_only,
                                                embed_subtitles: self.media_embed_subs,
                                                headers: HashMap::new(),
                                                title_hint: Some(entry.title.clone()),
                                                is_hls: false,
                                                is_dash: false,
                                            });
                                        }
                                        self.active_tab = NavTab::All;
                                    }
                                });
                            }

                            ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                                for fmt in &info.formats {
                                    ui.horizontal(|ui| {
                                        pill(ui, &fmt.ext, to_c32(theme.accent), theme);
                                        ui.label(RichText::new(&fmt.resolution).strong());
                                        if let Some(fps) = fmt.fps {
                                            ui.label(RichText::new(format!("{fps:.0}fps")).small().color(to_c32(theme.text_muted)));
                                        }
                                        if !fmt.note.is_empty() {
                                            ui.label(RichText::new(&fmt.note).small().color(to_c32(theme.text_muted)));
                                        }
                                        if let Some(sz) = fmt.filesize {
                                            ui.label(RichText::new(format_bytes(sz)).small().color(to_c32(theme.text_muted)));
                                        }
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            if ui.button("Baixar Formato").clicked() {
                                                let _ = self.manager.add_media_download(NewMediaRequest {
                                                    url: info.url.clone(),
                                                    save_dir: Some(PathBuf::from(&self.media_save_dir)),
                                                    format_id: Some(fmt.format_id.clone()),
                                                    quality_label: Some(fmt.resolution.clone()),
                                                    audio_only: fmt.is_audio_only || self.media_audio_only,
                                                    embed_subtitles: self.media_embed_subs,
                                                    headers: HashMap::new(),
                                                    title_hint: Some(info.title.clone()),
                                                    is_hls: false,
                                                    is_dash: false,
                                                });
                                                self.active_tab = NavTab::All;
                                            }
                                        });
                                    });
                                    ui.separator();
                                }
                            });
                        });
                }
            }
        }
    }

    fn render_extractors_view(&mut self, ui: &mut Ui, theme: &ThemeConfig) {
        ui.heading("Extratores & Plugins Pós-Download");
        ui.label(
            RichText::new(
                "Escolha o utilitário padrão ao concluir downloads compactados (WinRAR, NanaZip, PeaZip, 7-Zip, Windows Nativo) ou adicione plugins CLI customizados.",
            )
            .small()
            .color(to_c32(theme.text_muted)),
        );
        ui.add_space(8.0);

        let mut changed = false;
        {
            let mut settings = self.manager.settings.write();
            Frame::new()
                .fill(to_c32(theme.bg_secondary))
                .stroke(Stroke::new(1.0, to_c32(theme.border)))
                .corner_radius(CornerRadius::same(theme.corner_radius as u8))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        changed |= ui
                            .checkbox(&mut settings.auto_extract_archives, "Extrair arquivos automaticamente após download")
                            .changed();
                        changed |= ui
                            .checkbox(&mut settings.extract_into_subfolder, "Criar subpasta com o nome do arquivo")
                            .changed();
                        changed |= ui
                            .checkbox(&mut settings.delete_archives_after_extract, "Apagar compactados após extrair")
                            .changed();
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Extrator Padrão:");
                        let names: Vec<(String, String)> = settings
                            .extractors
                            .iter()
                            .map(|e| (e.id.clone(), e.name.clone()))
                            .collect();
                        let cur_label = extractor_display_name(&settings.default_extractor_id, &settings.extractors);
                        egui::ComboBox::from_id_salt("default_ext_combo")
                            .selected_text(cur_label)
                            .show_ui(ui, |ui| {
                                changed |= ui
                                    .selectable_value(&mut settings.default_extractor_id, "auto".into(), "Automático (Prioridade: NanaZip > 7-Zip > WinRAR > PeaZip > Windows)")
                                    .changed();
                                for (id, name) in names {
                                    changed |= ui
                                        .selectable_value(&mut settings.default_extractor_id, id, name)
                                        .changed();
                                }
                            });
                        if ui.button("Redetectar Instalados").clicked() {
                            drop(settings);
                            self.manager.refresh_extractors();
                            return;
                        }
                    });
                });
        }
        if changed {
            let _ = self.manager.save_current_settings();
        }

        ui.add_space(10.0);

        ScrollArea::vertical().show(ui, |ui| {
            let mut ext_changed = false;
            {
                let mut settings = self.manager.settings.write();
                for prof in &mut settings.extractors {
                    Frame::new()
                        .fill(to_c32(theme.bg_card))
                        .stroke(Stroke::new(1.0, to_c32(theme.border)))
                        .corner_radius(CornerRadius::same(theme.corner_radius as u8))
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ext_changed |= ui.checkbox(&mut prof.enabled, "").changed();
                                ui.label(RichText::new(&prof.name).strong().size(14.5));
                                pill(
                                    ui,
                                    if prof.detected { "Detectado" } else { "Não instalado" },
                                    if prof.detected { to_c32(theme.success) } else { to_c32(theme.text_muted) },
                                    theme,
                                );
                                pill(ui, prof.kind.label(), to_c32(theme.accent), theme);
                            });
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Executável:").small().color(to_c32(theme.text_muted)));
                                ext_changed |= ui
                                    .add(egui::TextEdit::singleline(&mut prof.executable).desired_width(360.0))
                                    .changed();
                                if ui.button("Selecionar .exe").clicked() {
                                    if let Some(p) = rfd::FileDialog::new()
                                        .add_filter("Executável", &["exe", "bat", "cmd"])
                                        .pick_file()
                                    {
                                        prof.executable = p.to_string_lossy().to_string();
                                        prof.detected = true;
                                        ext_changed = true;
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Args:").small().color(to_c32(theme.text_muted)));
                                ext_changed |= ui
                                    .add(egui::TextEdit::singleline(&mut prof.args_template).desired_width(360.0))
                                    .changed();
                            });
                        });
                    ui.add_space(6.0);
                }

                // Password list manager
                Frame::new()
                    .fill(to_c32(theme.bg_card))
                    .stroke(Stroke::new(1.0, to_c32(theme.border)))
                    .corner_radius(CornerRadius::same(theme.corner_radius as u8))
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.label(RichText::new("Lista de Senhas Automáticas (Estilo JDownloader 2)").strong());
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.new_password_entry)
                                    .hint_text("Adicionar senha comum...")
                                    .desired_width(240.0),
                            );
                            if ui.button("Adicionar Senha").clicked() && !self.new_password_entry.is_empty() {
                                settings.password_list.push(self.new_password_entry.clone());
                                self.new_password_entry.clear();
                                ext_changed = true;
                            }
                        });
                        let mut remove_idx = None;
                        for (idx, pw) in settings.password_list.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(pw).small());
                                if ui.button("Remover").clicked() {
                                    remove_idx = Some(idx);
                                }
                            });
                        }
                        if let Some(idx) = remove_idx {
                            settings.password_list.remove(idx);
                            ext_changed = true;
                        }
                    });

                ui.add_space(8.0);
                // Add custom plugin form
                Frame::new()
                    .fill(to_c32(theme.bg_card))
                    .stroke(Stroke::new(1.0, to_c32(theme.border)))
                    .corner_radius(CornerRadius::same(theme.corner_radius as u8))
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.label(RichText::new("+ Adicionar Plugin Extrator Customizado").strong());
                        ui.horizontal(|ui| {
                            ui.label("Nome:");
                            ui.add(egui::TextEdit::singleline(&mut self.new_ext_name).desired_width(160.0));
                            ui.label("Executável:");
                            ui.add(egui::TextEdit::singleline(&mut self.new_ext_exe).desired_width(240.0));
                            if ui.button("Procurar").clicked() {
                                if let Some(p) = rfd::FileDialog::new().pick_file() {
                                    self.new_ext_exe = p.to_string_lossy().to_string();
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Template Args:");
                            ui.add(egui::TextEdit::singleline(&mut self.new_ext_args).desired_width(280.0));
                            ui.label("Flag Senha:");
                            ui.add(egui::TextEdit::singleline(&mut self.new_ext_pw_arg).desired_width(120.0));
                            if accent_button(ui, "Salvar Plugin", theme).clicked()
                                && !self.new_ext_name.trim().is_empty()
                                && !self.new_ext_exe.trim().is_empty()
                            {
                                let id = format!("custom_{}", Uuid::new_v4().simple());
                                settings.extractors.push(ExtractorProfile {
                                    id,
                                    name: self.new_ext_name.trim().to_string(),
                                    kind: ExtractorKind::CustomPlugin,
                                    executable: self.new_ext_exe.trim().to_string(),
                                    args_template: self.new_ext_args.clone(),
                                    password_arg_template: if self.new_ext_pw_arg.trim().is_empty() {
                                        None
                                    } else {
                                        Some(self.new_ext_pw_arg.trim().to_string())
                                    },
                                    silent: true,
                                    detected: std::path::Path::new(self.new_ext_exe.trim()).exists(),
                                    detected_path: Some(self.new_ext_exe.trim().to_string()),
                                    supported_extensions: vec!["zip".into(), "rar".into(), "7z".into()],
                                    enabled: true,
                                });
                                self.new_ext_name.clear();
                                self.new_ext_exe.clear();
                                ext_changed = true;
                            }
                        });
                    });
            }
            if ext_changed {
                let _ = self.manager.save_current_settings();
            }
        });
    }

    fn render_extension_view(&mut self, ui: &mut Ui, theme: &ThemeConfig) {
        ui.heading("Extensão do Navegador & Native Messaging");
        ui.label(
            RichText::new(
                "Capture downloads automaticamente, detecte vídeos e streams (.m3u8/.mpd/.mp4) de qualquer página e envie todos os links para o LinkGrabber com cookies e headers de sessão.",
            )
            .small()
            .color(to_c32(theme.text_muted)),
        );
        ui.add_space(10.0);

        let port = self.manager.settings.read().ipc_port;
        Frame::new()
            .fill(to_c32(theme.bg_card))
            .stroke(Stroke::new(1.0, to_c32(theme.border)))
            .corner_radius(CornerRadius::same(theme.corner_radius as u8))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    pill(ui, &format!("API Local Ativa: http://127.0.0.1:{port}"), to_c32(theme.success), theme);
                    ui.label(
                        RichText::new("A extensão funciona imediatamente via API Local e também via Native Messaging Host (que abre o app automaticamente se estiver fechado).")
                            .small()
                            .color(to_c32(theme.text_muted)),
                    );
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label("ID da Extensão (Chrome/Edge, opcional):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.chrome_ext_id_input)
                            .hint_text("ex: abcdefghijklmnopqrstuvwxyz123456 (vazio = qualquer)")
                            .desired_width(340.0),
                    );
                });
                ui.add_space(8.0);
                if accent_button(ui, "Registrar Native Messaging Host no Windows", theme).clicked() {
                    let host_exe = std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.join("vortex-native-host.exe")))
                        .unwrap_or_else(|| PathBuf::from("vortex-native-host.exe"));

                    let id_opt = if self.chrome_ext_id_input.trim().is_empty() {
                        None
                    } else {
                        Some(self.chrome_ext_id_input.as_str())
                    };
                    match register_browser_native_host(&host_exe, id_opt) {
                        Ok(browsers) => {
                            self.ext_reg_status = Some(format!(
                                "Registrado com sucesso para: {} (Host: {})",
                                browsers.join(", "),
                                host_exe.display()
                            ));
                        }
                        Err(e) => {
                            self.ext_reg_status = Some(format!("Erro ao registrar: {e}"));
                        }
                    }
                }
                if let Some(ref st) = self.ext_reg_status {
                    ui.add_space(6.0);
                    ui.label(RichText::new(st).small().color(to_c32(theme.accent)));
                }

                ui.add_space(12.0);
                ui.separator();
                ui.label(RichText::new("Como instalar a extensão no Chrome / Edge / Brave:").strong());
                ui.label("1. Abra chrome://extensions (ou edge://extensions) e ative o 'Modo do desenvolvedor'.");
                ui.label("2. Clique em 'Carregar sem compactação' (Load unpacked) e selecione a pasta `extension/` deste repositório.");
                ui.label("3. O ícone do VortexDL aparecerá no navegador capturando downloads e detectando mídias em qualquer aba.");
            });
    }

    fn render_themes_view(&mut self, ui: &mut Ui, theme: &ThemeConfig) {
        ui.heading("Theme Studio — Personalização Visual");
        ui.label(
            RichText::new("Escolha um tema pronto ou personalize cores, cantos arredondados, densidade, escala de fonte e painéis ao vivo.")
                .small()
                .color(to_c32(theme.text_muted)),
        );
        ui.add_space(10.0);

        ui.horizontal_wrapped(|ui| {
            for preset in ThemeConfig::presets() {
                let selected = preset.preset_id == theme.preset_id;
                let btn = if selected {
                    accent_button(ui, &preset.name, theme)
                } else {
                    ui.button(&preset.name)
                };
                if btn.clicked() {
                    self.manager.settings.write().theme = preset;
                    let _ = self.manager.save_current_settings();
                }
            }
        });

        ui.add_space(10.0);
        let mut changed = false;
        let mut save_custom = false;
        {
            let mut settings = self.manager.settings.write();
            let t = &mut settings.theme;

            Frame::new()
                .fill(to_c32(theme.bg_card))
                .stroke(Stroke::new(1.0, to_c32(theme.border)))
                .corner_radius(CornerRadius::same(theme.corner_radius as u8))
                .inner_margin(Margin::same(14))
                .show(ui, |ui| {
                    ui.columns(2, |cols| {
                        cols[0].label(RichText::new("Paleta de Cores").strong());
                        changed |= color_row(&mut cols[0], "Fundo Principal", &mut t.bg_primary);
                        changed |= color_row(&mut cols[0], "Fundo Secundário / Barra", &mut t.bg_secondary);
                        changed |= color_row(&mut cols[0], "Cartões", &mut t.bg_card);
                        changed |= color_row(&mut cols[0], "Destaque (Accent)", &mut t.accent);
                        changed |= color_row(&mut cols[0], "Texto Principal", &mut t.text_primary);
                        changed |= color_row(&mut cols[0], "Texto Secundário", &mut t.text_muted);
                        changed |= color_row(&mut cols[0], "Bordas", &mut t.border);
                        changed |= color_row(&mut cols[0], "Sucesso / Velocidade", &mut t.success);

                        cols[1].label(RichText::new("Layout & Densidade").strong());
                        changed |= cols[1].checkbox(&mut t.dark_mode, "Modo Escuro").changed();
                        changed |= cols[1].checkbox(&mut t.compact_mode, "Modo Compacto").changed();
                        changed |= cols[1].checkbox(&mut t.show_segment_bars, "Mostrar barras de segmentos HTTP").changed();
                        changed |= cols[1].checkbox(&mut t.show_speed_graph, "Mostrar gráfico de velocidade ao vivo").changed();
                        changed |= cols[1].add(egui::Slider::new(&mut t.corner_radius, 0.0..=18.0).text("Arredondamento")).changed();
                        changed |= cols[1].add(egui::Slider::new(&mut t.font_scale, 0.85..=1.3).text("Escala da Fonte")).changed();
                        changed |= cols[1].add(egui::Slider::new(&mut t.sidebar_width, 180.0..=290.0).text("Largura Sidebar")).changed();
                    });

                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.label("Salvar como tema customizado:");
                        ui.add(egui::TextEdit::singleline(&mut self.custom_theme_name).desired_width(180.0));
                        if accent_button(ui, "Salvar Tema", theme).clicked() && !self.custom_theme_name.trim().is_empty() {
                            save_custom = true;
                        }
                    });
                });

            if save_custom {
                let mut saved = settings.theme.clone();
                saved.name = self.custom_theme_name.trim().to_string();
                saved.preset_id = format!("custom_{}", saved.name.to_lowercase().replace(' ', "_"));
                settings.custom_themes.retain(|x| x.name != saved.name);
                settings.custom_themes.push(saved.clone());
                settings.theme = saved;
                changed = true;
            }
        }
        if changed {
            let _ = self.manager.save_current_settings();
        }
    }

    fn render_settings_view(&mut self, ui: &mut Ui, theme: &ThemeConfig) {
        ui.heading("Configurações Gerais");
        ui.add_space(8.0);
        let mut changed = false;
        {
            let mut s = self.manager.settings.write();
            Frame::new()
                .fill(to_c32(theme.bg_card))
                .stroke(Stroke::new(1.0, to_c32(theme.border)))
                .corner_radius(CornerRadius::same(theme.corner_radius as u8))
                .inner_margin(Margin::same(14))
                .show(ui, |ui| {
                    ui.label(RichText::new("Diretórios & Categorias").strong());
                    ui.horizontal(|ui| {
                        let mut dir_str = s.default_download_dir.to_string_lossy().to_string();
                        if ui.add(egui::TextEdit::singleline(&mut dir_str).desired_width(360.0)).changed() {
                            s.default_download_dir = PathBuf::from(dir_str);
                            changed = true;
                        }
                        if ui.button("Escolher Pasta").clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                s.default_download_dir = p;
                                changed = true;
                            }
                        }
                    });
                    changed |= ui.checkbox(&mut s.categorize_into_subfolders, "Organizar downloads por categoria (Videos, Archives, Programs...)").changed();

                    ui.add_space(8.0);
                    ui.separator();
                    ui.label(RichText::new("Motor & Rede").strong());
                    changed |= ui.add(egui::Slider::new(&mut s.max_concurrent_downloads, 1..=12).text("Downloads simultâneos na fila")).changed();
                    changed |= ui.add(egui::Slider::new(&mut s.default_connections, 1..=32).text("Conexões por arquivo (segmentação)")).changed();
                    changed |= ui.checkbox(&mut s.clipboard_monitor, "Monitorar área de transferência para links").changed();
                    changed |= ui.checkbox(&mut s.auto_start_downloads, "Iniciar downloads automaticamente ao adicionar").changed();

                    ui.add_space(8.0);
                    ui.separator();
                    ui.label(RichText::new("BitTorrent").strong());
                    changed |= ui.checkbox(&mut s.torrent_seed_after_complete, "Continuar semeando após concluir").changed();
                    changed |= ui.add(egui::Slider::new(&mut s.torrent_max_ratio, 0.0..=5.0).text("Ratio máximo de seed (0 = ilimitado)")).changed();
                });
        }
        if changed {
            let _ = self.manager.save_current_settings();
        }
    }

    fn render_new_http_modal(&mut self, ctx: &egui::Context, theme: &ThemeConfig) {
        if !self.show_new_http_modal {
            return;
        }
        let extractors = self.manager.settings.read().extractors.clone();
        egui::Window::new("Novo Download")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(480.0);
                ui.label("URL (HTTP/HTTPS ou Magnet):");
                ui.horizontal(|ui| {
                    let resp = ui.add(egui::TextEdit::singleline(&mut self.new_http_url).desired_width(410.0));
                    if self.focus_new_http_url {
                        resp.request_focus();
                        self.focus_new_http_url = false;
                    }
                    if ui.button("Colar").clicked() {
                        if let Some(txt) = read_clipboard_text() {
                            self.new_http_url = txt.trim().to_string();
                        }
                    }
                });

                let detected_media =
                    vortex_core::media_engine::detect_media_url(self.new_http_url.trim());
                if let Some(media_match) = detected_media {
                    let kind_label = if media_match.is_hls {
                        "Stream HLS detectado — será baixado via motor de vídeo."
                    } else if media_match.is_dash {
                        "Stream DASH detectado — será baixado via motor de vídeo."
                    } else {
                        "Link de vídeo detectado — será baixado automaticamente em alta qualidade (yt-dlp)."
                    };
                    ui.colored_label(
                        to_c32(theme.accent),
                        kind_label,
                    );
                }

                ui.label("Nome do arquivo (opcional — detectado via servidor):");
                ui.add(egui::TextEdit::singleline(&mut self.new_http_filename).desired_width(f32::INFINITY));

                ui.horizontal(|ui| {
                    ui.label("Salvar em:");
                    ui.add(egui::TextEdit::singleline(&mut self.new_http_dir).desired_width(300.0));
                    if ui.button("Procurar").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.new_http_dir = p.to_string_lossy().to_string();
                        }
                    }
                });

                ui.add(egui::Slider::new(&mut self.new_http_connections, 1..=32).text("Conexões (Work-Stealing)"));

                if detected_media.is_none() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.new_http_auto_extract, "Auto-extrair");
                        ui.label("Senha (opcional):");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_http_password)
                                .hint_text("Senha do .rar / .zip / .7z")
                                .desired_width(210.0),
                        );
                    });
                }

                ui.collapsing("Avançado (Headers, Checksum, Extrator)", |ui| {
                    ui.label("Referer (opcional):");
                    ui.add(egui::TextEdit::singleline(&mut self.new_http_referer).desired_width(f32::INFINITY));
                    ui.label("Cookie (opcional):");
                    ui.add(egui::TextEdit::singleline(&mut self.new_http_cookie).desired_width(f32::INFINITY));
                    ui.label("Checksum esperado (sha256:... ou md5:...):");
                    ui.add(egui::TextEdit::singleline(&mut self.new_http_checksum).desired_width(f32::INFINITY));
                    egui::ComboBox::from_id_salt("modal_ext_combo")
                        .selected_text(extractor_display_name(&self.new_http_extractor_id, &extractors))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.new_http_extractor_id, "auto".into(), "Auto");
                            for prof in &extractors {
                                ui.selectable_value(&mut self.new_http_extractor_id, prof.id.clone(), &prof.name);
                            }
                        });
                });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancelar").clicked() {
                        self.show_new_http_modal = false;
                    }
                    if detected_media.is_some() && ui.button("Escolher Qualidade...").clicked() {
                        let url = self.new_http_url.trim().to_string();
                        if !url.is_empty() {
                            self.media_url = url.clone();
                            self.media_save_dir = self.new_http_dir.clone();
                            *self.media_probing.lock() = true;
                            *self.media_probe_result.lock() = None;
                            let mgr = self.manager.clone();
                            let probing = self.media_probing.clone();
                            let res_slot = self.media_probe_result.clone();
                            self.runtime.spawn(async move {
                                let res = mgr.probe_media(&url).await.map_err(|e| e.to_string());
                                *res_slot.lock() = Some(res);
                                *probing.lock() = false;
                            });
                            self.active_tab = NavTab::Media;
                            self.new_http_url.clear();
                            self.new_http_filename.clear();
                            self.show_new_http_modal = false;
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let btn_label = if detected_media.is_some() {
                            "Baixar Vídeo"
                        } else {
                            "Iniciar Download"
                        };
                        if accent_button(ui, btn_label, theme).clicked()
                            && !self.new_http_url.trim().is_empty()
                        {
                            let mut headers = HashMap::new();
                            if !self.new_http_referer.trim().is_empty() {
                                headers.insert("Referer".into(), self.new_http_referer.trim().into());
                            }
                            if !self.new_http_cookie.trim().is_empty() {
                                headers.insert("Cookie".into(), self.new_http_cookie.trim().into());
                            }
                            let _ = self.manager.add_http_download(NewDownloadRequest {
                                url: self.new_http_url.trim().to_string(),
                                filename: if self.new_http_filename.trim().is_empty() {
                                    None
                                } else {
                                    Some(self.new_http_filename.trim().to_string())
                                },
                                save_dir: Some(PathBuf::from(&self.new_http_dir)),
                                connections: Some(self.new_http_connections),
                                headers,
                                checksum: if self.new_http_checksum.trim().is_empty() {
                                    None
                                } else {
                                    Some(self.new_http_checksum.trim().to_string())
                                },
                                auto_extract: Some(self.new_http_auto_extract),
                                extractor_override: Some(self.new_http_extractor_id.clone()),
                                extract_password: if self.new_http_password.trim().is_empty() {
                                    None
                                } else {
                                    Some(self.new_http_password.trim().to_string())
                                },
                                start_immediately: Some(true),
                            });
                            self.new_http_url.clear();
                            self.new_http_filename.clear();
                            self.new_http_password.clear();
                            self.show_new_http_modal = false;
                        }
                    });
                });
            });
    }

    fn render_password_prompt_modal(
        &mut self,
        ctx: &egui::Context,
        snap: &ManagerSnapshot,
        theme: &ThemeConfig,
    ) {
        let Some(prompt) = snap.pending_password_prompts.first().cloned() else {
            self.pw_modal_id = None;
            return;
        };
        if self.pw_modal_id != Some(prompt.id) {
            self.pw_modal_id = Some(prompt.id);
            self.pw_modal_input.clear();
            self.pw_modal_focus = true;
            self.pw_modal_show_chars = false;
        }

        egui::Window::new("Senha de Extração Necessária")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(430.0);
                if prompt.wrong_password {
                    ui.label(
                        RichText::new("A senha informada está incorreta. Tente novamente:")
                            .color(to_c32(theme.error))
                            .strong(),
                    );
                } else {
                    ui.label(
                        RichText::new("Este arquivo compactado está protegido por senha:")
                            .color(to_c32(theme.text_muted)),
                    );
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(&prompt.display_name)
                        .strong()
                        .color(to_c32(theme.text_primary)),
                );
                ui.add_space(8.0);

                let mut submit_now = false;
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.pw_modal_input)
                            .password(!self.pw_modal_show_chars)
                            .hint_text("Digite a senha do arquivo...")
                            .desired_width(270.0),
                    );
                    if self.pw_modal_focus {
                        resp.request_focus();
                        self.pw_modal_focus = false;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit_now = true;
                    }
                    if ui
                        .button(if self.pw_modal_show_chars {
                            "Ocultar"
                        } else {
                            "Mostrar"
                        })
                        .clicked()
                    {
                        self.pw_modal_show_chars = !self.pw_modal_show_chars;
                    }
                    if ui.button("Colar").clicked() {
                        if let Some(txt) = read_clipboard_text() {
                            self.pw_modal_input = txt.trim().to_string();
                        }
                    }
                });

                ui.add_space(6.0);
                ui.checkbox(
                    &mut self.pw_modal_save_to_list,
                    "Salvar esta senha na lista automática (Configurações / Extratores)",
                );
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    if ui.button("Cancelar").clicked() {
                        self.manager.dismiss_password_prompt(prompt.id);
                        self.pw_modal_id = None;
                        self.pw_modal_input.clear();
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let can_submit = !self.pw_modal_input.trim().is_empty();
                        if ui
                            .add_enabled_ui(can_submit, |ui| accent_button(ui, "Extrair Agora", theme))
                            .inner
                            .clicked()
                        {
                            submit_now = true;
                        }
                    });
                });

                if submit_now && !self.pw_modal_input.trim().is_empty() {
                    let pw = self.pw_modal_input.trim().to_string();
                    self.manager
                        .submit_extraction_password(prompt.id, pw, self.pw_modal_save_to_list);
                    self.pw_modal_id = None;
                    self.pw_modal_input.clear();
                }
            });
    }

    fn render_set_password_modal(&mut self, ctx: &egui::Context, theme: &ThemeConfig) {
        let Some(target) = self.pw_edit_target.clone() else {
            return;
        };

        let (title_name, is_finished) = match &target {
            PasswordEditTarget::Download {
                name, is_finished, ..
            } => (name.as_str(), *is_finished),
            PasswordEditTarget::Package {
                name, all_finished, ..
            } => (name.as_str(), *all_finished),
        };

        egui::Window::new("Senha de Extração")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(410.0);
                ui.label(RichText::new(title_name).strong());
                ui.label(
                    RichText::new(if is_finished {
                        "Informe a senha para salvar e iniciar a extração:"
                    } else {
                        "Defina a senha agora — ela será usada automaticamente assim que o download concluir:"
                    })
                    .small()
                    .color(to_c32(theme.text_muted)),
                );
                ui.add_space(6.0);

                let mut save_only = false;
                let mut save_and_extract = false;

                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.pw_edit_input)
                            .password(!self.pw_edit_show_chars)
                            .hint_text("Digite a senha (ou deixe vazio para limpar)...")
                            .desired_width(250.0),
                    );
                    if self.pw_edit_focus {
                        resp.request_focus();
                        self.pw_edit_focus = false;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if is_finished && !self.pw_edit_input.trim().is_empty() {
                            save_and_extract = true;
                        } else {
                            save_only = true;
                        }
                    }
                    if ui
                        .button(if self.pw_edit_show_chars {
                            "Ocultar"
                        } else {
                            "Mostrar"
                        })
                        .clicked()
                    {
                        self.pw_edit_show_chars = !self.pw_edit_show_chars;
                    }
                    if ui.button("Colar").clicked() {
                        if let Some(txt) = read_clipboard_text() {
                            self.pw_edit_input = txt.trim().to_string();
                        }
                    }
                });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancelar").clicked() {
                        self.pw_edit_target = None;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if is_finished && !self.pw_edit_input.trim().is_empty() {
                            if accent_button(ui, "Salvar e Extrair", theme).clicked() {
                                save_and_extract = true;
                            }
                            if ui.button("Apenas Salvar").clicked() {
                                save_only = true;
                            }
                        } else if accent_button(ui, "Salvar Senha", theme).clicked() {
                            save_only = true;
                        }
                    });
                });

                if save_only || save_and_extract {
                    let pw_opt = if self.pw_edit_input.trim().is_empty() {
                        None
                    } else {
                        Some(self.pw_edit_input.trim().to_string())
                    };
                    match target {
                        PasswordEditTarget::Download { id, .. } => {
                            self.manager.set_download_password(id, pw_opt);
                            if save_and_extract {
                                self.manager.trigger_manual_extract(id, None);
                            }
                        }
                        PasswordEditTarget::Package { id, .. } => {
                            self.manager.set_package_password(id, pw_opt.clone());
                            if save_and_extract {
                                self.manager.trigger_package_extract(id, pw_opt);
                            }
                        }
                    }
                    self.pw_edit_target = None;
                    self.pw_edit_input.clear();
                }
            });
    }

    fn render_new_torrent_modal(&mut self, ctx: &egui::Context, theme: &ThemeConfig) {
        if !self.show_new_torrent_modal {
            return;
        }
        egui::Window::new("Adicionar Torrent / Magnet")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(460.0);
                ui.label("Link Magnet ou caminho do arquivo .torrent:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.new_torrent_source).desired_width(350.0));
                    if ui.button("Arquivo .torrent").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("Torrent", &["torrent"])
                            .pick_file()
                        {
                            self.new_torrent_source = p.to_string_lossy().to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Salvar em:");
                    ui.add(egui::TextEdit::singleline(&mut self.new_torrent_dir).desired_width(300.0));
                    if ui.button("Procurar").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.new_torrent_dir = p.to_string_lossy().to_string();
                        }
                    }
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancelar").clicked() {
                        self.show_new_torrent_modal = false;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if accent_button(ui, "Adicionar Torrent", theme).clicked()
                            && !self.new_torrent_source.trim().is_empty()
                        {
                            let _ = self.manager.add_torrent_async(
                                self.new_torrent_source.trim(),
                                PathBuf::from(&self.new_torrent_dir),
                            );
                            self.new_torrent_source.clear();
                            self.show_new_torrent_modal = false;
                            self.active_tab = NavTab::Torrents;
                        }
                    });
                });
            });
    }
}

fn accent_button(ui: &mut Ui, text: impl Into<String>, theme: &ThemeConfig) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text.into()).strong().color(Color32::WHITE))
            .fill(to_c32(theme.accent)),
    )
}

fn nav_item(
    ui: &mut Ui,
    current: &mut NavTab,
    tab: NavTab,
    label: &str,
    badge: Option<usize>,
    theme: &ThemeConfig,
) {
    let selected = *current == tab;
    let id = ui.make_persistent_id(("nav_item", label));
    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
    let fill = if selected {
        let a = to_c32(theme.accent);
        Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), 42)
    } else if hovered {
        to_c32(theme.bg_hover)
    } else {
        Color32::TRANSPARENT
    };
    let text_color = if selected {
        to_c32(theme.accent)
    } else {
        to_c32(theme.text_primary)
    };

    let inner = Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(RichText::new(label).color(text_color).strong())
                        .selectable(false),
                );
                if let Some(count) = badge.filter(|&c| c > 0) {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        Frame::new()
                            .fill(to_c32(theme.bg_card))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(6, 1))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(count.to_string())
                                            .small()
                                            .color(to_c32(theme.text_muted)),
                                    )
                                    .selectable(false),
                                );
                            });
                    });
                }
            });
        });

    let resp = ui
        .interact(inner.response.rect, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    if resp.clicked() {
        *current = tab;
    }
}

fn pill(ui: &mut Ui, text: &str, color: Color32, _theme: &ThemeConfig) {
    let bg = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 32);
    Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0, Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 90)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().strong().color(color));
        });
}

fn render_segments_strip(ui: &mut Ui, item: &DownloadItem, theme: &ThemeConfig) {
    let Some(total) = item.total_bytes.filter(|&t| t > 0) else {
        return;
    };
    let desired = Vec2::new(ui.available_width(), 6.0);
    let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, to_c32(theme.bg_primary));
    let accent = to_c32(theme.accent);
    let success = to_c32(theme.success);
    for seg in &item.segments {
        let x0 = rect.left() + (seg.start as f32 / total as f32) * rect.width();
        let seg_len = seg.total_len().max(1) as f32;
        let done_w = (seg.downloaded as f32 / total as f32) * rect.width();
        let full_w = (seg_len / total as f32) * rect.width();
        let color = if seg.done { success } else { accent };
        let fill_rect = egui::Rect::from_min_size(
            egui::pos2(x0, rect.top()),
            Vec2::new(done_w.min(full_w).max(1.5), rect.height()),
        );
        painter.rect_filled(fill_rect, 2.0, color);
    }
}

fn render_speed_sparkline(ui: &mut Ui, snap: &ManagerSnapshot, theme: &ThemeConfig) {
    let max_bps = snap
        .speed_history
        .iter()
        .map(|s| s.download_bps.max(s.upload_bps))
        .max()
        .unwrap_or(0)
        .max(1024 * 100);

    Frame::new()
        .fill(to_c32(theme.bg_card))
        .stroke(Stroke::new(1.0, to_c32(theme.border)))
        .corner_radius(CornerRadius::same(theme.corner_radius as u8))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Tráfego em Tempo Real (60s)")
                        .small()
                        .strong()
                        .color(to_c32(theme.text_muted)),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("Pico escala: {}", format_speed(max_bps)))
                            .small()
                            .color(to_c32(theme.text_muted)),
                    );
                });
            });
            ui.add_space(4.0);
            let desired = Vec2::new(ui.available_width(), 46.0);
            let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 6.0, to_c32(theme.bg_primary));

            let n = snap.speed_history.len();
            if n >= 2 {
                let mut pts = Vec::with_capacity(n);
                for (i, sample) in snap.speed_history.iter().enumerate() {
                    let x = rect.left() + (i as f32 / (n - 1) as f32) * rect.width();
                    let norm = (sample.download_bps as f32 / max_bps as f32).clamp(0.0, 1.0);
                    let y = rect.bottom() - 3.0 - norm * (rect.height() - 6.0);
                    pts.push(egui::pos2(x, y));
                }
                painter.add(egui::Shape::line(pts, Stroke::new(2.0, to_c32(theme.success))));
            }
        });
}

fn render_empty_state(ui: &mut Ui, title: &str, subtitle: &str, theme: &ThemeConfig) {
    Frame::new()
        .fill(to_c32(theme.bg_card))
        .stroke(Stroke::new(1.0, to_c32(theme.border)))
        .corner_radius(CornerRadius::same(theme.corner_radius as u8))
        .inner_margin(Margin::same(28))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(title).strong().size(16.0));
                ui.add_space(6.0);
                ui.label(RichText::new(subtitle).color(to_c32(theme.text_muted)));
            });
        });
}

fn color_row(ui: &mut Ui, label: &str, rgba: &mut [u8; 4]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let mut c = to_c32(*rgba);
        if ui.color_edit_button_srgba(&mut c).changed() {
            *rgba = from_c32(c);
            changed = true;
        }
        ui.label(label);
    });
    changed
}

fn read_clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

fn site_badge_style(site: &str, theme: &ThemeConfig) -> (&'static str, Color32) {
    match site {
        "FileDitch" => ("FD", Color32::from_rgb(245, 158, 11)),
        "FileKeeper" => ("FK", Color32::from_rgb(56, 189, 248)),
        "Pixeldrain" => ("PD", Color32::from_rgb(16, 185, 129)),
        "Gofile" => ("GF", Color32::from_rgb(139, 92, 246)),
        "VikingFile" => ("VF", Color32::from_rgb(244, 63, 94)),
        "Buzzheavier" => ("BH", Color32::from_rgb(251, 146, 60)),
        "MediaFire" => ("MF", Color32::from_rgb(59, 130, 246)),
        "1fichier" => ("1F", Color32::from_rgb(234, 88, 12)),
        "MEGA" => ("MG", Color32::from_rgb(220, 38, 38)),
        "Akirabox" => ("AK", Color32::from_rgb(168, 85, 247)),
        "Datanodes" => ("DN", Color32::from_rgb(20, 184, 166)),
        "GitHub" => ("GH", Color32::from_rgb(148, 163, 184)),
        "Google Drive" => ("GD", Color32::from_rgb(34, 197, 94)),
        "Dropbox" => ("DB", Color32::from_rgb(37, 99, 235)),
        "OneDrive" => ("OD", Color32::from_rgb(14, 165, 233)),
        "HuggingFace" => ("HF", Color32::from_rgb(250, 204, 21)),
        "Archive.org" => ("AR", Color32::from_rgb(156, 163, 175)),
        "Magnet" => ("MAG", Color32::from_rgb(236, 72, 153)),
        _ => ("WEB", to_c32(theme.text_muted)),
    }
}

#[cfg(windows)]
fn bring_main_window_to_front() {
    unsafe extern "system" {
        fn FindWindowW(lpClassName: *const u16, lpWindowName: *const u16) -> isize;
        fn ShowWindow(hWnd: isize, nCmdShow: i32) -> i32;
        fn IsIconic(hWnd: isize) -> i32;
        fn SetForegroundWindow(hWnd: isize) -> i32;
        fn BringWindowToTop(hWnd: isize) -> i32;
    }
    const SW_RESTORE: i32 = 9;
    const SW_SHOW: i32 = 5;
    let title: Vec<u16> = "VortexDL — Next-Gen Rust Download Manager"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd != 0 {
            if IsIconic(hwnd) != 0 {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            } else {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

fn extractor_display_name(id: &str, list: &[ExtractorProfile]) -> String {
    if id == "auto" {
        return "Auto (Melhor)".to_string();
    }
    list.iter()
        .find(|e| e.id == id)
        .map(|e| e.name.clone())
        .unwrap_or_else(|| id.to_string())
}

fn extractor_combo_label(
    override_id: Option<&str>,
    default_id: &str,
    list: &[ExtractorProfile],
) -> String {
    let eff = override_id.unwrap_or(default_id);
    if eff == "auto" {
        return "Extrair: Auto".to_string();
    }
    let short = list
        .iter()
        .find(|p| p.id == eff)
        .map(|p| {
            if p.id == "windows_native" {
                "Win. Nativo"
            } else {
                p.name.as_str()
            }
        })
        .unwrap_or(eff);
    format!("Extrair: {short}")
}
