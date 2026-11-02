//! `fontaine` — role dispatch only.
//!
//! All Win32 behaviour lives in `fontaine-win`; this binary parses argv and
//! reports results. See `docs/architecture.md` §2.

// GUI subsystem: starting the daemon must never create a console window
// (M1-F, measured flash of ~100 ms with focus steal on a console build).
// The CLI re-attaches to a console in `console::prepare_stdio`.
#![windows_subsystem = "windows"]

use std::process::ExitCode;

use fontaine_win::{console, control, daemon, doctor, dpi, host, registry};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // First: every path below may print.
    console::prepare_stdio(is_daemon_role(&args));
    dpi::set_per_monitor_v2();
    match dispatch(&args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("fontaine: {message}");
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
        Some("stop") => cmd_signal(control::WM_FONTAINE_STOP, "stop"),
        Some("restore") => cmd_signal(control::WM_FONTAINE_RESTORE, "restore"),
        Some("status") => cmd_status(),
        Some("autostart") => cmd_autostart(&args[1..]),
        Some("version") | Some("--version") | Some("-V") => {
            println!("fontaine {VERSION}");
            Ok(ExitCode::SUCCESS)
        }
        Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        Some("ui") | Some("--ui") => {
            println!("The library window is not implemented yet (milestone M5).");
            println!("Use `fontaine run` for the engine and `fontaine doctor` for diagnostics.");
            Ok(ExitCode::SUCCESS)
        }
        Some(other) => {
            print_help();
            Err(format!("unknown command: {other}"))
        }
    }
}

fn cmd_run(args: &[String]) -> Result<ExitCode, String> {
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
            other => return Err(format!("unknown option for run: {other}")),
        }
    }

    match daemon::run(options) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(err) => {
            // Best effort: a console close may already have invalidated the
            // handles, and a panic here would be reported as a crash.
            fontaine_win::derr!("fontaine: {err}");
            Ok(ExitCode::from(1))
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
        None => Err(format!("{label}: Fontaine is not running")),
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
    println!("fontaine {VERSION}");
    println!();
    println!("USAGE:");
    println!("  fontaine [run] [--color RRGGBB] [--repaint 1-4] [--host auto|workerw|layered]");
    println!("  fontaine doctor");
    println!("  fontaine stop | restore | status");
    println!("  fontaine autostart on|off|status");
    println!("  fontaine ui");
    println!();
    println!("  run is the default. --autostart is accepted for the Run key.");
}
