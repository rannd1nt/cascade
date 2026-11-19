//! `cascade` — role dispatch only.
//!
//! All Win32 behaviour lives in `cascade-win`; this binary parses argv and
//! reports results. See `docs/architecture.md` §2.

// GUI subsystem: starting the daemon must never create a console window
// (M1-F, measured flash of ~100 ms with focus steal on a console build).
// The CLI re-attaches to a console in `console::prepare_stdio`.
#![windows_subsystem = "windows"]

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use cascade_core::assess::{codec_key, Recommendation, Severity, Warning};
use cascade_core::client;
use cascade_core::device::{level_name, profile_name};
use cascade_core::ipc::{
    no_data, AssessReport, AssessRequest, AutostartStatus, EnabledData, LibraryAddData,
    LibraryAddResult, LibraryItemsData, LibraryPruneResult, LibraryRemoveData, MonitorData,
    PausePatch, SetData, SetReport, Settings, SettingsPatch,
};
use cascade_core::library::{dedupe_key, LibraryItem};
use cascade_core::rule_engine::{BatteryAction, PowerProfile};
use cascade_core::{MediaBackend, PresentationMode};
use cascade_media::{MediaEngineBackend, SourceReaderBackend};
use cascade_win::{
    console, control, daemon, doctor, dpi, host, install, listener, offline, registry, state,
};

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
        console::prepare_stdio(is_daemon_role(&args) || is_gui_role(&args));
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

/// `run`, `--autostart`, and `--listen` start the long-lived roles;
/// everything else is a one-shot CLI command that must print. The bare form
/// is decided in [`bare_main`] before this is reached.
fn is_daemon_role(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        None | Some("run") | Some("--autostart") | Some("--listen")
    )
}

/// `ui` / `--ui` open a GUI window (design-m5 D11c). Like the daemon it
/// must never *create* a console — a windowed app double-clicked from a
/// shortcut would flash one — while a terminal launch still attaches and
/// prints errors there (same `prepare_stdio` strategy as the daemon).
fn is_gui_role(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("ui") | Some("--ui"))
}

fn dispatch(args: &[String]) -> Result<ExitCode, String> {
    match args.first().map(String::as_str) {
        None => cmd_run(&[]),
        Some("run") => cmd_run(&args[1..]),
        Some("--autostart") => cmd_run(&args[1..]),
        // Pw2-S1 (`design-m5b.md` §6): the listener as a manual entry —
        // the automatic entry rules arrive with Pw2-S2 (§4.3).
        Some("--listen") => cmd_listen(&args[1..]),
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
            // The library window (design-m5 P2): a separate process that
            // talks to the daemon only over the pipe. D7 (a second
            // instance focuses the first) is deferred to P4 — today a
            // second launch simply opens a second window.
            cascade_ui::run()?;
            Ok(ExitCode::SUCCESS)
        }
        Some(other) => {
            print_help();
            Err(format!("unknown command: {other}"))
        }
    }
}

