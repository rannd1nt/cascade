//! `cascade` — role dispatch only.
//!
//! All Win32 behaviour lives in `cascade-win`; this binary parses argv and
//! reports results. See `docs/architecture.md` §2.

// GUI subsystem: starting the daemon must never create a console window
// (M1-F, measured flash of ~100 ms with focus steal on a console build).
// The CLI re-attaches to a console in `console::prepare_stdio`.
#![windows_subsystem = "windows"]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cascade_core::assess::{codec_key, Recommendation, Severity, Warning};
use cascade_core::device::{level_name, profile_name};
use cascade_core::ipc::{
    no_data, AssessReport, AssessRequest, AutostartStatus, EnabledData, LibraryAddData,
    LibraryAddResult, LibraryItemsData, LibraryPruneResult, LibraryRemoveData, MonitorData,
    PausePatch, SetData, SetReport, Settings, SettingsPatch,
};
use cascade_core::library::{dedupe_key, LibraryItem};
use cascade_core::rule_engine::{BatteryAction, PowerProfile};
use cascade_core::MediaBackend;
use cascade_media::{MediaEngineBackend, SourceReaderBackend};
use cascade_win::{console, control, daemon, doctor, dpi, host, install, ipc, registry, state};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    dpi::set_per_monitor_v2();
    // The bare form decides its own stdio strategy *after* the install
    // prompt decision (design-m4 §6.2); every other role prepares stdio
    // first — every path below may print.
    let result = if args.is_empty() {
        bare_main()
    } else {
        console::prepare_stdio(is_daemon_role(&args));
        dispatch(&args)
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            eprintln!("cascade: {message}");
            ExitCode::from(2)
        }
    }
}

/// What a bare `cascade` (no arguments) should do (design-m4 §6.2, D5).
/// Pure so the decision is unit-testable without a window station.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BareAction {
    /// Internal respawn child of a bare-started daemon: run, stay silent.
    RespawnRun,
    /// Not installed: ask before installing (double-click or typed).
    PromptInstall,
    /// Installed and a console exists (typed command): run the engine.
    Run,
    /// Installed but double-clicked: usage only, never start unprompted.
    Usage,
}

fn bare_action(installed: bool, have_console: bool, respawn: bool) -> BareAction {
    if respawn {
        BareAction::RespawnRun
    } else if !installed {
        BareAction::PromptInstall
    } else if have_console {
        BareAction::Run
    } else {
        BareAction::Usage
    }
}

/// The bare-invocation flow. Unlike every other role this one may show a
/// dialog *before* stdio exists — allocating a console first would flash a
/// window next to the prompt (design-m4 §6.2).
fn bare_main() -> Result<ExitCode, String> {
    let respawn = std::env::var_os("CASCADE_RESPAWN").is_some();
    let action = bare_action(
        install::is_installed_here(),
        console::have_console(),
        respawn,
    );
    match action {
        // The internal respawn keeps the silent daemon strategy (M1-F:
        // never allocate a console for the long-lived role).
        BareAction::RespawnRun => {
            console::prepare_stdio(true);
            cmd_run(&[])
        }
        BareAction::Run => {
            console::prepare_stdio(false);
            cmd_run(&[])
        }
        BareAction::Usage => {
            console::prepare_stdio(false);
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        BareAction::PromptInstall => {
            if install::confirm_install() {
                console::prepare_stdio(false);
                cmd_install(&[])
            } else {
                console::prepare_stdio(false);
                print_help();
                Ok(ExitCode::SUCCESS)
            }
        }
    }
}

/// `run` and `--autostart` start the long-lived daemon; everything else is
/// a one-shot CLI command that must print. The bare form is decided in
/// [`bare_main`] before this is reached.
fn is_daemon_role(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        None | Some("run") | Some("--autostart")
    )
}

fn dispatch(args: &[String]) -> Result<ExitCode, String> {
    match args.first().map(String::as_str) {
        None => cmd_run(&[]),
        Some("run") => cmd_run(&args[1..]),
        Some("--autostart") => cmd_run(&args[1..]),
        Some("doctor") => cmd_doctor(),
        Some("stop") => cmd_signal(control::WM_CASCADE_STOP, "stop"),
        Some("restore") => cmd_restore(),
        Some("status") => cmd_status(&args[1..]),
        Some("pause") => cmd_playback(true, &args[1..]),
        Some("resume") => cmd_playback(false, &args[1..]),
        Some("set") => cmd_set(&args[1..]),
        Some("clear") => cmd_clear(&args[1..]),
        Some("quit") => cmd_quit(&args[1..]),
        Some("settings") => cmd_settings(&args[1..]),
        Some("library") => cmd_library(&args[1..]),
        Some("assess") => cmd_assess(&args[1..]),
        Some("autostart") => cmd_autostart(&args[1..]),
        Some("install") => cmd_install(&args[1..]),
        Some("uninstall") => cmd_uninstall(&args[1..]),
        Some("version") | Some("--version") | Some("-V") => {
            println!("cascade {VERSION}");
            Ok(ExitCode::SUCCESS)
        }
        Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        Some("ui") | Some("--ui") => {
            println!("The library window is not implemented yet (milestone M5).");
            println!("Use `cascade run` for the engine and `cascade doctor` for diagnostics.");
            Ok(ExitCode::SUCCESS)
        }
        Some(other) => {
            print_help();
            Err(format!("unknown command: {other}"))
        }
    }
}

fn cmd_run(args: &[String]) -> Result<ExitCode, String> {
    let options = parse_run_options(args)?;

    // A backend is only worth constructing when a source exists
    // (design-m2 §2.2): `--video` wins, otherwise the last successful path
    // replays. The daemon resolves the same pair again and loads it.
    let source = options.video.clone().or_else(|| {
        state::Store::open()
            .load()
            .and_then(|previous| previous.video)
            .map(PathBuf::from)
    });
    let backend = build_backend(source.as_deref());
    // `cascade-win` may not depend on `cascade-media` (AGENTS §4): the
    // daemon builds later backends (a first `wallpaper.set` on a videoless
    // start) through this factory instead.
    let factory: daemon::BackendFactory = Box::new(|path: &Path| build_backend(Some(path)));
    // The library probe travels the same way (design-m4 §4): win may not
    // depend on media, so the binary injects it.
    let probe: daemon::ProbeFn = Box::new(cascade_media::probe);
    match daemon::run(options, backend, factory, probe) {
        Ok(daemon::RunExit::Exit) => Ok(ExitCode::SUCCESS),
        // A different-file `wallpaper.set` was answered: the exit ladder
        // has repainted and `state.json` holds the new path; re-exec so
        // the child loads it with flat memory (design-m2 §4).
        Ok(daemon::RunExit::Respawn) => respawn_child(),
        Err(err) => {
            // Best effort: a console close may already have invalidated the
            // handles, and a panic here would be reported as a crash.
            cascade_win::derr!("cascade: {err}");
            Ok(ExitCode::from(1))
        }
    }
}

