//! Re-exec of this binary after a transition (`design-m2.md` §4,
//! `design-m5b.md` §4.4 T2/T3).
//!
//! Shared by the full daemon (a respawn-triggering `wallpaper.set`), the
//! battery listener (T3 — AC confirmed), and the CLI (`main.rs` re-uses
//! [`respawn_child`]). One implementation, one argv contract: the child
//! inherits everything except `--video` (it reads the new path from
//! `state.json`) and sets `CASCADE_RESPAWN` so it retries the session
//! pipe until this process releases it (`daemon::create_respawn_pipe`).

use std::ffi::OsString;

use crate::dlog;

/// Spawn this binary as a child with `CASCADE_RESPAWN=1`. Returns the
/// child's pid. A failed spawn leaves the desktop as it is (the listener
/// never touched it; the daemon already ran its exit ladder) — the caller
/// logs the error and decides whether to retry.
pub fn spawn_child(args: Vec<OsString>) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|err| format!("respawn: {err}"))?;
    match std::process::Command::new(exe)
        .args(args)
        .env("CASCADE_RESPAWN", "1")
        .spawn()
    {
        Ok(child) => {
            dlog!("cascade: respawned as pid {}", child.id());
            Ok(child.id())
        }
        Err(err) => Err(format!("respawn: {err}")),
    }
}

/// Re-exec this binary after a respawn-triggering event. The child
/// inherits this process's argv minus `--video`, with `--listen` rewritten
/// to `run` so a transition child never lands back in the listener
/// (R3, `design-m5b.md` §7).
pub fn respawn_child() -> Result<u32, String> {
    spawn_child(respawn_args())
}

/// The respawn argv: this process's arguments minus `--video` and its
/// value, with `--listen` rewritten to `run`. Pure so the filter is
/// unit-testable without touching argv.
pub fn respawn_args() -> Vec<OsString> {
    rewrite_listen_flag(filter_video_flag(std::env::args_os().skip(1).collect()))
}

/// Drop `--video` and its value from the raw argv. Position anywhere,
/// other options untouched (`design-m2.md` §4).
pub fn filter_video_flag(raw: Vec<OsString>) -> Vec<OsString> {
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

/// Rewrite a leading `--listen` to `run` so a transition child spawned
/// from the listener takes the full-daemon entry path (`cmd_run`) instead
/// of looping back into `cmd_listen` (R3, `design-m5b.md` §7). Only the
/// first argument is checked — `--listen` takes no value, so a later
/// occurrence is an option value, not the command word.
pub fn rewrite_listen_flag(raw: Vec<OsString>) -> Vec<OsString> {
    let mut out = raw;
    if out.first().is_some_and(|arg| arg == "--listen") {
        out[0] = OsString::from("run");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn respawn_filter_drops_video_flag_and_keeps_everything_else() {
        // `--video` and its value go; the child reads `state.json` instead
        // (design-m2 §4). Position anywhere, other options untouched.
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
    fn rewrite_listen_flag_rewrites_only_the_leading_command_word() {
        // R3 (`design-m5b.md` §7): a transition child spawned from the
        // listener must land in `cmd_run`, never back in `cmd_listen`.
        assert_eq!(rewrite_listen_flag(raw(&["--listen"])), raw(&["run"]));
        assert_eq!(
            rewrite_listen_flag(raw(&["--listen", "--video", r"C:\a.mp4"])),
            raw(&["run", "--video", r"C:\a.mp4"])
        );
        // A later `--listen` is an option value, not the command word.
        assert_eq!(
            rewrite_listen_flag(raw(&["run", "--host", "--listen"])),
            raw(&["run", "--host", "--listen"])
        );
        assert_eq!(rewrite_listen_flag(raw(&["run"])), raw(&["run"]));
        assert_eq!(rewrite_listen_flag(raw(&[])), raw(&[]));
    }

    #[test]
    fn respawn_args_combine_both_filters() {
        // The full contract: drop `--video`, rewrite `--listen` → `run`.
        assert_eq!(
            rewrite_listen_flag(filter_video_flag(raw(&[
                "--listen",
                "--video",
                r"C:\a.mp4"
            ]))),
            raw(&["run"])
        );
    }
}
