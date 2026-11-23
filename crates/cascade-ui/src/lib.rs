//! `cascade ui` — the library window.
//!
//! A **separate process** from the daemon: it talks to the
//! engine only over the named pipe via `cascade_core::client`, and never
//! touches the host window, the wallpaper, or the media engine. Closing
//! the window ends this process; the daemon keeps running untouched —
//! that isolation is the exit criterion.

mod app;
mod dialog;
/// Window/taskbar icon decoded from the tracked PNG, UI process only
///
mod icon;
mod theme;
/// Thumbnail worker: disk cache, decode thread, texture LRU
/// UI-process only — the daemon never links it.
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
    // 980×590 default, 640×400 floor in **logical** points. These are
    // usability choices, not monitor assumptions: eframe/winit applies
    // per-monitor DPI scaling, the window can be dragged to any monitor,
    // and its geometry persists in platform storage.
    // forbids tuning to one desktop size, so no target resolution or
    // taskbar coordinate is encoded here.
    // winit leaves the window class icon null; without this the window
    // and its taskbar button show Windows' generic icon.
    let icon = icon::window_icon()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 590.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title("Cascade")
            .with_icon(icon),
        // glow, not wgpu — a lighter renderer on integrated GPUs.
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "cascade",
        options,
        Box::new(|cc| {
            // OS CJK fallbacks must be registered before the first frame
            // so non-ASCII library titles get real glyphs, not tofu
            // (refinement). No bundled font, no UI→daemon coupling.
            theme::install_system_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            // The reader thread needs the same context to request a
            // frame when an event arrives.
            let app = CascadeApp::new(cc.egui_ctx.clone())
                .map_err(Box::<dyn std::error::Error + Send + Sync>::from)?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|err| format!("could not open the window: {err}"))
}