/// Re-exec this binary after a respawn-triggering `wallpaper.set`
/// (design-m2 §4). The child inherits everything except `--video` (it
/// must read the new path from `state.json`, not the flag that started
/// this process) and sets `CASCADE_RESPAWN` so it retries the session
/// pipe until this process exits. A failed spawn leaves a repainted
/// desktop and a correct `state.json` — the next start loads the new
/// file anyway; the error still surfaces as a non-zero exit.
fn respawn_child() -> Result<ExitCode, String> {
    let exe = std::env::current_exe().map_err(|err| format!("respawn: {err}"))?;
    match std::process::Command::new(exe)
        .args(respawn_args())
        .env("CASCADE_RESPAWN", "1")
        .spawn()
    {
        Ok(child) => {
            cascade_win::dlog!("cascade: respawned as pid {}", child.id());
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => Err(format!("respawn: {err}")),
    }
}

/// The respawn argv: this process's arguments minus `--video` and its
/// value. Pure so the filter is unit-testable without touching argv.
fn respawn_args() -> Vec<std::ffi::OsString> {
    filter_video_flag(std::env::args_os().skip(1).collect())
}

fn filter_video_flag(raw: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut out = Vec::with_capacity(raw.len());
    let mut skip_next = false;
    for arg in raw {
        if skip_next {
            skip_next = false;
        } else if arg == "--video" {
            skip_next = true;
        } else {
            out.push(arg);
        }
    }
    out
}

/// Parse `run`/`--autostart` options. Kept free of side effects so the
/// `--video` handling is unit-testable without starting a daemon.
fn parse_run_options(args: &[String]) -> Result<daemon::DaemonOptions, String> {
    let mut options = daemon::DaemonOptions::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--color" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--color needs a value like 187A86".to_string())?;
                options.color = parse_color(value)?;
                index += 2;
            }
            "--repaint" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--repaint needs a level 1-4".to_string())?;
                let level: u32 = value
                    .parse()
                    .map_err(|_| format!("--repaint expects 1-4, got {value}"))?;
                if !(1..=4).contains(&level) {
                    return Err(format!("--repaint expects 1-4, got {level}"));
                }
                options.repaint_level = level;
                index += 2;
            }
            "--host" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--host needs auto, workerw, or layered".to_string())?;
                options.host_mode = host::HostModePreference::parse(value)
                    .ok_or_else(|| format!("--host expects auto|workerw|layered, got {value}"))?;
                index += 2;
            }
            "--video" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--video needs a path to a video file".to_string())?;
                options.video = Some(PathBuf::from(value));
                index += 2;
            }
            other => return Err(format!("unknown option for run: {other}")),
        }
    }
    Ok(options)
}

/// Build the media backend.
///
/// Default (no env): slice 2's Media Engine backend first. It is the
/// lightest measured video configuration (120.9 MB private bytes, R1 in
/// docs/rnd-tier2-step5-measurement.md Appendix G) and runs at ~1.2% CPU
/// with hardware decode on the target machine. The plain SourceReader CPU
/// path must not ship as the default: live-measured 2026-11-07 at
/// 11.5-12.0% CPU (≈140% of one core), 0% GPU decode and 247 MB private
/// bytes — the E3a gate decision, re-confirmed (Appendix D and G).
///
/// Opt-in: any `CASCADE_TIER2*` env var selects the Tier 2 SourceReader
/// first (RND experiments run through `deploy-tier2.ps1`, which sets
/// them). `CASCADE_TIER2_E3B=1` additionally enables the E3b GPU path
/// (measured 0.90-1.56% CPU, 172.8 MB private bytes) — frozen as an
/// opt-in experiment (design-m4.md §1, decision D1): re-measured only if
/// MediaEngine fails in the field. A construction
/// failure falls through to the next backend; a failure of both degrades
/// to the colour background instead of refusing to start (AGENTS §3.1).
fn build_backend(source: Option<&Path>) -> Option<Box<dyn MediaBackend>> {
    let _ = source?;
    let tier2_opted_in =
        std::env::vars_os().any(|(key, _)| key.to_string_lossy().starts_with("CASCADE_TIER2"));

    let source_reader = || match SourceReaderBackend::new() {
        Ok(backend) => {
            // Log the path that will actually run: without E3b the reader
            // decodes and converts on the CPU (measured 11.5-12% CPU).
            if std::env::var_os("CASCADE_TIER2_E3B").is_some() {
                cascade_win::dlog!("cascade: media backend: SourceReader (Tier 2 E3b)");
            } else {
                cascade_win::dlog!("cascade: media backend: SourceReader (Tier 2 CPU path)");
            }
            Some(Box::new(backend) as Box<dyn MediaBackend>)
        }
        Err(err) => {
            cascade_win::dlog!("cascade: SourceReader backend unavailable: {err}");
            None
        }
    };
    let media_engine = || match MediaEngineBackend::new() {
        Ok(backend) => {
            cascade_win::dlog!("cascade: media backend: MediaEngine (slice 2)");
            Some(Box::new(backend) as Box<dyn MediaBackend>)
        }
        Err(err) => {
            cascade_win::derr!("cascade: media backend unavailable: {err}");
            None
        }
    };

    if tier2_opted_in {
        source_reader().or_else(media_engine)
    } else {
        media_engine().or_else(source_reader)
    }
}

fn parse_color(text: &str) -> Result<u32, String> {
    let trimmed = text.trim().trim_start_matches('#');
    let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let value = u32::from_str_radix(trimmed, 16)
        .map_err(|_| format!("{text} is not a hex colour like 187A86"))?;
    if value > 0x00FF_FFFF {
        return Err(format!("{text} is out of range (expected RRGGBB)"));
    }
    let r = ((value >> 16) & 0xFF) as u8;
    let g = ((value >> 8) & 0xFF) as u8;
    let b = (value & 0xFF) as u8;
    Ok(host::rgb(r, g, b))
}

fn cmd_doctor() -> Result<ExitCode, String> {
    let report = doctor::collect(VERSION);
    report.print();
    Ok(ExitCode::SUCCESS)
}

/// `cascade install` — copy this exe to `%LOCALAPPDATA%\Cascade`, generate
/// the `cascade.cmd` shim, update the user PATH, enable autostart, and
/// start the engine (design-m4 §6.1). Runs from wherever the exe is.
fn cmd_install(args: &[String]) -> Result<ExitCode, String> {
    if !args.is_empty() {
        return Err(format!(
            "install takes no arguments (got: {})",
            args.join(" ")
        ));
    }
    let source = std::env::current_exe().map_err(|err| format!("install: {err}"))?;
    install::install_from(&source)?;
    Ok(ExitCode::SUCCESS)
}

/// `cascade uninstall [--purge]` — reverse the install (design-m4 §6.1).
/// Without `--purge`, `library.json` and `state.json` are kept.
fn cmd_uninstall(args: &[String]) -> Result<ExitCode, String> {
    let purge = uninstall_options(args)?;
    install::uninstall(purge)?;
    Ok(ExitCode::SUCCESS)
}

/// Parse `uninstall` options. Pure so the flag handling is unit-testable.
fn uninstall_options(args: &[String]) -> Result<bool, String> {
    let mut purge = false;
    for arg in args {
        match arg.as_str() {
            "--purge" => purge = true,
            other => {
                return Err(format!(
                    "uninstall: unknown option {other} (expected --purge)"
                ))
            }
        }
    }
    Ok(purge)
}

