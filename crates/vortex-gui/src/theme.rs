use egui::{Color32, CornerRadius, FontId, Margin, Shadow, Stroke, Style, TextStyle, Visuals};
use vortex_core::model::ThemeConfig;

pub fn to_c32(rgba: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3])
}

pub fn from_c32(c: Color32) -> [u8; 4] {
    [c.r(), c.g(), c.b(), c.a()]
}

pub fn apply_theme(ctx: &egui::Context, theme: &ThemeConfig) {
    let mut visuals = if theme.dark_mode {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    let bg_primary = to_c32(theme.bg_primary);
    let bg_secondary = to_c32(theme.bg_secondary);
    let bg_card = to_c32(theme.bg_card);
    let bg_hover = to_c32(theme.bg_hover);
    let accent = to_c32(theme.accent);
    let text_primary = to_c32(theme.text_primary);
    let text_muted = to_c32(theme.text_muted);
    let border = to_c32(theme.border);
    let cr = CornerRadius::same(theme.corner_radius.round().clamp(0.0, 24.0) as u8);

    visuals.panel_fill = bg_primary;
    visuals.window_fill = bg_secondary;
    visuals.extreme_bg_color = bg_primary;
    visuals.faint_bg_color = bg_card;
    visuals.override_text_color = Some(text_primary);
    visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 140);
    visuals.selection.stroke = Stroke::new(1.0, accent);
    visuals.slider_trailing_fill = true;

    visuals.widgets.noninteractive.bg_fill = bg_card;
    visuals.widgets.noninteractive.weak_bg_fill = bg_secondary;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text_muted);
    visuals.widgets.noninteractive.corner_radius = cr;

    visuals.widgets.inactive.bg_fill = border;
    visuals.widgets.inactive.weak_bg_fill = bg_secondary;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, text_primary);
    visuals.widgets.inactive.corner_radius = cr;

    visuals.widgets.hovered.bg_fill = bg_hover;
    visuals.widgets.hovered.weak_bg_fill = bg_hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.2, text_primary);
    visuals.widgets.hovered.corner_radius = cr;

    visuals.widgets.active.bg_fill = accent;
    visuals.widgets.active.weak_bg_fill = accent;
    visuals.widgets.active.bg_stroke = Stroke::new(1.2, accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    visuals.widgets.active.corner_radius = cr;

    visuals.widgets.open.bg_fill = bg_hover;
    visuals.widgets.open.weak_bg_fill = bg_hover;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, accent);
    visuals.widgets.open.fg_stroke = Stroke::new(1.2, text_primary);
    visuals.widgets.open.corner_radius = cr;

    visuals.window_corner_radius = cr;
    visuals.window_stroke = Stroke::new(1.0, border);
    visuals.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(90),
    };

    let mut style = Style::default();
    style.visuals = visuals;
    style.interaction.selectable_labels = false;
    let s = theme.font_scale.clamp(0.8, 1.4);
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(11.5 * s)),
        (TextStyle::Body, FontId::proportional(13.5 * s)),
        (TextStyle::Button, FontId::proportional(13.0 * s)),
        (TextStyle::Heading, FontId::proportional(18.5 * s)),
        (TextStyle::Monospace, FontId::monospace(12.5 * s)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.window_margin = Margin::same(14);
    let egui_theme = if theme.dark_mode {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_style_of(egui_theme, style);
    ctx.set_theme(egui_theme);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_dropdown_visuals_follow_active_theme() {
        let ctx = egui::Context::default();
        let theme = ThemeConfig::catppuccin_mocha();
        apply_theme(&ctx, &theme);
        let style = ctx.style_of(egui::Theme::Dark);
        assert_eq!(style.visuals.widgets.open.bg_fill, to_c32(theme.bg_hover));
        assert_eq!(style.visuals.widgets.open.weak_bg_fill, to_c32(theme.bg_hover));
        assert_eq!(style.visuals.widgets.open.bg_stroke.color, to_c32(theme.accent));
        assert_eq!(style.visuals.widgets.open.fg_stroke.color, to_c32(theme.text_primary));
    }
}
