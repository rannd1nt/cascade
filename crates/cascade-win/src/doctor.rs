//! `cascade doctor` — one command that answers "why is it not working?".
//!
//! Runs in-process, is safe to execute while the daemon is running, and changes
//! nothing except asking Explorer for the wallpaper surface (the same message
//! the daemon sends before attaching).

use std::path::PathBuf;
use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetParent, GetWindow, GW_CHILD, GW_HWNDNEXT};

use cascade_core::device::{
    codec_support, level_name, profile_name, CodecSupport, DeviceProfile, MonitorProfile,
};

use crate::device;
use crate::dpi;
use crate::host;
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
    pub device: DeviceProfile,
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
        device: device::collect(),
        shell: shell_status(),
        wallpaper: wallpaper::snapshot(),
        autostart: registry::autostart_state(&exe),
        exe,
        default_host_color: default_color,
    }
}

impl DoctorReport {
    pub fn print(&self) {
        println!("Cascade doctor");
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

        println!("  monitors        {}", self.device.monitors.len());
        for m in &self.device.monitors {
            let extras = extras_label(m);
            let extras = if extras.is_empty() {
                String::new()
            } else {
                format!("  {extras}")
            };
            println!(
                "                  {}  {}x{} at ({},{}){}  work {}x{}{extras}",
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

        if self.device.gpus.is_empty() {
            println!("  gpu             (no adapter reported)");
        }
        for (index, gpu) in self.device.gpus.iter().enumerate() {
            let label = if index == 0 { "gpu" } else { "" };
            let driver = match &gpu.driver_version {
                Some(version) => format!("  driver {version}"),
                None => "  driver (not reported)".to_string(),
            };
            println!(
                "  {label:<15}{}  vendor {}  dedicated {} MB, shared {} MB{driver}",
                gpu.name,
                cascade_core::device::vendor_label(gpu.vendor_id),
                gpu.dedicated_mb,
                gpu.shared_mb
            );
        }

        match &self.device.cpu {
            Some(cpu) => {
                let name = match &cpu.name {
                    Some(name) => format!("{name}  "),
                    None => "(name not reported)  ".to_string(),
                };
                println!(
                    "  cpu             {name}{} cores / {} threads",
                    cpu.cores, cpu.threads
                );
            }
            None => println!("  cpu             (unavailable)"),
        }

        match &self.device.ram {
            Some(ram) => println!(
                "  ram             {} total, {} available",
                gigabytes(ram.total_mb),
                gigabytes(ram.available_mb)
            ),
            None => println!("  ram             (unavailable)"),
        }

        match &self.device.decode {
            Some(capability) => {
                println!(
                    "  decode          {} hardware decoder MFT{}",
                    capability.decoders.len(),
                    if capability.decoders.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                );
                println!(
                    "                  GPU decode: {}",
                    gpu_decode_line(&capability.gpu_codecs)
                );
                for (subtype, support) in codec_support(capability) {
                    println!("                  {}", decode_row(&subtype, &support));
                }
                if capability.decoders.is_empty() && capability.gpu_codecs.is_empty() {
                    println!(
                        "                  no hardware video decoder: playback is unavailable"
                    );
                }
            }
            None => println!("  decode          (unavailable)"),
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

/// `60 Hz, 96 dpi, SDR (BT.709), 8 bpc` — only the parts the OS reported.
/// Pure; unit-tested (slice ).
fn extras_label(m: &MonitorProfile) -> String {
    let mut parts = Vec::new();
    if let Some(hz) = m.refresh_hz {
        parts.push(format!("{hz} Hz"));
    }
    if let Some(dpi) = m.dpi {
        parts.push(format!("{dpi} dpi"));
    }
    if let Some(color_space) = &m.color_space {
        parts.push(color_space.clone());
    }
    if let Some(bpc) = m.bits_per_color {
        parts.push(format!("{bpc} bpc"));
    }
    parts.join(", ")
}

/// Megabytes as GB with one decimal (`16122` → `15.7 GB`). Pure; unit-tested.
fn gigabytes(megabytes: u64) -> String {
    format!("{:.1} GB", megabytes as f64 / 1024.0)
}

/// The `GPU decode:` line: sorted labels joined (`av01, h264, hevc`), or an
/// explicit `(none reported)` — an empty list means the D3D11 query ran and
/// found nothing, which is printed, not hidden. Pure; unit-tested.
fn gpu_decode_line(codecs: &[String]) -> String {
    if codecs.is_empty() {
        "(none reported)".to_string()
    } else {
        codecs.join(", ")
    }
}

/// One `decode` detail row: `h264: profiles Baseline, High; levels 3.1, 4.0`.
/// A decoder that advertises only the subtype prints that honestly instead
/// of empty lists. Pure; unit-tested.
fn decode_row(subtype: &str, support: &CodecSupport) -> String {
    if support.profiles.is_empty() && support.levels.is_empty() {
        return format!("{subtype}: supported (no profile/level advertised)");
    }
    let profiles: Vec<String> = support
        .profiles
        .iter()
        .map(|profile| profile_name(subtype, *profile))
        .collect();
    let levels: Vec<String> = support
        .levels
        .iter()
        .map(|level| level_name(subtype, *level))
        .collect();
    format!(
        "{subtype}: profiles {}; levels {}",
        profiles.join(", "),
        levels.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn yes_no_formats() {
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
    }

    #[test]
    fn gpu_decode_line_joins_sorted_labels_or_says_none() {
        let codecs = vec![
            "av01".to_string(),
            "h264".to_string(),
            "hevc".to_string(),
            "vp90".to_string(),
        ];
        assert_eq!(gpu_decode_line(&codecs), "av01, h264, hevc, vp90");
        // An empty list is reported honestly, not as a blank line.
        assert_eq!(gpu_decode_line(&[]), "(none reported)");
    }

    fn sample_monitor() -> MonitorProfile {
        MonitorProfile {
            id: r"\\.\DISPLAY1".to_string(),
            rect: cascade_core::Rect::new(0, 0, 1366, 768),
            work: cascade_core::Rect::new(0, 0, 1366, 728),
            primary: true,
            refresh_hz: None,
            dpi: None,
            hdr: None,
            bits_per_color: None,
            color_space: None,
        }
    }

    #[test]
    fn extras_label_only_lists_what_was_reported() {
        let mut monitor = sample_monitor();
        assert_eq!(extras_label(&monitor), "", "nothing reported → no suffix");

        monitor.refresh_hz = Some(60);
        monitor.dpi = Some(96);
        assert_eq!(extras_label(&monitor), "60 Hz, 96 dpi");

        monitor.color_space = Some("SDR (BT.709)".to_string());
        monitor.bits_per_color = Some(8);
        assert_eq!(extras_label(&monitor), "60 Hz, 96 dpi, SDR (BT.709), 8 bpc");
    }

    #[test]
    fn gigabytes_formats_one_decimal() {
        assert_eq!(gigabytes(16122), "15.7 GB");
        assert_eq!(gigabytes(4096), "4.0 GB");
        assert_eq!(gigabytes(0), "0.0 GB");
    }

    #[test]
    fn decode_row_names_profiles_and_levels_in_set_order() {
        let support = CodecSupport {
            profiles: BTreeSet::from([66, 100]),
            levels: BTreeSet::from([31, 40]),
        };
        assert_eq!(
            decode_row("h264", &support),
            "h264: profiles Baseline, High; levels 3.1, 4.0"
        );
    }

    #[test]
    fn decode_row_of_a_subtype_only_says_supported() {
        assert_eq!(
            decode_row("h264", &CodecSupport::default()),
            "h264: supported (no profile/level advertised)"
        );
    }
}