fn cmd_signal(message: u32, label: &str) -> Result<ExitCode, String> {
    match control::find_existing() {
        Some(hwnd) => {
            if control::post(hwnd, message) {
                println!("{label}: requested");
                Ok(ExitCode::SUCCESS)
            } else {
                Err(format!("{label}: could not reach the running daemon"))
            }
        }
        None => Err(format!("{label}: Cascade is not running")),
    }
}

/// `cascade status [--json]` — one request over the control pipe
/// (`ipc.md` §7). Exit code 1 when the engine is not reachable, so scripts
/// can branch on it.
fn cmd_status(args: &[String]) -> Result<ExitCode, String> {
    let json = status_options(args)?;
    match ipc_client_status() {
        Ok(status) => {
            if json {
                // `to_string_pretty` cannot fail on a Value built here.
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status)
                        .map_err(|err| format!("could not format status: {err}"))?
                );
            } else {
                print_status_summary(&status);
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(message) => Err(message),
    }
}

/// Request the `status` payload and decode it into the shared type.
fn ipc_client_status() -> Result<cascade_core::ipc::Status, String> {
    let data = cascade_win::ipc::request("status", cascade_core::ipc::no_data())
        .map_err(|err| err.to_string())?;
    serde_json::from_value(data).map_err(|err| format!("engine sent an unreadable status: {err}"))
}

/// Parse `status` flags. Kept pure for unit testing.
fn status_options(args: &[String]) -> Result<bool, String> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            other => return Err(format!("unknown option for status: {other}")),
        }
    }
    Ok(json)
}

fn print_status_summary(status: &cascade_core::ipc::Status) {
    println!("engine: {}", status.engine);
    println!("mode: {}", status.mode);
    if !status.paused_reasons.is_empty() {
        println!("paused because: {}", status.paused_reasons.join(", "));
    }
    for monitor in &status.monitors {
        let rect = monitor.rect;
        let primary = if monitor.primary { " (primary)" } else { "" };
        let source = monitor
            .source
            .as_ref()
            .map(|source| format!(" — {}", source.title))
            .unwrap_or_default();
        println!(
            "  {} {w}x{h} at {x},{y}{primary}{source}",
            monitor.id,
            w = rect.w,
            h = rect.h,
            x = rect.x,
            y = rect.y,
        );
        // Playback counters exist only on the monitor the engine is
        // attached to (M4 S3b); unknown values are omitted on the wire and
        // therefore here too — never printed as a fake zero.
        let stats = [
            monitor
                .hwdec
                .map(|on| format!("hwdec: {}", if on { "yes" } else { "no" })),
            monitor.fps.map(|fps| format!("fps: {fps:.2}")),
            monitor.dropped.map(|dropped| format!("dropped: {dropped}")),
            monitor
                .video
                .map(|video| format!("video: {}x{}", video.w, video.h)),
        ];
        let present: Vec<String> = stats.into_iter().flatten().collect();
        if !present.is_empty() {
            println!("      {}", present.join(", "));
        }
    }
    println!(
        "host: {} (repaint level {})",
        status.host.strategy, status.host.repaint_level
    );
    println!(
        "memory: {} MB working set, {} MB private",
        status.memory.working_set_mb, status.memory.private_mb
    );
    println!(
        "autostart: {}{}",
        if status.autostart.enabled {
            "enabled"
        } else {
            "disabled"
        },
        if status.autostart.windows_disabled {
            " (disabled in Windows Settings)"
        } else {
            ""
        }
    );
}

fn cmd_autostart(args: &[String]) -> Result<ExitCode, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot resolve the current executable: {err}"))?;

    match args.first().map(String::as_str) {
        Some("on") => {
            registry::autostart_enable(&exe)
                .map_err(|code| format!("could not write the Run key (error {code})"))?;
            println!("autostart: enabled");
            println!("  {}", registry::autostart_command(&exe));
        }
        Some("off") => {
            registry::autostart_disable()
                .map_err(|code| format!("could not remove the Run value (error {code})"))?;
            println!("autostart: disabled");
        }
        Some("status") | None => {
            let state = registry::autostart_state(&exe);
            println!(
                "autostart: {}",
                if state.registered {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            if let Some(command) = state.command {
                println!("  command: {command}");
            }
            if state.registered && !state.points_to_current_exe {
                println!("  warning: the entry points to a different executable");
            }
            if state.windows_disabled {
                println!("  warning: disabled in Settings > Apps > Startup");
            }
        }
        Some(other) => return Err(format!("autostart expects on|off|status, got {other}")),
    }
    Ok(ExitCode::SUCCESS)
}

// --- Control commands (M4 S2, ipc.md §3) ------------------------------------

/// `cascade restore` — repaint through the pipe, with the window message as
/// the fallback for a daemon that predates `wallpaper.restore`.
fn cmd_restore() -> Result<ExitCode, String> {
    match ipc::request("wallpaper.restore", no_data()) {
        Ok(_) => {
            println!("restore: requested");
            Ok(ExitCode::SUCCESS)
        }
        // The daemon is not running at all: say so instead of pretending.
        Err(ipc::ClientError::NotRunning) => Err("Cascade is not running".to_string()),
        // Busy, timeout, an older daemon without the command: the window
        // post has always worked, so fall back to it.
        Err(_) => cmd_signal(control::WM_CASCADE_RESTORE, "restore"),
    }
}

/// `cascade pause` / `cascade resume` — the user pause toggle (design-m4
/// §2.6): runtime-only, never written to `config.toml`.
fn cmd_playback(paused: bool, args: &[String]) -> Result<ExitCode, String> {
    let label = if paused { "pause" } else { "resume" };
    if !args.is_empty() {
        return Err(format!("{label} takes no arguments"));
    }
    let cmd = if paused {
        "playback.pause"
    } else {
        "playback.resume"
    };
    ipc::request(cmd, no_data()).map_err(|err| err.to_string())?;
    println!("{label}d");
    Ok(ExitCode::SUCCESS)
}

/// What `set` accepted: exactly one file plus an optional monitor target.
/// Pure syntax — file existence is the daemon's job (`file_missing`,
/// checked before any playback surgery).
#[derive(Debug)]
struct SetOptions {
    /// Exactly one of `path` / `id` reaches the wire (`ipc.md` §3).
    path: Option<PathBuf>,
    id: Option<String>,
    monitor: Option<String>,
}

fn parse_set_options(args: &[String]) -> Result<SetOptions, String> {
    let mut file: Option<PathBuf> = None;
    let mut item_id: Option<String> = None;
    let mut monitor: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--monitor" => {
                if monitor.is_some() {
                    return Err("--monitor given twice".to_string());
                }
                let value = args.get(index + 1).ok_or_else(|| {
                    "--monitor needs a monitor id like \\\\.\\DISPLAY1".to_string()
                })?;
                monitor = Some(value.clone());
                index += 2;
            }
            "--id" => {
                if item_id.is_some() {
                    return Err("--id given twice".to_string());
                }
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--id needs a library id like a3f2".to_string())?;
                if !is_item_id(value) {
                    return Err(format!("'{value}' is not a library id (4 hex characters)"));
                }
                item_id = Some(value.clone());
                index += 2;
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown option for set: {other}"));
            }
            other => {
                if file.is_some() {
                    return Err(format!(
                        "set takes one video file, got a second argument: {other}"
                    ));
                }
                file = Some(PathBuf::from(other));
                index += 1;
            }
        }
    }
    match (file, item_id) {
        (None, None) => Err(
            "set needs a path or a library id: `cascade set <path> [--id <a3f2>] [--monitor <id>]`"
                .to_string(),
        ),
        (Some(_), Some(_)) => {
            Err("set takes exactly one of a path or --id (a library item), not both".to_string())
        }
        (path, id) => Ok(SetOptions { path, id, monitor }),
    }
}