fn cmd_run(args: &[String]) -> Result<ExitCode, String> {
    // `--detached` (design-m5 §4) never reaches the daemon: it changes how
    // *this* process spawns the child, not what the child does.
    let (detached, args) = split_detached_flag(args)?;
    if detached {
        return run_detached(&args);
    }
    let options = parse_run_options(&args)?;

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

/// `cascade --listen` (`design-m5b.md` §4.2, Pw2-S1): run the battery
/// listener — control pipe and window only. Structural skip-load: this
/// branch returns before `build_backend` is ever called, so no backend, no
/// factory, and no Media Foundation session can exist in this process.
fn cmd_listen(args: &[String]) -> Result<ExitCode, String> {
    if let Some(extra) = args.first() {
        return Err(format!("unknown `--listen` argument: {extra}"));
    }
    match listener::run() {
        Ok(daemon::RunExit::Exit) => Ok(ExitCode::SUCCESS),
        // The listener never respawns in Pw2-S1 (transitions arrive with
        // Pw2-S2); the arm exists so the shape matches `cmd_run`.
        Ok(daemon::RunExit::Respawn) => respawn_child(),
        Err(err) => {
            cascade_win::derr!("cascade: {err}");
            Ok(ExitCode::from(1))
        }
    }
}

/// `DETACHED_PROCESS` (Win32): the child gets no console of its own, so it
/// survives this process *and* the terminal that launched it — the whole
/// point of `run --detached` (design-m5 §4). Value from the process
/// creation flags:
/// <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// How long `run --detached` waits for the child's pipe to answer
/// (design-m5 §4). A healthy daemon is up well inside this; the loop polls
/// `status` every [`DETACHED_POLL`].
const DETACHED_READY_TIMEOUT: Duration = Duration::from_secs(2);
const DETACHED_POLL: Duration = Duration::from_millis(100);

/// Strip `--detached` from a `run` argv (design-m5 §4). Pure so the flag
/// handling is unit-testable; the child never sees it and validates the
/// remaining flags itself.
fn split_detached_flag(args: &[String]) -> Result<(bool, Vec<String>), String> {
    let mut detached = false;
    let mut rest = Vec::with_capacity(args.len());
    for arg in args {
        match arg.as_str() {
            "--detached" if detached => return Err("--detached given twice".to_string()),
            "--detached" => detached = true,
            other => rest.push(other.to_string()),
        }
    }
    Ok((detached, rest))
}

/// The detached child's argv: `run` plus this command's flags. Pure so the
/// re-entry contract is unit-testable — the child must take the daemon
/// path (`is_daemon_role`), never the bare double-click flow
/// (design-m4 §6.2, which may prompt to install) and never the
/// unknown-command arm of `dispatch` (a flag-only argv would land there).
fn detached_child_args(args: &[String]) -> Vec<String> {
    let mut child = Vec::with_capacity(args.len() + 1);
    child.push("run".to_string());
    child.extend(args.iter().cloned());
    child
}

/// `cascade run --detached` (design-m5 §4): start the daemon without a
/// console, wait until its pipe answers, print the pid, return — the
/// caller may close the terminal immediately and the wallpaper keeps
/// playing. Plain `run` stays foreground for debugging.
fn run_detached(args: &[String]) -> Result<ExitCode, String> {
    // Single instance (`ipc.md` §1): refuse *before* spawning a child that
    // would only print an error and die.
    if control::find_existing().is_some() {
        return Err(
            "Cascade is already running — `cascade status` to inspect, `cascade stop` to end it"
                .to_string(),
        );
    }
    // Validate the flags here (pure parsing, no backend) so a typo fails
    // before a process exists.
    parse_run_options(args)?;
    // The caller's std handles are inheritable (a pipe when the CLI is
    // piped, `CONOUT$` in a terminal): without clearing the flag the
    // spawned daemon would copy them, and a caller reading our stdout
    // would then wait for an EOF that only arrives when the daemon exits
    // (measured hang, 2026-11-11, build 26200).
    console::make_stdio_non_inheritable();
    let exe = std::env::current_exe().map_err(|err| format!("--detached: {err}"))?;
    let mut child = std::process::Command::new(exe)
        .args(detached_child_args(args))
        // The marker keeps the whole lineage out of the user's console:
        // `console.rs::detached_lineage` skips `AttachConsole`, so closing
        // the terminal cannot end the wallpaper engine (`CTRL_CLOSE`).
        .env(console::DETACHED_MARKER, "1")
        .creation_flags(DETACHED_PROCESS)
        // The child must not inherit this terminal's handles: once the
        // terminal closes they die, and the daemon's `eprintln!` would
        // panic on the dead handle (AGENTS §3 — a daemon must not panic).
        // `NUL` handles are permanent — exactly what an autostart start
        // gets (`console.rs`).
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("--detached: could not start the engine: {err}"))?;
    let pid = child.id();
    let deadline = Instant::now() + DETACHED_READY_TIMEOUT;
    // Assigned by the first failed poll before it is ever read — the
    // timeout message below can only be reached through that assignment.
    let mut last_error: String;
    loop {
        // A child that already died lost a race or failed to start; its
        // own diagnostics went to NUL, so the foreground run is the way
        // to see them (stated in the error below).
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "the engine exited immediately ({status}) — run `cascade run` in the foreground to see why"
                ));
            }
            Ok(None) => {}
            Err(err) => return Err(format!("--detached: {err}")),
        }
        match client::request("status", no_data()) {
            // The pipe answers: the daemon is up and controllable.
            Ok(_) => {
                println!("detached: engine running as pid {pid}");
                return Ok(ExitCode::SUCCESS);
            }
            // Not yet (or briefly busy): poll again until the deadline.
            Err(err) => last_error = err.to_string(),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "--detached: the engine did not come up within 2 s ({last_error}) — run `cascade doctor`"
            ));
        }
        std::thread::sleep(DETACHED_POLL);
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
    let data =
        client::request("status", cascade_core::ipc::no_data()).map_err(|err| err.to_string())?;
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
    match client::request("wallpaper.restore", no_data()) {
        Ok(_) => {
            println!("restore: requested");
            Ok(ExitCode::SUCCESS)
        }
        // The daemon is not running at all: say so instead of pretending.
        Err(client::ClientError::NotRunning) => Err("Cascade is not running".to_string()),
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
    client::request(cmd, no_data()).map_err(|err| err.to_string())?;
    println!("{label}d");
    Ok(ExitCode::SUCCESS)
}

/// What `set` accepted: exactly one target plus an optional monitor
/// target. Pure syntax — file existence is the daemon's job (`file_missing`,
/// checked before any playback surgery).
#[derive(Debug)]
struct SetOptions {
    /// Exactly one of `path` / `id` / `ordinal` reaches the wire
    /// (`ipc.md` §3): ordinals are resolved to the stable id first
    /// (`design-m5.md` D1).
    path: Option<PathBuf>,
    id: Option<String>,
    /// A CLI-only library number (`set 3`), resolved against a fresh
    /// `library.list` at execution time.
    ordinal: Option<usize>,
    monitor: Option<String>,
}

fn parse_set_options(args: &[String]) -> Result<SetOptions, String> {
    let mut file: Option<PathBuf> = None;
    let mut item_id: Option<String> = None;
    let mut ordinal: Option<usize> = None;
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
                if file.is_some() || item_id.is_some() || ordinal.is_some() {
                    return Err(format!(
                        "set takes one video file or library number, got a second argument: {other}"
                    ));
                }
                // A bare decimal number is a library ordinal (`set 3`,
                // design-m5 D1), never a path: resolve it against a fresh
                // list at execution time. A file actually named `3` needs
                // a `.\` prefix or a full path.
                match parse_ordinal(other) {
                    Some(n) => ordinal = Some(n),
                    None => file = Some(PathBuf::from(other)),
                }
                index += 1;
            }
        }
    }
    let targets = usize::from(file.is_some())
        + usize::from(item_id.is_some())
        + usize::from(ordinal.is_some());
    match targets {
        0 => Err(
            "set needs a path, a library number, or an id: `cascade set <path|3> [--id <a3f2>] [--monitor <id>]`"
                .to_string(),
        ),
        1 => Ok(SetOptions {
            path: file,
            id: item_id,
            ordinal,
            monitor,
        }),
        _ => Err(
            "set takes exactly one of a path, a library number, or --id (a library item)".to_string(),
        ),
    }
}

