//! Dark theme from design-m5 §5.1: flat dark surfaces plus an indigo
//! accent, plain `egui::Visuals`/`Style` work — no custom renderer, no
//! shaders. The palette is the documented "approximate starting
//! palette"; tune visually against the reference (§5.1), the
//! general-Windows-11 rule (AGENTS §1) outranks pixel-matching.

use eframe::egui;

// Palette (design-m5 §5.1).
pub const BG: egui::Color32 = egui::Color32::from_rgb(0x0A, 0x0A, 0x0A);
pub const PANEL: egui::Color32 = egui::Color32::from_rgb(0x14, 0x14, 0x16);
pub const SURFACE: egui::Color32 = egui::Color32::from_rgb(0x1C, 0x1C, 0x20);
pub const BORDER: egui::Color32 = egui::Color32::from_rgb(0x2A, 0x2A, 0x2E);
pub const TEXT: egui::Color32 = egui::Color32::from_rgb(0xFA, 0xFA, 0xFA);
pub const MUTED: egui::Color32 = egui::Color32::from_rgb(0x9A, 0x9A, 0xA3);
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x5D, 0x5F, 0xEF);
pub const ACCENT_HOVER: egui::Color32 = egui::Color32::from_rgb(0x4F, 0x51, 0xD6);

// Status colours for the strip dot (UI-only; the daemon never sends
// colours — `status.mode` is a plain string, ipc.md §4).
pub const OK: egui::Color32 = egui::Color32::from_rgb(0x4C, 0xC7, 0x72);
pub const WARN: egui::Color32 = egui::Color32::from_rgb(0xE0, 0xA0, 0x3C);
pub const ERR: egui::Color32 = egui::Color32::from_rgb(0xE0, 0x5C, 0x5C);

/// Install the palette on the egui context — called once before the
/// first frame (design-m5 §5.1).
pub fn apply(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.window_stroke = egui::Stroke::new(1.0, BORDER);
    visuals.faint_bg_color = SURFACE;
    visuals.extreme_bg_color = SURFACE;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT_HOVER);

    // Widgets: surface fills, accent for the active/pressed state.
    visuals.widgets.inactive.bg_fill = SURFACE;
    visuals.widgets.inactive.weak_bg_fill = SURFACE;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT);
    visuals.widgets.hovered.bg_fill = BORDER;
    visuals.widgets.hovered.weak_bg_fill = BORDER;
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, TEXT);
    visuals.widgets.active.bg_fill = ACCENT_HOVER;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, TEXT);

    // egui 0.37 styles per `Theme` (the palette is dark-only, §5.1).
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals(visuals);

    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 5.0);
    });
}

/// A filled accent button for the primary actions (Add, Set as
/// wallpaper, Start engine).
pub fn accent_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(label).fill(ACCENT))
}

/// The small muted caption used for hints and empty states.
pub fn muted(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(MUTED));
}