/// `cascade set <path> | --id <a3f2> [--monitor <id>]` — play a file
/// directly or a library item (design-m4 S2 path, S3 id). The path is made
/// absolute here: the daemon's working directory is not the CLI's.
fn cmd_set(args: &[String]) -> Result<ExitCode, String> {
    let options = parse_set_options(args)?;
    // Exactly one of path / id goes out (`ipc.md` §3).
    let (data, label) = if let Some(item_id) = options.id {
        (
            SetData {
                path: None,
                id: Some(item_id.clone()),
                monitor: options.monitor,
            },
            format!("playing library item {item_id}"),
        )
    } else {
        let path = options.path.expect("parser guarantees one of the two");
        let absolute = std::path::absolute(&path)
            .map_err(|err| format!("cannot resolve {}: {err}", path.display()))?;
        (
            SetData {
                path: Some(absolute.display().to_string()),
                id: None,
                monitor: options.monitor,
            },
            format!("playing: {}", absolute.display()),
        )
    };
    let payload =
        serde_json::to_value(data).map_err(|err| format!("could not build request: {err}"))?;
    let data = ipc::request("wallpaper.set", payload).map_err(|err| err.to_string())?;
    println!("{label}");
    // S5 (design-m4 §5.3): the daemon assessed before answering — a
    // rejection (`error`) already surfaced through the Err above; what
    // comes back here is the honest print-then-apply part.
    if let Ok(report) = serde_json::from_value::<SetReport>(data) {
        print_warnings(&report.warnings);
        if !report.warnings.is_empty() {
            if let Some(rec) = &report.recommendation {
                print_recommendation(rec);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Parse an optional `--monitor <id>` flag and nothing else (`clear`).
fn parse_monitor_option(args: &[String]) -> Result<Option<String>, String> {
    let mut monitor: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--monitor" => {
                if monitor.is_some() {
                    return Err("--monitor given twice".to_string());
                }
                let value = args.get(index + 1).ok_or_else(|| {
                    "--monitor needs a monitor id like \\\\.\\DISPLAY1".to_string()
                })?;
                monitor = Some(value.clone());
                index += 2;
            }
            other => return Err(format!("unknown option for clear: {other}")),
        }
    }
    Ok(monitor)
}

/// `cascade clear [--monitor <id>]` — back to the colour background. User
/// files are never touched (AGENTS §3.3).
fn cmd_clear(args: &[String]) -> Result<ExitCode, String> {
    let monitor = parse_monitor_option(args)?;
    let payload = serde_json::to_value(MonitorData { monitor })
        .map_err(|err| format!("could not build request: {err}"))?;
    ipc::request("wallpaper.clear", payload).map_err(|err| err.to_string())?;
    println!("cleared; the desktop keeps its colour background");
    Ok(ExitCode::SUCCESS)
}

/// `cascade quit` — graceful exit over the pipe only (design-m4 §2.3): the
/// response is flushed before the exit ladder starts. `cascade stop` stays
/// the window-based escape hatch.
fn cmd_quit(args: &[String]) -> Result<ExitCode, String> {
    if !args.is_empty() {
        return Err("quit takes no arguments".to_string());
    }
    match ipc::request("quit", no_data()) {
        Ok(_) => {
            println!("stopping");
            Ok(ExitCode::SUCCESS)
        }
        Err(ipc::ClientError::NotRunning) => Err("Cascade is not running".to_string()),
        Err(ipc::ClientError::Busy) => {
            Err("the engine is busy — retry, or use `cascade stop`".to_string())
        }
        Err(err) => Err(err.to_string()),
    }
}

/// `cascade settings get|set` (`ipc.md` §5).
fn cmd_settings(args: &[String]) -> Result<ExitCode, String> {
    match args.first().map(String::as_str) {
        Some("get") => settings_get(&args[1..]),
        Some("set") => settings_set(&args[1..]),
        Some(other) => Err(format!("settings expects get|set, got {other}")),
        None => Err("settings expects get or set — see `cascade help`".to_string()),
    }
}

fn settings_get(args: &[String]) -> Result<ExitCode, String> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            other => return Err(format!("unknown option for settings get: {other}")),
        }
    }
    let data = ipc::request("settings.get", no_data()).map_err(|err| err.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&data)
                .map_err(|err| format!("could not format settings: {err}"))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    let settings: Settings = serde_json::from_value(data)
        .map_err(|err| format!("engine sent unreadable settings: {err}"))?;
    print_settings(&settings);
    Ok(ExitCode::SUCCESS)
}

fn print_settings(settings: &Settings) {
    println!("power_profile: {}", settings.power_profile.as_str());
    println!("on_battery: {}", settings.on_battery.as_str());
    println!("pause.fullscreen: {}", settings.pause.fullscreen);
    println!("pause.maximized: {}", settings.pause.maximized);
    println!("pause.display_off: {}", settings.pause.display_off);
    println!("pause.session_lock: {}", settings.pause.session_lock);
    println!("pause.screensaver: {}", settings.pause.screensaver);
    println!("pause.remote_session: {}", settings.pause.remote_session);
    println!(
        "restore_wallpaper_on_exit: {}",
        settings.restore_wallpaper_on_exit
    );
    println!("tray: {}", settings.tray);
    println!(
        "autostart: {}",
        if settings.autostart { "on" } else { "off" }
    );
}

