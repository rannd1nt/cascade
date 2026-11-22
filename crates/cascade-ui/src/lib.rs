//! `cascade ui` — the library window (design-m5 §5, D11).
//!
//! A **separate process** from the daemon (AGENTS §3.4): it talks to the
//! engine only over the named pipe via `cascade_core::client`, and never
//! touches the host window, the wallpaper, or the media engine. Closing
//! the window ends this process; the daemon keeps running untouched —
//! that isolation is the M5 exit criterion (§11).

mod app;
mod dialog;
mod theme;
/// Thumbnail worker: disk cache, decode thread, texture LRU
/// (design-m5-p4 §11). UI-process only — the daemon never links it.
mod thumbs;

use eframe::egui;

pub use app::CascadeApp;

/// Run the library window until it is closed. Called from the binary's
/// `ui` / `--ui` arm — `cascade ui` and a double-click on the exe (bare
/// `--ui`, `is_gui_role` in the binary) both land here.
///
/// Errors are plain strings: they print on the CLI like every other
/// command (`main.rs` maps them into its `Result<ExitCode, String>`).
pub fn run() -> Result<(), String> {
    // 980×590 default, 640×400 floor: the grid needs room to scan a
    // list, but the window must stay usable on the smallest supported
    // desktop (AGENTS §1 — no resolution assumptions; the size persists
    // in the platform storage, D4). 590 keeps the status strip clear of
    // the taskbar on a 1366×768 machine: measured 2026-11-12, default
    // 640 opened at y=78 and its bottom (757) sat behind the taskbar
    // (top 728).
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 590.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title("Cascade"),
        // D5: glow, not wgpu — a lighter renderer on integrated GPUs.
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "cascade",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            // The reader thread needs the same context to request a
            // frame when an event arrives (design-m5-p4 §5).
            let app = CascadeApp::new(cc.egui_ctx.clone())
                .map_err(Box::<dyn std::error::Error + Send + Sync>::from)?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|err| format!("could not open the window: {err}"))
}