/// `cascade set <path> | <n> | --id <a3f2> [--monitor <id>]` — play a file
/// directly or a library item (design-m4 S2 path, S3 id; design-m5 D1 for
/// the ordinal). The path is made absolute here: the daemon's working
/// directory is not the CLI's.
fn cmd_set(args: &[String]) -> Result<ExitCode, String> {
    let options = parse_set_options(args)?;
    // Exactly one of path / id / ordinal goes out (`ipc.md` §3).
    let (data, label) = if let Some(item_id) = options.id {
        (
            SetData {
                path: None,
                id: Some(item_id.clone()),
                monitor: options.monitor,
            },
            format!("playing library item {item_id}"),
        )
    } else if let Some(n) = options.ordinal {
        // Ordinals are CLI-only: resolve against a *fresh* list at
        // execution time, then send the stable wire id (design-m5 D1).
        // The success line names the resolved title and path, so a stale
        // number is visible immediately.
        let items = fetch_library()?;
        let item =
            resolve_ordinal(&items.items, n).ok_or_else(|| ordinal_error(n, items.items.len()))?;
        (
            SetData {
                path: None,
                id: Some(item.id.clone()),
                monitor: options.monitor,
            },
            format!("playing: {} ({})", item.title, item.path),
        )
    } else {
        let path = options.path.expect("parser guarantees exactly one target");
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
    let data = client::request("wallpaper.set", payload).map_err(|err| err.to_string())?;
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
    client::request("wallpaper.clear", payload).map_err(|err| err.to_string())?;
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
    match client::request("quit", no_data()) {
        Ok(_) => {
            println!("stopping");
            Ok(ExitCode::SUCCESS)
        }
        Err(client::ClientError::NotRunning) => Err("Cascade is not running".to_string()),
        Err(client::ClientError::Busy) => {
            Err("the engine is busy — retry, or use `cascade stop`".to_string())
        }
        Err(err) => Err(err.to_string()),
    }
}

// --- Routing guard (design-m5b §3.1) ----------------------------------------
/// What the window probe says about our processes, relative to the retry
/// budget (`design-m5b.md` §3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Guard {
    /// `control::find_existing()` found no Cascade control window.
    Absent,
    /// Our control window exists and the budget still has room: a daemon is
    /// booting (or hung).
    Present,
    /// Our control window exists but the budget is spent: the pipe never
    /// came up inside it.
    Unanswered,
}

/// The decision for one attempt (`design-m5b.md` §3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// The pipe route decides: either a daemon payload or an honest error.
    Daemon,
    /// No pipe while our control window exists: retry inside the budget.
    Retry,
    /// Two probes found nothing of ours: the direct-file route.
    Offline,
    /// The window stayed and the budget ran out: never write behind a
    /// possibly-live daemon.
    Hung,
}

/// The answer to one routed request: the daemon's payload, or the positive
/// verdict that nothing of ours is running (`design-m5b.md` §3.1).
enum Routed {
    Daemon(serde_json::Value),
    Offline,
}

/// Retry cadence while a control window exists without a pipe: every 150 ms
/// up to 1.5 s in total (`design-m5b.md` §3.1).
const GUARD_RETRY: Duration = Duration::from_millis(150);
const GUARD_BUDGET: Duration = Duration::from_millis(1_500);
/// The confirm delay after a first negative verdict: the daemon's boot window
/// between pipe creation and control window is ~100 ms, so a second probe
/// 200 ms later settles it (AGENTS §3.17 — no watcher, no auto-spawn).
const GUARD_CONFIRM: Duration = Duration::from_millis(200);
const GUARD_HUNG: &str = "a Cascade process is present but not answering — wait a moment and retry";

/// The routing decision for a single attempt. Pure: the loop in
/// [`request_routed`] performs the sleeps and re-probes; this truth table is
/// unit-tested.
fn route(result: &Result<serde_json::Value, client::ClientError>, guard: Guard) -> Route {
    match result {
        Ok(_) => Route::Daemon,
        Err(client::ClientError::NotRunning) => match guard {
            Guard::Absent => Route::Offline,
            Guard::Present => Route::Retry,
            Guard::Unanswered => Route::Hung,
        },
        // Any other failure means a process of ours owns the pipe (busy,
        // wedged, version skew): surface it unchanged — the pipe always
        // wins, so the file route is never reached behind it.
        Err(_) => Route::Daemon,
    }
}

