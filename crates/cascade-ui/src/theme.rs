//! Dark theme from: flat dark surfaces plus an indigo
//! accent, plain `egui::Visuals`/`Style` work — no custom renderer, no
//! shaders. The palette is the documented "approximate starting
//! palette"; tune visually against the reference, the
//! general-Windows-11 rule outranks pixel-matching.

use eframe::egui;
use egui::{
    class::HasClasses as _,
    theme::{DefaultStyle, StyleProvider},
    widget_style::{CheckboxStyle, StyleArgs, WidgetState},
};

// Palette.
pub const BG: egui::Color32 = egui::Color32::from_rgb(0x0A, 0x0A, 0x0A);
pub const PANEL: egui::Color32 = egui::Color32::from_rgb(0x14, 0x14, 0x16);
pub const SURFACE: egui::Color32 = egui::Color32::from_rgb(0x1C, 0x1C, 0x20);
pub const BORDER: egui::Color32 = egui::Color32::from_rgb(0x2A, 0x2A, 0x2E);
pub const TEXT: egui::Color32 = egui::Color32::from_rgb(0xFA, 0xFA, 0xFA);
pub const MUTED: egui::Color32 = egui::Color32::from_rgb(0x9A, 0x9A, 0xA3);
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x5D, 0x5F, 0xEF);
pub const ACCENT_HOVER: egui::Color32 = egui::Color32::from_rgb(0x4F, 0x51, 0xD6);
/// Tinted row behind the active sidebar tab — not a button fill.
pub const ACCENT_SOFT: egui::Color32 = egui::Color32::from_rgb(0x22, 0x24, 0x4A);
/// Class added to a checked checkbox so the style provider can fill it with
/// the accent colour.
const CHECKED_CLASS: &str = "cascade-checked";

// Status colours for notices (UI-only; the daemon never sends colours —
// `status.mode` is a plain string).
pub const WARN: egui::Color32 = egui::Color32::from_rgb(0xE0, 0xA0, 0x3C);
pub const ERR: egui::Color32 = egui::Color32::from_rgb(0xE0, 0x5C, 0x5C);

/// egui 0.37 derives the checkbox frame from interaction state, which has no
/// "checked" value. The class added by `checkbox` carries that state here.
struct AccentCheckbox;

impl StyleProvider<CheckboxStyle> for AccentCheckbox {
    fn style(&mut self, args: &StyleArgs<'_>) -> CheckboxStyle {
        let mut style: CheckboxStyle = DefaultStyle.style(args);
        if args.has_class(CHECKED_CLASS) {
            let fill = if matches!(args.state, WidgetState::Hovered | WidgetState::Active) {
                ACCENT_HOVER
            } else {
                ACCENT
            };
            style.checkbox_frame.fill = fill;
            style.checkbox_frame.stroke = egui::Stroke::new(1.0, fill);
            style.check_stroke = egui::Stroke::new(2.0, TEXT);
        }
        style
    }
}

/// Install OS font fallbacks so titles outside ASCII (Japanese, Chinese,
/// Korean, and other non-Latin scripts) render instead of showing tofu
/// boxes. egui's bundled font only covers Latin + Cyrillic; the fallback
/// list is appended *after* it, so Latin text keeps its current look and
/// the system face is used only when a glyph is missing (refinement).
///
/// Files come from `%WINDIR%\Fonts` via `std` path probing only — no font
/// is bundled with the binary, no new dependency, and no `cascade-win`
/// code in the UI process. A missing file is simply skipped; the app
/// still runs with egui's default font if no CJK face is installed.
pub fn install_system_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontData, FontFamily, FontInsert, FontPriority, InsertFontFamily};

    let fonts_dir = std::env::var_os("WINDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("Fonts");

    // Broad CJK (YaHei) first, then Japanese (Yu Gothic) and Korean
    // (Malgun Gothic). `Lowest` makes each a fallback rather than
    // replacing the primary Latin face.
    let candidates: &[(&str, &str)] = &[
        ("cascade-cjk-yahei", "msyh.ttc"),
        ("cascade-cjk-yugoth", "YuGothM.ttc"),
        ("cascade-cjk-malgun", "malgun.ttf"),
    ];
    let families = || {
        vec![
            InsertFontFamily {
                family: FontFamily::Proportional,
                priority: FontPriority::Lowest,
            },
            InsertFontFamily {
                family: FontFamily::Monospace,
                priority: FontPriority::Lowest,
            },
        ]
    };

    for &(name, file) in candidates {
        let Ok(bytes) = std::fs::read(fonts_dir.join(file)) else {
            continue;
        };
        ctx.add_font(FontInsert::new(
            name,
            FontData::from_owned(bytes),
            families(),
        ));
    }
}

/// Install the palette on the egui context — called once before the
/// first frame.
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

    // egui 0.37 styles per `Theme` (the palette is dark-only).
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals(visuals);
    ctx.replace_widget_theme::<CheckboxStyle>(AccentCheckbox);

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

/// A checkbox whose checked frame uses the accent palette.
pub fn checkbox(ui: &mut egui::Ui, checked: &mut bool, label: &str) -> egui::Response {
    let is_checked = *checked;
    ui.add(egui::Checkbox::new(checked, label).with_class_if(CHECKED_CLASS, is_checked))
}

/// The small muted caption used for hints and empty states.
pub fn muted(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(MUTED));
}
