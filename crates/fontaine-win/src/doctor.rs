//! `fontaine doctor` — one command that answers "why is it not working?".
//!
//! Runs in-process, is safe to execute while the daemon is running, and changes
//! nothing except asking Explorer for the wallpaper surface (the same message
//! the daemon sends before attaching).

use std::path::PathBuf;
use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetParent, GetWindow, GW_CHILD, GW_HWNDNEXT};

use crate::dpi;
use crate::host;
use crate::monitor::{self, MonitorInfo};
use crate::registry::{self, AutostartState};
use crate::shell::{self, ShellSurface};
use crate::system::{self, OsInfo, SessionInfo};
use crate::wallpaper::{self, WallpaperSnapshot};

#[derive(Clone, Debug, Default)]
pub struct ShellStatus {
    pub progman: Option<usize>,
    pub progman_ex_style: u32,
    pub workerw: Option<usize>,
    pub defview: Option<usize>,
    pub defview_ex_style: u32,
    pub mode: Option<&'static str>,
    pub workerw_parent_is_progman: bool,
    pub tree: Vec<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DoctorReport {
    pub version: String,
    pub os: OsInfo,
    pub dpi: &'static str,
    pub session: SessionInfo,
    pub monitors: Vec<MonitorInfo>,
    pub shell: ShellStatus,
    pub wallpaper: WallpaperSnapshot,
    pub autostart: AutostartState,
    pub exe: PathBuf,
    pub default_host_color: u32,
}

fn walk_tree(hwnd: HWND, depth: usize, highlight: Option<HWND>, out: &mut Vec<String>) {
    if depth > 2 {
        return;
    }
    // SAFETY: standard child enumeration for a live window.
    let mut child = unsafe { GetWindow(hwnd, GW_CHILD) };
    let mut seen = 0;
    while !child.is_null() && seen < 12 {
        let class = shell::class_name(child);
        let marker = if Some(child) == highlight {
            "  <- wallpaper surface"
        } else {
            ""
        };
        out.push(format!(
            "{}{class} 0x{:x}{marker}",
            "  ".repeat(depth + 1),
            child as usize
        ));
        walk_tree(child, depth + 1, highlight, out);
        // SAFETY: `child` is a live window from the enumeration above.
        child = unsafe { GetWindow(child, GW_HWNDNEXT) };
        seen += 1;
    }
}

fn shell_status() -> ShellStatus {
    let mut status = ShellStatus::default();

    let progman = match shell::progman() {
        Some(h) => h,
        None => {
            status.error = Some("Progman not found; Explorer may still be starting".into());
            return status;
        }
    };
    status.progman = Some(progman as usize);
    status.progman_ex_style = shell::ex_style(progman);
    status.defview = shell::defview(progman).map(|h| h as usize);
    if let Some(view) = status.defview {
        status.defview_ex_style = shell::ex_style(view as HWND);
    }

    match shell::discover(Duration::from_millis(1_500)) {
        Ok(surface) => {
            let ShellSurface {
                progman,
                workerw,
                defview,
                mode,
            } = surface;
            status.workerw = Some(workerw as usize);
            status.mode = Some(mode.label());
            // SAFETY: GetParent only inspects the handle.
            status.workerw_parent_is_progman = unsafe { GetParent(workerw) } == progman;
            let _ = defview;
            walk_tree(progman, 0, Some(workerw), &mut status.tree);
        }
        Err(err) => {
            status.error = Some(err.to_string());
            walk_tree(progman, 0, None, &mut status.tree);
        }
    }
    status
}

pub fn collect(version: &str) -> DoctorReport {
    let exe = std::env::current_exe().unwrap_or_default();
    let default_color = host::HostOptions::default().color;
    DoctorReport {
        version: version.to_string(),
        os: system::os_info(),
        dpi: dpi::thread_awareness(),
        session: system::session_info(),
        monitors: monitor::enumerate(),
        shell: shell_status(),
        wallpaper: wallpaper::snapshot(),
        autostart: registry::autostart_state(&exe),
        exe,
        default_host_color: default_color,
    }
}

impl DoctorReport {
    pub fn print(&self) {
        println!("Fontaine doctor");
        println!("  version         {}", self.version);
        println!("  os              {}", self.os);
        println!("  dpi awareness   {} (wanted: per-monitor-v2)", self.dpi);
        println!(
            "  session         id {} ({})",
            self.session.id,
            if self.session.remote {
                "remote"
            } else {
                "local"
            }
        );
        println!("  executable      {}", self.exe.display());

        println!("  monitors        {}", self.monitors.len());
        for m in &self.monitors {
            println!(
                "                  {}  {}x{} at ({},{}){}  work {}x{}",
                m.id,
                m.rect.w,
                m.rect.h,
                m.rect.x,
                m.rect.y,
                if m.primary { "  primary" } else { "" },
                m.work.w,
                m.work.h
            );
        }

        println!("  shell");
        match self.shell.progman {
            Some(h) => println!(
                "                  Progman      0x{h:x}  ex=0x{:08x}",
                self.shell.progman_ex_style
            ),
            None => println!("                  Progman      not found"),
        }
        match self.shell.defview {
            Some(h) => println!(
                "                  DefView      0x{h:x}  ex=0x{:08x}",
                self.shell.defview_ex_style
            ),
            None => println!("                  DefView      not found"),
        }
        match self.shell.workerw {
            Some(h) => println!(
                "                  WorkerW      0x{h:x}{}",
                if self.shell.workerw_parent_is_progman {
                    "  (child of Progman)"
                } else {
                    "  (sibling layout)"
                }
            ),
            None => println!("                  WorkerW      not found"),
        }
        if let Some(err) = &self.shell.error {
            println!("                  error        {err}");
        }
        if !self.shell.tree.is_empty() {
            println!("                  hierarchy");
            for line in &self.shell.tree {
                println!("                  {line}");
            }
        }

        println!("  wallpaper");
        match &self.wallpaper.path {
            Some(p) => println!("                  path         {p}"),
            None => println!("                  path         (none)"),
        }
        println!(
            "                  background   {}",
            self.wallpaper
                .background_type
                .map(|t| t.to_string())
                .unwrap_or_else(|| "unknown".into())
        );
        println!("                  source       {}", self.wallpaper.source);

        println!("  autostart");
        println!(
            "                  registered   {}",
            yes_no(self.autostart.registered)
        );
        if let Some(cmd) = &self.autostart.command {
            println!("                  command      {cmd}");
        }
        if self.autostart.registered && !self.autostart.points_to_current_exe {
            println!("                  warning      points to a different executable");
        }
        if self.autostart.windows_disabled {
            println!("                  warning      disabled in Settings > Apps > Startup");
        } else if !self.autostart.approval_unknown {
            println!("                  windows      enabled (or never toggled)");
        }

        println!(
            "  host strategy   {}  (solid colour {})",
            self.shell.mode.unwrap_or("unavailable"),
            host::rgb_hex(self.default_host_color)
        );
    }
}

fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}
