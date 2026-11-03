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

use cascade_core::MediaBackend;
use cascade_media::MediaEngineBackend;
use cascade_win::{console, control, daemon, doctor, dpi, host, registry, state};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // First: every path below may print.
    console::prepare_stdio(is_daemon_role(&args));
    dpi::set_per_monitor_v2();
    match dispatch(&args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("cascade: {message}");
            ExitCode::from(2)
        }
    }
}

/// `run`, `--autostart`, and the bare form start the long-lived daemon;
/// everything else is a one-shot CLI command that must print.
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
        Some("restore") => cmd_signal(control::WM_CASCADE_RESTORE, "restore"),
        Some("status") => cmd_status(),
        Some("autostart") => cmd_autostart(&args[1..]),
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
    match daemon::run(options, backend) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(err) => {
            // Best effort: a console close may already have invalidated the
            // handles, and a panic here would be reported as a crash.
            cascade_win::derr!("cascade: {err}");
            Ok(ExitCode::from(1))
        }
    }
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

/// Build the Media Foundation backend, but only when a source exists:
/// constructing it runs `MFStartup`, which a colour-only run must not pay
/// for (design-m2 §2.2). A construction failure degrades to the colour
/// background instead of refusing to start (AGENTS §3.1).
fn build_backend(source: Option<&Path>) -> Option<Box<dyn MediaBackend>> {
    let _ = source?;
    match MediaEngineBackend::new() {
        Ok(backend) => Some(Box::new(backend)),
        Err(err) => {
            cascade_win::derr!("cascade: media backend unavailable: {err}");
            None
        }
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

fn cmd_status() -> Result<ExitCode, String> {
    match control::find_existing() {
        Some(hwnd) => {
            println!("engine: running");
            println!("control window: 0x{:x}", hwnd as usize);
            println!("mode: attached (detailed status arrives with the IPC layer)");
        }
        None => {
            println!("engine: not running");
        }
    }
    Ok(ExitCode::SUCCESS)
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

fn print_help() {
    println!("cascade {VERSION}");
    println!();
    println!("USAGE:");
    println!("  cascade [run] [--color RRGGBB] [--repaint 1-4] [--host auto|workerw|layered]");
    println!("           [--video <path>]   play this file behind the desktop icons");
    println!("  cascade doctor");
    println!("  cascade stop | restore | status");
    println!("  cascade autostart on|off|status");
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
}