/// One request, routed (`design-m5b.md` §3.1): the pipe first, and the
/// direct file route only after a positive double verdict — two probes,
/// 200 ms apart, with no control window either time.
fn request_routed(cmd: &str, data: &serde_json::Value) -> Result<Routed, String> {
    let deadline = Instant::now() + GUARD_BUDGET;
    let mut confirming = false;
    loop {
        let result = client::request(cmd, data.clone());
        let guard = if control::find_existing().is_none() {
            Guard::Absent
        } else if Instant::now() >= deadline {
            Guard::Unanswered
        } else {
            Guard::Present
        };
        match route(&result, guard) {
            Route::Daemon => match result {
                Ok(value) => return Ok(Routed::Daemon(value)),
                Err(err) => return Err(err.to_string()),
            },
            Route::Retry => {
                // Never sleep past the budget; the next probe then reports
                // `Guard::Unanswered` if the window is still there.
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(GUARD_RETRY.min(remaining));
            }
            Route::Offline => {
                if confirming {
                    return Ok(Routed::Offline);
                }
                confirming = true;
                std::thread::sleep(GUARD_CONFIRM);
            }
            Route::Hung => return Err(GUARD_HUNG.to_string()),
        }
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
    let data = match request_routed("settings.get", &no_data())? {
        Routed::Daemon(data) => data,
        // The same file and the same Run-key fill the daemon applies at its
        // wire edge (design-m5b §3.3): same shape, so scripts cannot tell.
        Routed::Offline => serde_json::to_value(offline::settings_get())
            .map_err(|err| format!("could not format settings: {err}"))?,
    };
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
    println!("engine_mode: {}", settings.engine_mode.as_str());
    println!("volume: {}", settings.volume);
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
        let status: AutostartStatus = match request_routed("autostart.set", &payload)? {
            Routed::Daemon(data) => serde_json::from_value(data)
                .map_err(|err| format!("engine sent an unreadable autostart status: {err}"))?,
            // The Run key is the oracle (design-m4 D10) and this write is
            // already direct: the same registry calls `cascade autostart
            // on|off` makes, so the status view is identical (design-m5b §3.2).
            Routed::Offline => {
                let exe = std::env::current_exe()
                    .map_err(|err| format!("cannot resolve the current executable: {err}"))?;
                if enabled {
                    registry::autostart_enable(&exe)
                        .map_err(|code| format!("could not write the Run key (error {code})"))?;
                } else {
                    registry::autostart_disable()
                        .map_err(|code| format!("could not remove the Run value (error {code})"))?;
                }
                let state = registry::autostart_state(&exe);
                AutostartStatus {
                    enabled: state.registered,
                    windows_disabled: state.windows_disabled,
                }
            }
        };
        println!("autostart = {}", if status.enabled { "on" } else { "off" });
        if status.windows_disabled {
            println!("  warning: disabled in Settings > Apps > Startup");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let (patch, canonical) = build_patch(key, value)?;
    let payload =
        serde_json::to_value(patch).map_err(|err| format!("could not build request: {err}"))?;
    let (settings, offline_route) = match request_routed("settings.set", &payload)? {
        Routed::Daemon(data) => (
            serde_json::from_value::<Settings>(data)
                .map_err(|err| format!("engine sent unreadable settings: {err}"))?,
            false,
        ),
        // Apply and persist in this process; with no live engine nothing
        // applies in-process, and a failed save is an error here so the CLI
        // never echoes a change that did not stick (design-m5b §3.3).
        Routed::Offline => (offline::settings_apply(patch)?, true),
    };
    println!("{key} = {canonical}");
    for note in set_notes(key, offline_route) {
        println!("  {note}");
    }
    // A contradictory-but-allowed combination applies and warns (D9).
    if let Some(warning) = settings.conflict_warning() {
        println!("  warning: {warning}");
    }
    Ok(ExitCode::SUCCESS)
}

/// The apply-semantics note after `settings set`: the daemon route says how
/// a live engine applies the value, the offline route says there is no live
/// engine yet (`design-m5b.md` §3.3 — honest echoes, same shape).
fn set_notes(key: &str, offline_route: bool) -> Vec<&'static str> {
    match (key, offline_route) {
        ("engine_mode", false) => vec!["daemon restarts to apply it if a video is playing"],
        ("engine_mode", true) => vec!["applied at the next start (no daemon running)"],
        ("volume", false) => vec!["applied to the playing video immediately (no restart)"],
        ("volume", true) => vec!["applied at the next load (no daemon running)"],
        _ => vec![],
    }
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
        "engine_mode" => {
            // Requested route, not the running one (design-engine-mode §4):
            // the CLI never claims which mode the engine ended up in.
            let mode = PresentationMode::parse(value)
                .ok_or_else(|| format!("engine_mode expects accurate|efficient, got {value}"))?;
            patch.engine_mode = Some(mode);
            mode.as_str().to_string()
        }
        "volume" => {
            // Strict at the CLI (design-volume §3): out-of-range is an
            // error here, never a silent clamp — the daemon clamps only
            // what a hand-crafted pipe client sends.
            let level = parse_volume(value)?;
            patch.volume = Some(level);
            level.to_string()
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
        "unknown setting key '{key}' (valid: power_profile, on_battery, engine_mode, \
         volume, pause.fullscreen, pause.maximized, pause.display_off, pause.session_lock, \
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

/// `0..=100`, the audio level (`design-volume.md` §2). Parsed as `u32`
/// first so `-1` and `abc` get the same readable error as `101`.
fn parse_volume(value: &str) -> Result<u8, String> {
    let level: u32 = value
        .trim()
        .parse()
        .map_err(|_| format!("volume expects a number 0-100, got {value}"))?;
    if level > 100 {
        return Err(format!("volume expects 0-100, got {value}"));
    }
    Ok(level as u8)
}

// --- Library commands (M4 S3, ipc.md §3 / design-m4 §4) ---------------------

/// `cascade library ls|add|rm|prune` — every subcommand needs a daemon
/// (single writer: the main loop, `ipc.md` §3). `list`/`remove` stay as
/// readable aliases (design-m5 §4).
fn cmd_library(args: &[String]) -> Result<ExitCode, String> {
    match args.first().map(String::as_str) {
        Some("ls") | Some("list") => library_list(&args[1..]),
        Some("add") => library_add(&args[1..]),
        Some("rm") | Some("remove") => library_remove(&args[1..]),
        Some("prune") => library_prune(&args[1..]),
        Some(other) => Err(format!("library expects ls|add|rm|prune, got {other}")),
        None => Err("library expects ls|add|rm|prune — see `cascade help`".to_string()),
    }
}

/// Fresh `library.list` — ordinals resolve against the live list at
/// execution time, never a cached one (design-m5 D1). Routed: with no
/// daemon the stored file answers (design-m5b §3.3).
fn fetch_library() -> Result<LibraryItemsData, String> {
    let data = match request_routed("library.list", &no_data())? {
        Routed::Daemon(data) => data,
        Routed::Offline => return Ok(offline::library_list()),
    };
    serde_json::from_value(data).map_err(|err| format!("engine sent an unreadable library: {err}"))
}

/// A CLI ordinal: a pure decimal number (`design-m5.md` D1). Numbers live
/// only in the CLI; the wire keeps the stable 4-hex id. `0` parses here
/// and is rejected by [`resolve_ordinal`] with a readable message.
fn parse_ordinal(token: &str) -> Option<usize> {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // A number too large for usize is not an ordinal (and not a path
    // either — it fails the matchers downstream with a clear error).
    token.parse().ok()
}

/// Resolve a 1-based ordinal against a freshly fetched list, in list
/// order — exactly the numbering `library ls` prints.
fn resolve_ordinal(items: &[LibraryItem], n: usize) -> Option<&LibraryItem> {
    if n == 0 {
        return None;
    }
    items.get(n - 1)
}

/// The readable failure for an ordinal that did not resolve — `0` and
/// "past the end" are different mistakes and say so.
fn ordinal_error(n: usize, len: usize) -> String {
    if n == 0 {
        "library numbers start at 1 — run `cascade library ls`".to_string()
    } else {
        format!("no library item {n} — the library has {len} item(s); run `cascade library ls`")
    }
}

fn library_list(args: &[String]) -> Result<ExitCode, String> {
    if !args.is_empty() {
        return Err("ls takes no arguments".to_string());
    }
    let items = fetch_library()?;
    if items.items.is_empty() {
        println!("library is empty — add files with `cascade library add <path>`");
        return Ok(ExitCode::SUCCESS);
    }
    // The `ID` column is the ordinal the other commands accept
    // (design-m5 §4): `set 3`, `library rm 3` — the wire id stays behind
    // `--id` for scripts.
    println!("ID  TITLE  RESOLUTION  FPS  LENGTH  CODEC  PATH");
    for (index, item) in items.items.iter().enumerate() {
        println!(
            "{}  {}  {}x{}  {}  {}  {}  {}",
            index + 1,
            item.title,
            item.width,
            item.height,
            item.fps
                .map(|fps| format!("{fps:.2}"))
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
    let payload = serde_json::to_value(LibraryAddData {
        paths: paths.clone(),
    })
    .map_err(|err| format!("could not build request: {err}"))?;
    let result: LibraryAddResult = match request_routed("library.add", &payload)? {
        Routed::Daemon(data) => serde_json::from_value(data)
            .map_err(|err| format!("engine sent an unreadable add result: {err}"))?,
        // The same shared core with the same probe (design-m5b §3.3): the
        // added/rejected/assessments output cannot drift from the daemon's.
        Routed::Offline => {
            let probe: daemon::ProbeFn = Box::new(cascade_media::probe);
            offline::library_add(paths, &probe)?
        }
    };
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

/// `cascade library rm <n|id|path>` (alias `remove`) — edits the list
/// only; the file on
/// disk is never touched (AGENTS §3.3).
fn library_remove(args: &[String]) -> Result<ExitCode, String> {
    if args.len() != 1 {
        return Err(
            "rm needs one number, id, or path: `cascade library rm <n|id|path>`".to_string(),
        );
    }
    let token = args[0].as_str();
    // The list doubles as the ordinal/id/path index and the confirmation
    // source (design-m5 D1: resolve fresh, send the stable id).
    let items = fetch_library()?;
    // Only a path-shaped token needs absolutising (the daemon's cwd is
    // not the CLI's); ordinals and ids match as-is.
    let absolute = if parse_ordinal(token).is_none() && !is_item_id(token) {
        Some(
            std::path::absolute(token)
                .map_err(|err| format!("cannot resolve {token}: {err}"))?
                .display()
                .to_string(),
        )
    } else {
        None
    };
    let ordinal = parse_ordinal(token);
    let item = resolve_remove_target(&items.items, token, absolute.as_deref().unwrap_or(""))
        .ok_or_else(|| match ordinal {
            Some(n) => ordinal_error(n, items.items.len()),
            None => format!("no library item matches '{token}' — run `cascade library ls`"),
        })?;
    let payload = serde_json::to_value(LibraryRemoveData {
        id: item.id.clone(),
    })
    .map_err(|err| format!("could not build request: {err}"))?;
    match request_routed("library.remove", &payload)? {
        Routed::Daemon(_) => {}
        // The same fresh-store remove as the daemon's arm; `None` means the
        // id vanished between the list and this call (a race), reported
        // with the daemon's exact wording.
        Routed::Offline => {
            offline::library_remove(&item.id)?.ok_or_else(|| {
                format!(
                    "no library item with id '{}' — run `cascade library list`",
                    item.id
                )
            })?;
        }
    }
    println!(
        "removed: {} ({}) — the file itself is untouched",
        item.title, item.path
    );
    Ok(ExitCode::SUCCESS)
}

/// Resolve `library rm <token>` against a fetched list (design-m5 D1):
/// an **in-range ordinal** wins, then a 4-hex id, then a path. Pure so the
/// precedence is unit-testable; `absolute_path` is only read by the path
/// branch (the caller computes it for path-shaped tokens only).
fn resolve_remove_target<'a>(
    items: &'a [LibraryItem],
    token: &str,
    absolute_path: &str,
) -> Option<&'a LibraryItem> {
    if let Some(n) = parse_ordinal(token) {
        if let Some(item) = resolve_ordinal(items, n) {
            return Some(item);
        }
        // A 4-hex id may be all digits ("9182"): an out-of-range number
        // falls through to the id match instead of failing outright.
        if is_item_id(token) {
            return items
                .iter()
                .find(|item| item.id.eq_ignore_ascii_case(token));
        }
        return None;
    }
    if is_item_id(token) {
        return items
            .iter()
            .find(|item| item.id.eq_ignore_ascii_case(token));
    }
    find_item_by_path(items, absolute_path)
}

/// `cascade library prune` — same drop rule as the start-time prune.
fn library_prune(args: &[String]) -> Result<ExitCode, String> {
    if !args.is_empty() {
        return Err("prune takes no arguments".to_string());
    }
    let result: LibraryPruneResult = match request_routed("library.prune", &no_data())? {
        Routed::Daemon(data) => serde_json::from_value(data)
            .map_err(|err| format!("engine sent an unreadable prune result: {err}"))?,
        // The daemon's exact drop rule against a freshly loaded store
        // (design-m5b §3.3).
        Routed::Offline => LibraryPruneResult {
            removed: offline::library_prune()?,
        },
    };
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
    let data = client::request("assess", payload).map_err(|err| err.to_string())?;
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
    println!("  cascade [run] [--detached] [--color RRGGBB] [--repaint 1-4]");
    println!("           [--host auto|workerw|layered] [--video <path>]");
    println!("           play this file behind the desktop icons;");
    println!("           --detached returns at once — the terminal may be closed");
    println!("  cascade doctor");
    println!("  cascade stop | restore | quit");
    println!("  cascade status [--json]");
    println!("  cascade pause | resume");
    println!("  cascade set <path|n> | --id <a3f2> [--monitor <id>]");
    println!("                                        play a file, a library number, or an id");
    println!("  cascade clear [--monitor <id>]        back to the colour background");
    println!("  cascade settings get [--json]");
    println!("  cascade settings set <key> <value>    keys: power_profile, on_battery,");
    println!("                                        engine_mode accurate|efficient,");
    println!("                                        volume 0-100 (0 = muted),");
    println!("                                        pause.fullscreen, pause.maximized,");
    println!("                                        pause.display_off, pause.session_lock,");
    println!("                                        pause.screensaver, pause.remote_session,");
    println!("                                        restore_wallpaper_on_exit, tray, autostart");
    println!("  cascade library ls                    list with numbers (alias: list)");
    println!("  cascade library add <path>...         files are referenced, never copied");
    println!("  cascade library rm <n|path>           remove by number (alias: remove;");
    println!("                                        file untouched — id also accepted)");
    println!("  cascade library prune                 drop entries whose files are gone");
    println!("  cascade assess [<file>] [--monitor <id>] [--json]");
    println!("                                        no file: device + recommendation;");
    println!("                                        with a file: + its warnings");
    println!("  cascade autostart on|off|status");
    println!(
        "  cascade install                    copy this exe to %LOCALAPPDATA%\\Cascade + PATH"
    );
    println!("  cascade uninstall [--purge]        remove the install (--purge also drops state)");
    println!("  cascade ui                          open the library window");
    println!("                                      (alias: --ui; needs a running engine");
    println!("                                      or offers to start it)");
    println!();
    println!("  run is the default. --autostart is accepted for the Run key.");
    println!("  Without --video, the last successfully played path replays.");
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- routing guard (design-m5b §3.1) --------------------------------

    #[test]
    fn route_truth_table_covers_every_attempt() {
        let ok: Result<serde_json::Value, client::ClientError> =
            Ok(serde_json::json!({"settings": {}}));
        let not_running: Result<serde_json::Value, client::ClientError> =
            Err(client::ClientError::NotRunning);
        let busy: Result<serde_json::Value, client::ClientError> = Err(client::ClientError::Busy);
        let timeout: Result<serde_json::Value, client::ClientError> =
            Err(client::ClientError::Timeout);

        // A daemon answered, or the failure is not NotRunning (busy, wedged,
        // version skew): the pipe route decides whatever the probe says —
        // the file route is never reached behind a live process.
        for guard in [Guard::Absent, Guard::Present, Guard::Unanswered] {
            assert_eq!(route(&ok, guard), Route::Daemon, "ok + {guard:?}");
            assert_eq!(route(&busy, guard), Route::Daemon, "busy + {guard:?}");
            assert_eq!(route(&timeout, guard), Route::Daemon, "timeout + {guard:?}");
        }

        // NotRunning: the window probe alone picks the branch.
        assert_eq!(route(&not_running, Guard::Absent), Route::Offline);
        assert_eq!(route(&not_running, Guard::Present), Route::Retry);
        assert_eq!(route(&not_running, Guard::Unanswered), Route::Hung);
    }

    #[test]
    fn offline_set_notes_are_honest_about_the_missing_engine() {
        // Daemon route: unchanged wording (design-engine-mode §3,
        // design-volume §3).
        assert_eq!(
            set_notes("engine_mode", false),
            vec!["daemon restarts to apply it if a video is playing"]
        );
        assert_eq!(
            set_notes("volume", false),
            vec!["applied to the playing video immediately (no restart)"]
        );
        // Offline route: same shape, honest apply semantics (design-m5b §3.3).
        assert_eq!(
            set_notes("engine_mode", true),
            vec!["applied at the next start (no daemon running)"]
        );
        assert_eq!(
            set_notes("volume", true),
            vec!["applied at the next load (no daemon running)"]
        );
        // Every other key carries no note on either route.
        assert!(set_notes("tray", false).is_empty());
        assert!(set_notes("on_battery", true).is_empty());
    }

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
        assert!(!is_daemon_role(&args(&["ui"])));
        // Run options only count after `run`; a bare flag is a CLI typo.
        assert!(!is_daemon_role(&args(&["--repaint", "2"])));
        assert!(!is_daemon_role(&args(&["--color", "FF0000"])));
        assert!(!is_daemon_role(&args(&["--video", "clip.mp4"])));
    }

    #[test]
    fn gui_role_is_exactly_the_two_ui_spellings() {
        assert!(is_gui_role(&args(&["ui"])));
        assert!(is_gui_role(&args(&["--ui"])));
        // `ui` with arguments stays the GUI role; the window itself takes
        // none (design-m5 §5) — anything else must keep its own stdio.
        assert!(is_gui_role(&args(&["ui", "extra"])));
        assert!(!is_gui_role(&args(&["status"])));
        assert!(!is_gui_role(&args(&["run"])));
        assert!(!is_gui_role(&args(&["uix"])));
        assert!(!is_gui_role(&[]));
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

        let (patch, canonical) = build_patch("engine_mode", "EFFICIENT").expect("valid");
        assert_eq!(patch.engine_mode, Some(PresentationMode::Efficient));
        assert_eq!(canonical, "efficient");

        let (patch, canonical) = build_patch("engine_mode", "accurate").expect("valid");
        assert_eq!(patch.engine_mode, Some(PresentationMode::Accurate));
        assert_eq!(canonical, "accurate");

        // Volume: the boundaries and the middle are valid (design-volume §2).
        for value in ["0", "50", "100"] {
            let (patch, canonical) = build_patch("volume", value).expect("valid");
            assert_eq!(patch.volume, Some(value.parse::<u8>().expect("number")));
            assert_eq!(canonical, value, "canonical spelling is the number");
        }

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
        assert!(build_patch("engine_mode", "fast").is_err());
        assert!(build_patch("engine_mode", "quality").is_err());
        assert!(build_patch("tray", "yes").is_err());
        assert!(build_patch("pause.fullscreen", "on").is_err());
        // Volume rejects everything outside 0..=100 — never a silent clamp.
        for bad in ["101", "255", "-1", "abc", "", "50.5", "1e2"] {
            assert!(
                build_patch("volume", bad).is_err(),
                "must reject volume {bad:?}"
            );
        }
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
    fn volume_values_are_numbers_from_zero_to_hundred() {
        assert_eq!(parse_volume("0").expect("zero"), 0, "the default is valid");
        assert_eq!(parse_volume("100").expect("full"), 100);
        assert_eq!(parse_volume(" 50 ").expect("trimmed"), 50);
        // Every rejection says what is valid (design-volume §3).
        for bad in ["101", "255", "-1", "abc"] {
            let err = parse_volume(bad).expect_err("must reject");
            assert!(err.contains("0-100"), "error must state the range: {err}");
        }
    }

    #[test]
    fn library_subcommands_are_validated() {
        // No/unknown subcommand fails before any pipe request is made.
        let err = cmd_library(&[]).expect_err("needs a subcommand");
        assert!(err.contains("ls|add|rm|prune"), "unexpected: {err}");
        let err = cmd_library(&args(&["frobnicate"])).expect_err("unknown");
        assert!(err.contains("frobnicate"), "unexpected: {err}");
        // Argument arity is checked before touching the daemon too.
        assert!(cmd_library(&args(&["ls", "extra"])).is_err());
        assert!(cmd_library(&args(&["list", "extra"])).is_err());
        assert!(cmd_library(&args(&["prune", "extra"])).is_err());
        assert!(cmd_library(&args(&["rm"])).is_err());
        assert!(cmd_library(&args(&["remove", "a", "b"])).is_err());
        assert!(cmd_library(&args(&["add"])).is_err());
    }

    #[test]
    fn detached_flag_is_stripped_once() {
        // The child never sees `--detached`; sibling flags pass through.
        let (detached, rest) =
            split_detached_flag(&args(&["--video", "clip.mp4", "--detached"])).expect("valid");
        assert!(detached);
        assert_eq!(rest, args(&["--video", "clip.mp4"]));

        let (detached, rest) = split_detached_flag(&args(&["--color", "FF0000"])).expect("valid");
        assert!(!detached, "no flag, no detach");
        assert_eq!(rest, args(&["--color", "FF0000"]));

        let err =
            split_detached_flag(&args(&["--detached", "--detached"])).expect_err("one flag only");
        assert!(err.contains("twice"), "unexpected: {err}");
    }

    #[test]
    fn detached_child_reenters_through_run() {
        // The child always re-enters as `run`: never the bare double-click
        // flow (design-m4 §6.2 may prompt to install) and never the
        // unknown-command arm of `dispatch`.
        assert_eq!(detached_child_args(&[]), args(&["run"]));
        assert_eq!(
            detached_child_args(&args(&["--video", "clip.mp4"])),
            args(&["run", "--video", "clip.mp4"])
        );
        let child = detached_child_args(&args(&["--color", "FF0000"]));
        assert_eq!(child.first().map(String::as_str), Some("run"));
        assert!(
            !child.iter().any(|arg| arg == "--detached"),
            "the flag never reaches the child"
        );
        assert!(is_daemon_role(&child), "the child is a daemon role");
    }

    #[test]
    fn library_ordinals_are_pure_decimal_numbers() {
        assert_eq!(parse_ordinal("1"), Some(1));
        assert_eq!(parse_ordinal("3"), Some(3));
        assert_eq!(parse_ordinal("0"), Some(0), "parses; resolve rejects it");
        assert_eq!(parse_ordinal("12"), Some(12));
        // Never a path, never an id, never a decorated number.
        assert_eq!(parse_ordinal("clip.mp4"), None);
        assert_eq!(parse_ordinal("a3f2"), None, "hex ids are not ordinals");
        assert_eq!(parse_ordinal(" 3"), None, "no trimming");
        assert_eq!(parse_ordinal("-1"), None);
        assert_eq!(parse_ordinal(""), None);
        assert_eq!(parse_ordinal("3x"), None);
        assert_eq!(
            parse_ordinal("99999999999999999999999999"),
            None,
            "too large for usize"
        );
    }

    #[test]
    fn ordinal_resolution_is_one_based_and_bounded() {
        let items = vec![
            item("a1b2", r"C:\v\one.mp4"),
            item("b2c3", r"C:\v\two.mp4"),
            item("c3d4", r"C:\v\three.mp4"),
        ];
        assert_eq!(resolve_ordinal(&items, 1).expect("first").id, "a1b2");
        assert_eq!(resolve_ordinal(&items, 3).expect("third").id, "c3d4");
        assert!(resolve_ordinal(&items, 0).is_none(), "0 is never a row");
        assert!(resolve_ordinal(&items, 4).is_none(), "past the end");
        assert!(resolve_ordinal(&[], 1).is_none(), "empty library");
    }

    #[test]
    fn set_options_accept_library_numbers() {
        // A bare number is an ordinal, not a path (design-m5 D1).
        let options = parse_set_options(&args(&["3"])).expect("number");
        assert_eq!(options.ordinal, Some(3));
        assert!(options.path.is_none());
        assert!(options.id.is_none());

        let options = parse_set_options(&args(&["3", "--monitor", r"\\.\DISPLAY1"]))
            .expect("number and monitor");
        assert_eq!(options.ordinal, Some(3));
        assert_eq!(options.monitor.as_deref(), Some(r"\\.\DISPLAY1"));

        // Exactly one target: number + path, number + id all fail.
        assert!(parse_set_options(&args(&["3", "clip.mp4"])).is_err());
        assert!(parse_set_options(&args(&["3", "--id", "a3f2"])).is_err());
        // `0` parses as a number and is rejected at resolution time with
        // a readable message, never sent on the wire.
        assert_eq!(
            parse_set_options(&args(&["0"])).expect("zero").ordinal,
            Some(0)
        );
    }

    #[test]
    fn remove_target_resolution_precedence() {
        let items = vec![
            item("a1b2", r"C:\v\one.mp4"),
            item("9182", r"C:\v\two.mp4"),
            item("c3d4", r"C:\v\three.mp4"),
        ];
        // In-range ordinal wins — even over an id that looks like one.
        assert_eq!(
            resolve_remove_target(&items, "2", "").expect("ordinal").id,
            "9182"
        );
        // Out-of-range digits fall through to the all-digit id match.
        assert_eq!(
            resolve_remove_target(&items, "9182", "").expect("id").id,
            "9182"
        );
        // Out-of-range digits with no matching id resolve to nothing.
        assert!(resolve_remove_target(&items, "77", "").is_none());
        assert!(resolve_remove_target(&items, "0", "").is_none());
        // Hex ids and paths keep working (the old names are aliases).
        assert_eq!(
            resolve_remove_target(&items, "C3D4", "")
                .expect("hex id")
                .id,
            "c3d4"
        );
        assert_eq!(
            resolve_remove_target(&items, "other.mp4", r"C:\v\three.mp4")
                .expect("path")
                .id,
            "c3d4"
        );
        assert!(resolve_remove_target(&items, "missing.mp4", r"C:\v\missing.mp4").is_none());
    }

    /// A library row for the pure resolvers above.
    fn item(id: &str, path: &str) -> LibraryItem {
        LibraryItem::new(
            id.to_string(),
            path.to_string(),
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
        )
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