/// `cascade settings set <key> <value>` — exactly two arguments, a strictly
/// validated key (`design-m4` §3: unknown key → error, never ignored).
fn settings_set(args: &[String]) -> Result<ExitCode, String> {
    if args.len() != 2 {
        return Err(
            "settings set needs <key> <value> — see `cascade help` for the keys".to_string(),
        );
    }
    let key = args[0].as_str();
    let value = args[1].as_str();

    // `autostart` is not file backed (design-m4 D10): the Run key is the
    // oracle, so it travels as its own command instead of a settings patch.
    if key == "autostart" {
        let enabled = parse_bool(value)?;
        let payload = serde_json::to_value(EnabledData { enabled })
            .map_err(|err| format!("could not build request: {err}"))?;
        let data = ipc::request("autostart.set", payload).map_err(|err| err.to_string())?;
        let status: AutostartStatus = serde_json::from_value(data)
            .map_err(|err| format!("engine sent an unreadable autostart status: {err}"))?;
        println!("autostart = {}", if status.enabled { "on" } else { "off" });
        if status.windows_disabled {
            println!("  warning: disabled in Settings > Apps > Startup");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let (patch, canonical) = build_patch(key, value)?;
    let payload =
        serde_json::to_value(patch).map_err(|err| format!("could not build request: {err}"))?;
    let data = ipc::request("settings.set", payload).map_err(|err| err.to_string())?;
    let settings: Settings = serde_json::from_value(data)
        .map_err(|err| format!("engine sent unreadable settings: {err}"))?;
    println!("{key} = {canonical}");
    // A contradictory-but-allowed combination applies and warns (D9).
    if let Some(warning) = settings.conflict_warning() {
        println!("  warning: {warning}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Validate one `settings set <key> <value>` pair and build the patch. The
/// second element is the canonical spelling echoed back to the user.
fn build_patch(key: &str, value: &str) -> Result<(SettingsPatch, String), String> {
    let mut patch = SettingsPatch::default();
    let canonical = match key {
        "power_profile" => {
            let profile = PowerProfile::parse(value).ok_or_else(|| {
                format!("power_profile expects off|balanced|battery|quality, got {value}")
            })?;
            patch.power_profile = Some(profile);
            profile.as_str().to_string()
        }
        "on_battery" => {
            let action = BatteryAction::parse(value).ok_or_else(|| {
                format!("on_battery expects off|pause|stop|continue, got {value}")
            })?;
            patch.on_battery = Some(action);
            action.as_str().to_string()
        }
        "restore_wallpaper_on_exit" | "tray" => {
            let flag = parse_bool(value)?;
            if key == "restore_wallpaper_on_exit" {
                patch.restore_wallpaper_on_exit = Some(flag);
            } else {
                patch.tray = Some(flag);
            }
            flag.to_string()
        }
        other => {
            let pause_key = other
                .strip_prefix("pause.")
                .ok_or_else(|| unknown_setting_key(other))?;
            let flag = parse_bool(value)?;
            let mut pause = PausePatch::default();
            match pause_key {
                "fullscreen" => pause.fullscreen = Some(flag),
                "maximized" => pause.maximized = Some(flag),
                "display_off" => pause.display_off = Some(flag),
                "session_lock" => pause.session_lock = Some(flag),
                "screensaver" => pause.screensaver = Some(flag),
                "remote_session" => pause.remote_session = Some(flag),
                _ => return Err(unknown_setting_key(other)),
            }
            patch.pause = Some(pause);
            flag.to_string()
        }
    };
    Ok((patch, canonical))
}

/// The error every unknown `settings set` key produces — it lists what *is`
/// valid so the user never has to open the docs (`design-m4` §3).
fn unknown_setting_key(key: &str) -> String {
    format!(
        "unknown setting key '{key}' (valid: power_profile, on_battery, \
         pause.fullscreen, pause.maximized, pause.display_off, pause.session_lock, \
         pause.screensaver, pause.remote_session, restore_wallpaper_on_exit, tray, autostart)"
    )
}

/// `true`/`false`, case-insensitive — the only boolean spelling the CLI
/// accepts.
fn parse_bool(value: &str) -> Result<bool, String> {
    if value.eq_ignore_ascii_case("true") {
        Ok(true)
    } else if value.eq_ignore_ascii_case("false") {
        Ok(false)
    } else {
        Err(format!("expected true or false, got {value}"))
    }
}

// --- Library commands (M4 S3, ipc.md §3 / design-m4 §4) ---------------------

/// `cascade library list|add|remove|prune` — every subcommand needs a
/// daemon (single writer: the main loop, `ipc.md` §3).
fn cmd_library(args: &[String]) -> Result<ExitCode, String> {
    match args.first().map(String::as_str) {
        Some("list") => library_list(&args[1..]),
        Some("add") => library_add(&args[1..]),
        Some("remove") => library_remove(&args[1..]),
        Some("prune") => library_prune(&args[1..]),
        Some(other) => Err(format!(
            "library expects list|add|remove|prune, got {other}"
        )),
        None => Err("library expects list|add|remove|prune — see `cascade help`".to_string()),
    }
}

fn library_list(args: &[String]) -> Result<ExitCode, String> {
    if !args.is_empty() {
        return Err("list takes no arguments".to_string());
    }
    let data = ipc::request("library.list", no_data()).map_err(|err| err.to_string())?;
    let items: LibraryItemsData = serde_json::from_value(data)
        .map_err(|err| format!("engine sent an unreadable library: {err}"))?;
    if items.items.is_empty() {
        println!("library is empty — add files with `cascade library add <path>`");
        return Ok(ExitCode::SUCCESS);
    }
    for item in &items.items {
        println!(
            "{}  {}  {}x{}  {}  {}  {}  {}",
            item.id,
            item.title,
            item.width,
            item.height,
            item.fps
                .map(|fps| format!("{fps:.2} fps"))
                .unwrap_or_else(|| "-".to_string()),
            item.duration_ms
                .map(format_duration)
                .unwrap_or_else(|| "-".to_string()),
            item.codec,
            item.path
        );
    }
    println!("{} item(s)", items.items.len());
    Ok(ExitCode::SUCCESS)
}

/// `cascade library add <path>...` — reference, never copy (AGENTS §3.3).
/// Paths are absolutised here: the daemon's cwd is not the CLI's.
fn library_add(args: &[String]) -> Result<ExitCode, String> {
    if args.is_empty() {
        return Err(
            "add needs at least one video file: `cascade library add <path>...`".to_string(),
        );
    }
    let mut paths = Vec::with_capacity(args.len());
    for arg in args {
        let absolute =
            std::path::absolute(arg).map_err(|err| format!("cannot resolve {arg}: {err}"))?;
        paths.push(absolute.display().to_string());
    }
    let payload = serde_json::to_value(LibraryAddData { paths })
        .map_err(|err| format!("could not build request: {err}"))?;
    let data = ipc::request("library.add", payload).map_err(|err| err.to_string())?;
    let result: LibraryAddResult = serde_json::from_value(data)
        .map_err(|err| format!("engine sent an unreadable add result: {err}"))?;
    for item in &result.added {
        println!("added: {} ({})", item.title, item.path);
        // Badge data computed at the moment the item entered the library
        // (design-m4 §5.3 step 5) — printed here so the CLI is honest too;
        // an `error` row never blocks an add (the library is paths only).
        if let Some(assessment) = result.assessments.iter().find(|a| a.id == item.id) {
            print_warnings(&assessment.warnings);
        }
    }
    if !result.rejected.is_empty() {
        let details = result
            .rejected
            .iter()
            .map(|entry| format!("  {} — {}", entry.path, entry.reason))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "{} added, {} rejected:\n{details}",
            result.added.len(),
            result.rejected.len()
        ));
    }
    Ok(ExitCode::SUCCESS)
}

/// `cascade library remove <id|path>` — edits the list only; the file on
/// disk is never touched (AGENTS §3.3).
fn library_remove(args: &[String]) -> Result<ExitCode, String> {
    if args.len() != 1 {
        return Err("remove needs one id or path: `cascade library remove <id|path>`".to_string());
    }
    let token = args[0].as_str();
    // The list doubles as the path→id index and the confirmation source.
    let data = ipc::request("library.list", no_data()).map_err(|err| err.to_string())?;
    let items: LibraryItemsData = serde_json::from_value(data)
        .map_err(|err| format!("engine sent an unreadable library: {err}"))?;
    let item = if is_item_id(token) {
        items
            .items
            .iter()
            .find(|item| item.id.eq_ignore_ascii_case(token))
    } else {
        let absolute =
            std::path::absolute(token).map_err(|err| format!("cannot resolve {token}: {err}"))?;
        find_item_by_path(&items.items, &absolute.display().to_string())
    }
    .ok_or_else(|| format!("no library item matches '{token}' — run `cascade library list`"))?;
    let payload = serde_json::to_value(LibraryRemoveData {
        id: item.id.clone(),
    })
    .map_err(|err| format!("could not build request: {err}"))?;
    ipc::request("library.remove", payload).map_err(|err| err.to_string())?;
    println!(
        "removed: {} ({}) — the file itself is untouched",
        item.title, item.path
    );
    Ok(ExitCode::SUCCESS)
}

/// `cascade library prune` — same drop rule as the start-time prune.
fn library_prune(args: &[String]) -> Result<ExitCode, String> {
    if !args.is_empty() {
        return Err("prune takes no arguments".to_string());
    }
    let data = ipc::request("library.prune", no_data()).map_err(|err| err.to_string())?;
    let result: LibraryPruneResult = serde_json::from_value(data)
        .map_err(|err| format!("engine sent an unreadable prune result: {err}"))?;
    if result.removed == 0 {
        println!("nothing to prune — every library file exists");
    } else {
        println!(
            "pruned {} entr{} whose files no longer exist",
            result.removed,
            if result.removed == 1 { "y" } else { "ies" }
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// A library id: exactly 4 hex characters (design-m4 §4).
fn is_item_id(token: &str) -> bool {
    token.len() == 4 && token.chars().all(|c| c.is_ascii_hexdigit())
}

/// Resolve a path argument against a fetched list with the same
/// case/separator rule the daemon dedupes with.
fn find_item_by_path<'a>(items: &'a [LibraryItem], path: &str) -> Option<&'a LibraryItem> {
    let key = dedupe_key(path);
    items.iter().find(|item| dedupe_key(&item.path) == key)
}

/// `125_000` → `2:05`; over an hour → `1:02:03`.
fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (hours, minutes, seconds) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// `cascade assess [<file>] [--monitor <id>] [--json]` (design-m4 §3):
/// no file → device profile + recommendation for the selected monitor;
/// with a file → + that file's descriptor and structured warnings.
/// Read-only, never blocks — an `error` row is a diagnosis (only
/// `cascade set` refuses). The path is absolutised here: the daemon's
/// working directory is not the CLI's.
fn cmd_assess(args: &[String]) -> Result<ExitCode, String> {
    let mut path: Option<String> = None;
    let mut monitor: Option<String> = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--monitor" => {
                if monitor.is_some() {
                    return Err("--monitor given twice".to_string());
                }
                let value = args.get(index + 1).ok_or_else(|| {
                    "--monitor needs a monitor id like \\\\.\\DISPLAY1".to_string()
                })?;
                monitor = Some(value.clone());
                index += 2;
            }
            "--json" => {
                if json {
                    return Err("--json given twice".to_string());
                }
                json = true;
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option for assess: {other}"));
            }
            other => {
                if path.is_some() {
                    return Err("assess takes at most one file".to_string());
                }
                let absolute = std::path::absolute(other)
                    .map_err(|err| format!("cannot resolve {other}: {err}"))?;
                path = Some(absolute.display().to_string());
                index += 1;
            }
        }
    }
    let payload = serde_json::to_value(AssessRequest { path, monitor })
        .map_err(|err| format!("could not build request: {err}"))?;
    let data = ipc::request("assess", payload).map_err(|err| err.to_string())?;
    if json {
        let text = serde_json::to_string_pretty(&data)
            .map_err(|err| format!("could not print the report: {err}"))?;
        println!("{text}");
        return Ok(ExitCode::SUCCESS);
    }
    let report: AssessReport = serde_json::from_value(data)
        .map_err(|err| format!("engine sent an unreadable assess report: {err}"))?;
    print_assess_report(&report);
    Ok(ExitCode::SUCCESS)
}

/// One structured warning, three lines: measured values, consequence,
/// fix (design-m4 §5.2 — this text is what the M5 badge shows).
fn print_warnings(warnings: &[Warning]) {
    for warning in warnings {
        println!(
            "[{}] {}: {}",
            warning.severity.as_str(),
            warning.metric.as_str(),
            warning.detail
        );
        println!("         → {}", warning.consequence);
        println!("         fix: {}", warning.fix);
    }
}

fn print_recommendation(rec: &Recommendation) {
    println!("recommend {}x{} — {}", rec.width, rec.height, rec.note);
}

/// The readable `assess` report (the `--json` form is the raw payload).
fn print_assess_report(report: &AssessReport) {
    let decode = if report.device.gpu_decode.is_empty() {
        "unknown".to_string()
    } else {
        report.device.gpu_decode.join(", ")
    };
    let ram = report
        .device
        .ram_total_mb
        .map(|mb| format!("{mb} MB"))
        .unwrap_or_else(|| "unknown".to_string());
    println!("device   GPU decode: {decode} · RAM: {ram}");
    if let Some(monitor) = report
        .device
        .monitors
        .iter()
        .find(|monitor| monitor.id == report.monitor)
    {
        let hz = monitor
            .refresh_hz
            .map(|hz| format!(" @ {hz} Hz"))
            .unwrap_or_default();
        let transfer = match monitor.hdr {
            Some(true) => "HDR",
            Some(false) => "SDR",
            None => "transfer unknown",
        };
        println!(
            "monitor  {} {}x{}{hz}, {transfer}",
            monitor.id, monitor.width, monitor.height
        );
    }
    print_recommendation(&report.recommendation);
    if let Some(video) = &report.video {
        let key = codec_key(&video.codec);
        let profile = video
            .profile
            .as_deref()
            .and_then(|value| value.parse::<u32>().ok())
            .map(|profile| profile_name(&key, profile))
            .unwrap_or_else(|| video.codec.clone());
        let level = video
            .level
            .as_deref()
            .and_then(|value| value.parse::<u32>().ok())
            .map(|level| format!(", level {}", level_name(&key, level)))
            .unwrap_or_default();
        let fps = video
            .fps
            .map(|fps| format!("{fps:.2} fps"))
            .unwrap_or_else(|| "-".to_string());
        let bitrate = video
            .bitrate_bps
            .map(|bps| format!("{} kbps", bps / 1000))
            .unwrap_or_else(|| "-".to_string());
        let transfer = match video.hdr {
            Some(true) => "HDR",
            Some(false) => "SDR",
            None => "transfer unknown",
        };
        println!(
            "video    {}x{} {profile}{level} {fps} {bitrate}, {transfer}",
            video.width, video.height
        );
    }
    print_warnings(&report.warnings);
    if report.video.is_none() {
        return;
    }
    let errors = report
        .warnings
        .iter()
        .filter(|warning| warning.severity == Severity::Error)
        .count();
    if errors > 0 {
        println!(
            "{errors} error(s) — `cascade set` refuses this file (codec policy, architecture.md §6)"
        );
    } else if report.warnings.is_empty() {
        println!("no warnings — this video fits the display and the hardware");
    } else {
        println!("{} warning(s)", report.warnings.len());
    }
}

fn print_help() {
    println!("cascade {VERSION}");
    println!();
    println!("USAGE:");
    println!("  cascade [run] [--color RRGGBB] [--repaint 1-4] [--host auto|workerw|layered]");
    println!("           [--video <path>]   play this file behind the desktop icons");
    println!("  cascade doctor");
    println!("  cascade stop | restore | quit");
    println!("  cascade status [--json]");
    println!("  cascade pause | resume");
    println!("  cascade set <path> | --id <a3f2> [--monitor <id>]  play a file or library item");
    println!("  cascade clear [--monitor <id>]        back to the colour background");
    println!("  cascade settings get [--json]");
    println!("  cascade settings set <key> <value>    keys: power_profile, on_battery,");
    println!("                                        pause.fullscreen, pause.maximized,");
    println!("                                        pause.display_off, pause.session_lock,");
    println!("                                        pause.screensaver, pause.remote_session,");
    println!("                                        restore_wallpaper_on_exit, tray, autostart");
    println!("  cascade library list");
    println!("  cascade library add <path>...         files are referenced, never copied");
    println!("  cascade library remove <id|path>      remove from library (file untouched)");
    println!("  cascade library prune                 drop entries whose files are gone");
    println!("  cascade assess [<file>] [--monitor <id>] [--json]");
    println!("                                        no file: device + recommendation;");
    println!("                                        with a file: + its warnings");
    println!("  cascade autostart on|off|status");
    println!(
        "  cascade install                    copy this exe to %LOCALAPPDATA%\\Cascade + PATH"
    );
    println!("  cascade uninstall [--purge]        remove the install (--purge also drops state)");
    println!("  cascade ui");
    println!();
    println!("  run is the default. --autostart is accepted for the Run key.");
    println!("  Without --video, the last successfully played path replays.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn daemon_role_is_bare_run_and_autostart_only() {
        assert!(is_daemon_role(&[]));
        assert!(is_daemon_role(&args(&["run"])));
        assert!(is_daemon_role(&args(&["--autostart"])));
        assert!(!is_daemon_role(&args(&["status"])));
        assert!(!is_daemon_role(&args(&["doctor"])));
        // Run options only count after `run`; a bare flag is a CLI typo.
        assert!(!is_daemon_role(&args(&["--repaint", "2"])));
        assert!(!is_daemon_role(&args(&["--color", "FF0000"])));
        assert!(!is_daemon_role(&args(&["--video", "clip.mp4"])));
    }

    #[test]
    fn bare_action_follows_the_d5_flow() {
        // Respawn wins: the internal child must never hit the prompt.
        assert_eq!(bare_action(true, true, true), BareAction::RespawnRun);
        assert_eq!(bare_action(false, false, true), BareAction::RespawnRun);
        // Not installed: always the Yes/No prompt (D5), console or not.
        assert_eq!(bare_action(false, true, false), BareAction::PromptInstall);
        assert_eq!(bare_action(false, false, false), BareAction::PromptInstall);
        // Installed: typed command runs, double-click shows usage only.
        assert_eq!(bare_action(true, true, false), BareAction::Run);
        assert_eq!(bare_action(true, false, false), BareAction::Usage);
    }

    #[test]
    fn uninstall_options_only_accept_purge() {
        assert_eq!(uninstall_options(&[]), Ok(false));
        assert_eq!(uninstall_options(&args(&["--purge"])), Ok(true));
        assert!(uninstall_options(&args(&["--nope"])).is_err());
        assert!(uninstall_options(&args(&["--purge", "--nope"])).is_err());
    }

    #[test]
    fn run_options_parse_video_path() {
        let options = parse_run_options(&args(&["--video", r"C:\videos\my clip.mp4"]))
            .expect("valid options");
        assert_eq!(
            options.video.as_deref(),
            Some(Path::new(r"C:\videos\my clip.mp4"))
        );
        // Everything else keeps the documented baseline.
        assert_eq!(options.repaint_level, 1);
        assert!(options.video.is_some());
    }

    #[test]
    fn run_options_without_video_have_no_source() {
        let options = parse_run_options(&args(&["--color", "187A86"])).expect("valid options");
        assert!(options.video.is_none());
    }

    #[test]
    fn respawn_filter_drops_video_flag_and_keeps_everything_else() {
        // `--video` and its value go; the child reads `state.json` instead
        // (design-m2 §4). Position anywhere, other options untouched.
        let raw = |list: &[&str]| -> Vec<std::ffi::OsString> {
            list.iter().map(std::ffi::OsString::from).collect()
        };
        assert_eq!(
            filter_video_flag(raw(&["run", "--video", r"C:\a.mp4", "--repaint", "2"])),
            raw(&["run", "--repaint", "2"])
        );
        assert_eq!(
            filter_video_flag(raw(&["run", "--color", "FF0000", "--video", r"C:\b.mp4"])),
            raw(&["run", "--color", "FF0000"])
        );
        assert_eq!(filter_video_flag(raw(&["run"])), raw(&["run"]));
        assert_eq!(filter_video_flag(raw(&[])), raw(&[]));
        // A trailing `--video` with no value loses nothing else.
        assert_eq!(filter_video_flag(raw(&["run", "--video"])), raw(&["run"]));
    }

    #[test]
    fn status_options_parse_json_flag() {
        assert!(!status_options(&[]).expect("no flags"));
        assert!(status_options(&args(&["--json"])).expect("--json"));
        assert!(
            status_options(&args(&["--yaml"])).is_err(),
            "unknown flags must be rejected, not ignored"
        );
    }

    #[test]
    fn run_options_reject_missing_video_value() {
        let err = parse_run_options(&args(&["--video"])).expect_err("needs a value");
        assert!(err.contains("--video"), "unexpected error: {err}");
    }

    #[test]
    fn parse_color_accepts_common_hex_forms() {
        let teal = host::rgb(0x18, 0x7A, 0x86);
        assert_eq!(parse_color("187A86").expect("plain"), teal);
        assert_eq!(parse_color("187a86").expect("lowercase"), teal);
        assert_eq!(parse_color("#187A86").expect("hash"), teal);
        assert_eq!(parse_color("0x187A86").expect("0x prefix"), teal);
        assert_eq!(parse_color("  187A86 ").expect("trimmed"), teal);
        assert_eq!(parse_color("000000").expect("black"), 0);
        // Fewer than six digits is still a valid RRGGBB value.
        assert_eq!(
            parse_color("187A8").expect("short form"),
            host::rgb(0x01, 0x87, 0xA8)
        );
    }

    #[test]
    fn parse_color_rejects_bad_input() {
        assert!(parse_color("").is_err());
        assert!(parse_color("xyz").is_err());
        assert!(parse_color("GGGGGG").is_err());
        assert!(parse_color("1000000").is_err()); // 7 digits > 0xFFFFFF
        assert!(parse_color("187A86FF").is_err()); // 8 digits > 0xFFFFFF
    }

    #[test]
    fn set_options_parse_file_and_monitor() {
        let options = parse_set_options(&args(&[
            r"C:\videos\my clip.mp4",
            "--monitor",
            r"\\.\DISPLAY1",
        ]))
        .expect("file and monitor");
        assert_eq!(
            options.path.as_deref(),
            Some(Path::new(r"C:\videos\my clip.mp4"))
        );
        assert!(options.id.is_none());
        assert_eq!(options.monitor.as_deref(), Some(r"\\.\DISPLAY1"));

        // The monitor is optional; the file never is.
        let options = parse_set_options(&args(&["clip.mp4"])).expect("file only");
        assert_eq!(options.path.as_deref(), Some(Path::new("clip.mp4")));
        assert!(options.monitor.is_none());

        // A library id replaces the file (S3).
        let options = parse_set_options(&args(&["--id", "a3f2"])).expect("id only");
        assert!(options.path.is_none());
        assert_eq!(options.id.as_deref(), Some("a3f2"));
        let err = parse_set_options(&args(&["--id", "zzzz"])).expect_err("hex only");
        assert!(err.contains("not a library id"), "unexpected: {err}");
        // Exactly one of path / id — never both, never neither.
        assert!(parse_set_options(&args(&["a.mp4", "--id", "a3f2"])).is_err());
        assert!(parse_set_options(&args(&["--id"])).is_err());
        assert!(parse_set_options(&args(&["--id", "a3f2", "--id", "b1c1"])).is_err());
    }

    #[test]
    fn set_options_reject_bad_input() {
        // No file.
        assert!(parse_set_options(&[]).is_err());
        assert!(parse_set_options(&args(&["--monitor", r"\\.\DISPLAY1"])).is_err());
        // Two files.
        let err = parse_set_options(&args(&["a.mp4", "b.mp4"])).expect_err("one file only");
        assert!(err.contains("one video file"), "unexpected: {err}");
        // Unknown flag.
        assert!(parse_set_options(&args(&["a.mp4", "--force"])).is_err());
        // `--monitor` without a value, and given twice.
        assert!(parse_set_options(&args(&["a.mp4", "--monitor"])).is_err());
        assert!(parse_set_options(&args(&[
            "a.mp4",
            "--monitor",
            r"\\.\DISPLAY1",
            "--monitor",
            r"\\.\DISPLAY2"
        ]))
        .is_err());
    }

    #[test]
    fn clear_options_parse_monitor_flag() {
        assert!(parse_monitor_option(&[]).expect("no flags").is_none());
        assert_eq!(
            parse_monitor_option(&args(&["--monitor", r"\\.\DISPLAY2"]))
                .expect("monitor")
                .as_deref(),
            Some(r"\\.\DISPLAY2")
        );
        // Anything else is an error, not silently ignored.
        assert!(parse_monitor_option(&args(&["DISPLAY2"])).is_err());
        assert!(parse_monitor_option(&args(&["--monitor"])).is_err());
        assert!(parse_monitor_option(&args(&["--monitor", "A", "--monitor", "B"])).is_err());
    }

    #[test]
    fn settings_patch_keys_are_validated_strictly() {
        // Known keys parse and canonicalise.
        let (patch, canonical) = build_patch("power_profile", "battery").expect("valid");
        assert_eq!(patch.power_profile, Some(PowerProfile::Battery));
        assert_eq!(canonical, "battery");

        let (patch, canonical) = build_patch("pause.session_lock", "FALSE").expect("valid");
        assert_eq!(patch.pause.expect("pause patch").session_lock, Some(false));
        assert_eq!(canonical, "false");

        let (patch, canonical) = build_patch("on_battery", "pause").expect("valid");
        assert_eq!(patch.on_battery, Some(BatteryAction::Pause));
        assert_eq!(canonical, "pause");

        // Unknown keys fail with the list of valid ones — never ignored.
        for key in ["video", "pause.nope", "battery"] {
            let err = build_patch(key, "true").expect_err("must reject");
            assert!(
                err.contains("power_profile") && err.contains("pause.session_lock"),
                "error must list valid keys: {err}"
            );
        }
        // Bad values fail too.
        assert!(build_patch("power_profile", "turbo").is_err());
        assert!(build_patch("on_battery", "sometimes").is_err());
        assert!(build_patch("tray", "yes").is_err());
        assert!(build_patch("pause.fullscreen", "on").is_err());
        // `autostart` is a valid key overall but not a settings patch —
        // `settings_set` intercepts it before `build_patch`.
        assert!(build_patch("autostart", "true").is_err());
    }

    #[test]
    fn bool_values_are_true_or_false_only() {
        assert!(parse_bool("true").expect("lower"));
        assert!(parse_bool("TRUE").expect("upper"));
        assert!(!parse_bool("False").expect("mixed"));
        assert!(parse_bool("1").is_err());
        assert!(parse_bool("on").is_err());
    }

    #[test]
    fn library_subcommands_are_validated() {
        // No/unknown subcommand fails before any pipe request is made.
        let err = cmd_library(&[]).expect_err("needs a subcommand");
        assert!(err.contains("list|add|remove|prune"), "unexpected: {err}");
        let err = cmd_library(&args(&["frobnicate"])).expect_err("unknown");
        assert!(err.contains("frobnicate"), "unexpected: {err}");
        // Argument arity is checked before touching the daemon too.
        assert!(cmd_library(&args(&["list", "extra"])).is_err());
        assert!(cmd_library(&args(&["prune", "extra"])).is_err());
        assert!(cmd_library(&args(&["remove"])).is_err());
        assert!(cmd_library(&args(&["remove", "a", "b"])).is_err());
        assert!(cmd_library(&args(&["add"])).is_err());
    }

    #[test]
    fn item_ids_are_exactly_four_hex_characters() {
        assert!(is_item_id("a3f2"));
        assert!(is_item_id("00ff"));
        assert!(is_item_id("A3F2"), "case-insensitive on the wire");
        assert!(!is_item_id("a3f"), "three chars");
        assert!(!is_item_id("a3f22"), "five chars");
        assert!(!is_item_id("zzzz"), "hex only");
        assert!(!is_item_id(""), "never empty");
    }

    #[test]
    fn duration_formats_readably() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(5_000), "0:05");
        assert_eq!(format_duration(125_000), "2:05");
        assert_eq!(format_duration(3_723_000), "1:02:03");
    }

    #[test]
    fn find_item_by_path_uses_the_daemon_dedupe_rule() {
        let items = vec![LibraryItem::new(
            "a1b2".to_string(),
            r"C:\Videos\Clip.MP4".to_string(),
            cascade_core::library::VideoMetadata {
                width: 1,
                height: 1,
                fps: None,
                duration_ms: None,
                bitrate_bps: None,
                codec: "H264".to_string(),
                profile: None,
                level: None,
                hdr: None,
            },
            0,
        )];
        let found = find_item_by_path(&items, r"c:\videos\clip.mp4").expect("case-insensitive");
        assert_eq!(found.id, "a1b2");
        assert!(find_item_by_path(&items, r"C:\Videos\other.mp4").is_none());
    }
}
