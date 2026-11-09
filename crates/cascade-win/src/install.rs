//! `cascade install` / `cascade uninstall` (design-m4 §6, D2/D5/D11).
//!
//! Install runs from wherever the exe currently is (Downloads, a USB stick,
//! the build output) and:
//!
//! 1. copies itself to `%LOCALAPPDATA%\Cascade\cascade.exe`,
//! 2. generates the `cascade.cmd` shim so `cascade ...` typed from PATH is
//!    synchronous in PowerShell 5.1 (§6.4),
//! 3. appends the directory to the **user** `Path` (kind preserved) and
//!    broadcasts `WM_SETTINGCHANGE`,
//! 4. points the `Run` key at the installed copy,
//! 5. starts the engine from the installed copy if none is running.
//!
//! `uninstall` reverses the steps and deletes only files this module
//! created. Anything else in the data directory (`measurements/`, logs,
//! user JSON without `--purge`) is left in place — Cascade never deletes
//! what it cannot recognize as its own (AGENTS §3.3, §3.12).

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::LPARAM;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, SendMessageTimeoutW, HWND_BROADCAST, IDYES, MB_ICONQUESTION, MB_YESNO,
    SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

use crate::{control, registry, state, wide};

/// The shipped binary name. AGENTS §3.14: the stem must stay exactly
/// `cascade` (7 chars) — renaming it requires a re-measurement first, so
/// this assertion is a guard, not documentation.
pub const EXE_NAME: &str = "cascade.exe";
/// The generated PowerShell 5.1 shim (design-m4 §6.1 step 2).
///
/// It lives in its own `bin\` subdirectory because a same-directory
/// `cascade.exe` shadows it: PATHEXT tries `.EXE` before `.CMD`, so *that*
/// directory — not the exe directory — is what goes on PATH (measured
/// 2026-11-09, `probe-s6.ps1`: `Get-Command cascade` resolved to
/// `cascade.exe`; see design-m4 §6.4 correction).
pub const SHIM_NAME: &str = "cascade.cmd";
/// Subdirectory of the data dir holding only the shim; the PATH entry.
pub const SHIM_DIR_NAME: &str = "bin";
/// Rename target for the self-delete Windows refuses on a running image.
const STALE_NAME: &str = "cascade.stale.exe";
/// The PATH entry in its literal form; we detect both this and the
/// expanded `%LOCALAPPDATA%`-resolved directory, case-insensitively.
const PATH_LITERAL: &str = "%LOCALAPPDATA%\\Cascade\\bin";

/// The `%LOCALAPPDATA%\Cascade` directory, or `None` when the environment
/// is unusable (AGENTS: no `unwrap()` in library code).
fn data_dir() -> Result<PathBuf, String> {
    state::data_dir()
        .ok_or_else(|| "cannot resolve %LOCALAPPDATA% (environment unusable)".to_string())
}

/// The path the installed copy lives at.
pub fn installed_exe() -> Option<PathBuf> {
    Some(state::data_dir()?.join(EXE_NAME))
}

fn same_path(a: &Path, b: &Path) -> bool {
    // Canonicalize when both exist (case, `\\?\`, 8.3); fall back to a
    // case-insensitive raw compare so a not-yet-created destination can
    // still be compared against a running image's path.
    let a = fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

/// `true` when this process **is** the installed copy (design-m4 §6.2:
/// which bare-invocation behaviour applies, and whether the installed exe
/// can be deleted outright or only deferred).
pub fn is_installed_here() -> bool {
    let (Some(dest), Ok(current)) = (installed_exe(), std::env::current_exe()) else {
        return false;
    };
    same_path(&current, &dest)
}

// ---------------------------------------------------------------------
// Pure helpers (unit-tested): PATH editing, shim content.
// ---------------------------------------------------------------------

/// The `cascade.cmd` body (design-m4 §6.1 step 2). Pure ASCII on purpose:
/// cmd.exe parses batch files in the OEM code page, so a non-ASCII username
/// must never enter this file — `%LOCALAPPDATA%` expands when cmd *runs* it
/// (cmd's own environment, always present on Windows 11).
pub fn shim_content() -> String {
    "@echo off\r\n\"%LOCALAPPDATA%\\Cascade\\cascade.exe\" %*\r\n".to_string()
}

/// Split a PATH value into trimmed, non-empty entries.
pub fn path_entries(raw: &str) -> Vec<String> {
    raw.split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(String::from)
        .collect()
}

fn entry_matches(entry: &str, dir: &str, literal: &str) -> bool {
    // A trailing backslash is decoration, not identity (`C:\a` == `C:\a\`).
    let entry = entry.trim().trim_end_matches('\\');
    let dir = dir.trim().trim_end_matches('\\');
    let literal = literal.trim().trim_end_matches('\\');
    entry.eq_ignore_ascii_case(dir) || entry.eq_ignore_ascii_case(literal)
}

/// `true` when the PATH already carries our directory — either the expanded
/// absolute path or the literal `%LOCALAPPDATA%\Cascade` form, either case,
/// with or without a trailing backslash.
pub fn path_has_dir(raw: &str, dir: &str, literal: &str) -> bool {
    path_entries(raw)
        .iter()
        .any(|entry| entry_matches(entry, dir, literal))
}

/// Append `dir` to `raw`. The caller must have checked [`path_has_dir`]
/// first; entries already present keep their exact text.
pub fn path_with_dir(raw: &str, dir: &str) -> String {
    let base = raw.trim().trim_end_matches(';');
    if base.is_empty() {
        dir.to_string()
    } else {
        format!("{base};{dir}")
    }
}

/// Drop every entry matching `dir` or `literal`, keeping the remaining
/// entries' text verbatim. Empty segments introduced by the user are
/// normalized away only when a write-back happens anyway.
pub fn path_without_dir(raw: &str, dir: &str, literal: &str) -> String {
    path_entries(raw)
        .iter()
        .filter(|entry| !entry_matches(entry, dir, literal))
        .cloned()
        .collect::<Vec<_>>()
        .join(";")
}

/// The shim-only subdirectory of the data dir (the PATH entry).
fn shim_dir(dir: &Path) -> PathBuf {
    dir.join(SHIM_DIR_NAME)
}

/// The `cascade.cmd` path inside the data dir.
fn shim_path(dir: &Path) -> PathBuf {
    shim_dir(dir).join(SHIM_NAME)
}

// ---------------------------------------------------------------------
// Win32 side effects.
// ---------------------------------------------------------------------

/// Ask every top-level window to re-read the environment (design-m4 §6.1
/// step 3) so newly launched processes see the PATH change. Non-fatal: a
/// timeout only means some window ignored the message; the registry value
/// is already correct and the next logon picks it up regardless.
fn broadcast_environment() {
    let name = wide::to_wide("Environment");
    let mut result = 0usize;
    // SAFETY: `name` is NUL-terminated, `result` is a valid out pointer,
    // and `HWND_BROADCAST`/`WM_SETTINGCHANGE` are constants. The message
    // text must travel as a pointer, hence the LPARAM cast.
    let sent = unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            name.as_ptr() as LPARAM,
            SMTO_ABORTIFHUNG,
            5_000,
            &mut result,
        )
    };
    if sent == 0 {
        println!("note: environment broadcast timed out (non-fatal).");
    }
}

fn read_path() -> (u32, String) {
    registry::read_string_raw(
        HKEY_CURRENT_USER,
        registry::ENVIRONMENT_KEY,
        registry::PATH_VALUE,
    )
    .unwrap_or((REG_SZ, String::new()))
}

/// Poll `condition` every 200 ms until it holds or `timeout` passes.
/// AGENTS §7: never sleep-and-hope — bounded polling with a deadline.
fn wait_for(condition: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Ask a running daemon to exit through its ladder (repaint first, AGENTS
/// §3.1) and wait for its control window to disappear — the window
/// existing is exactly "the daemon is alive".
pub fn stop_engine() -> Result<(), String> {
    if let Some(hwnd) = control::find_existing() {
        if !control::post(hwnd, control::WM_CASCADE_STOP) {
            return Err("could not reach the running daemon".to_string());
        }
        if !wait_for(
            || control::find_existing().is_none(),
            Duration::from_secs(15),
        ) {
            return Err("engine did not stop within 15 s".to_string());
        }
    }
    Ok(())
}

/// Start the engine from `exe` and wait (bounded) for its control window.
fn start_engine(exe: &Path) -> Result<(), String> {
    let mut command = Command::new(exe);
    command
        .arg("--autostart")
        .creation_flags(CREATE_NO_WINDOW)
        // Null stdio: the long-lived daemon must not hold the installer's
        // (possibly redirected) pipes open — otherwise a CLI caller reading
        // our stdout waits for the daemon's exit instead of ours.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .map_err(|err| format!("start engine: {err}"))?;
    if !wait_for(
        || control::find_existing().is_some(),
        Duration::from_secs(15),
    ) {
        return Err("engine did not appear within 15 s".to_string());
    }
    Ok(())
}

/// The D5 double-click prompt (design-m4 §6.2). Anything but an explicit
/// Yes (including a MessageBox failure) returns `false` — never installs
/// without consent.
pub fn confirm_install() -> bool {
    let text = wide::to_wide(
        "Cascade is not installed on this PC.\n\n\
         Install it to %LOCALAPPDATA%\\Cascade and add it to your PATH?\n\n\
         (You can remove it later with: cascade uninstall)",
    );
    let caption = wide::to_wide("Cascade");
    // SAFETY: both buffers are NUL-terminated UTF-16; a null owner makes
    // the box task-modal; the return value is only compared to IDYES.
    let answer = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_YESNO | MB_ICONQUESTION,
        )
    };
    answer == IDYES
}

// ---------------------------------------------------------------------
// Install / uninstall.
// ---------------------------------------------------------------------

/// Run the design-m4 §6.1 install steps, printing each one.
///
/// Failure at any step leaves a truthful partial state: files land before
/// PATH/autostart, and `uninstall` reverses any subset.
pub fn install_from(source: &Path) -> Result<(), String> {
    let dir = data_dir()?;
    let dest_exe = dir.join(EXE_NAME);

    // Leftovers an earlier deferred delete could not finish (§6.1
    // fallback): the renamed image and helper scripts in `%TEMP%`.
    sweep_leftovers(&dir);

    if same_path(source, &dest_exe) {
        println!("Installed copy is this executable: {}", dest_exe.display());
    } else {
        // A running image cannot be overwritten, so stop the engine first;
        // step 5 restarts it from the installed copy.
        if control::find_existing().is_some() {
            println!("Stopping the running engine...");
            stop_engine()?;
        }
        fs::create_dir_all(&dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
        fs::copy(source, &dest_exe)
            .map_err(|err| format!("copy {} -> {}: {err}", source.display(), dest_exe.display()))?;
        println!("Installed to {}", dest_exe.display());
    }

    // Step 2: the PowerShell 5.1 shim (D11), always rewritten. It sits in
    // `bin\` so no same-directory exe can shadow it (PATHEXT order).
    let bin = shim_dir(&dir);
    fs::create_dir_all(&bin).map_err(|err| format!("create {}: {err}", bin.display()))?;
    let shim = bin.join(SHIM_NAME);
    let content = shim_content();
    match fs::write(&shim, &content) {
        Ok(()) => println!("Shim written: {}", shim.display()),
        Err(err) => {
            // Running *through* the shim can hold the file open. The
            // content is a constant, so an existing identical file means
            // the on-disk state is already correct.
            match fs::read_to_string(&shim) {
                Ok(existing) if existing == content => {
                    println!("Shim already in place: {}", shim.display());
                }
                _ => return Err(format!("write {}: {err}", shim.display())),
            }
        }
    }

    // Step 3: user PATH with the registry kind preserved, then broadcast
    // so newly launched terminals pick it up (already-open ones do not).
    let entry_str = bin.to_string_lossy().into_owned();
    let (kind, raw) = read_path();
    if path_has_dir(&raw, &entry_str, PATH_LITERAL) {
        println!("PATH already contains {} (user).", bin.display());
    } else {
        let updated = path_with_dir(&raw, &entry_str);
        registry::write_string_typed(
            registry::ENVIRONMENT_KEY,
            registry::PATH_VALUE,
            kind,
            &updated,
        )
        .map_err(|rc| format!("PATH: registry write failed ({rc})"))?;
        broadcast_environment();
        println!("PATH updated (user). Already-open terminals will NOT see it —");
        println!("open a new terminal and type: cascade status");
    }

    // Step 4: autostart points at the installed copy.
    registry::autostart_enable(&dest_exe)
        .map_err(|rc| format!("autostart: registry write failed ({rc})"))?;
    println!("Autostart: on.");

    // Step 5: engine from the installed copy (it was stopped above when
    // the source differed; a same-file install keeps a running engine).
    if control::find_existing().is_some() {
        println!("Engine: already running.");
    } else {
        start_engine(&dest_exe)?;
        println!("Engine: running.");
    }
    Ok(())
}

/// Reverse the install (design-m4 §6.1), printing each step. Deletes only
/// files Cascade itself created; every other file in the data directory is
/// reported as left in place.
pub fn uninstall(purge: bool) -> Result<(), String> {
    let dir = data_dir()?;
    let bin = shim_dir(&dir);
    let entry_str = bin.to_string_lossy().into_owned();
    let running_from_dest = is_installed_here();

    // 1. engine down — the exit ladder repaints the desktop first.
    if control::find_existing().is_some() {
        stop_engine()?;
        println!("Engine: stopped.");
    } else {
        println!("Engine: not running.");
    }

    // 2. autostart off.
    registry::autostart_disable()
        .map_err(|rc| format!("autostart: registry delete failed ({rc})"))?;
    println!("Autostart: off.");

    // 3. PATH entry gone (either form), then broadcast.
    let (kind, raw) = read_path();
    if path_has_dir(&raw, &entry_str, PATH_LITERAL) {
        let updated = path_without_dir(&raw, &entry_str, PATH_LITERAL);
        registry::write_string_typed(
            registry::ENVIRONMENT_KEY,
            registry::PATH_VALUE,
            kind,
            &updated,
        )
        .map_err(|rc| format!("PATH: registry write failed ({rc})"))?;
        broadcast_environment();
        println!("PATH: removed (user).");
    } else {
        println!("PATH: not present (user).");
    }

    // 4. product files only (allowlist). Anything Windows refuses right
    // now — most often the image this very process runs from — is handed
    // to a hidden helper that retries after we exit.
    let mut deferred: Vec<PathBuf> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    remove_now_or_defer(&shim_path(&dir), &mut deferred);

    let exe = dir.join(EXE_NAME);
    if running_from_dest {
        // Windows cannot delete a running image, but renaming one is
        // legal; the helper then deletes the renamed file.
        let stale = dir.join(STALE_NAME);
        remove_now_or_defer(&stale, &mut deferred); // an older leftover
        match fs::rename(&exe, &stale) {
            Ok(()) => {
                println!(
                    "Removed {} (deferred: this process is the running image).",
                    exe.display()
                );
                deferred.push(stale);
            }
            Err(err) => {
                warnings.push(format!(
                    "could not rename the running image ({err}); deleting it after exit instead"
                ));
                deferred.push(exe.clone());
            }
        }
    } else {
        remove_now_or_defer(&exe, &mut deferred);
    }

    if purge {
        remove_now_or_defer(&dir.join("state.json"), &mut deferred);
        remove_now_or_defer(&dir.join("library.json"), &mut deferred);
        println!("Purged state.json and library.json.");
    } else {
        println!("Data kept: library.json, state.json (use --purge to remove).");
    }

    // 5. the directories only when they are already empty — unknown files
    // (measurements, logs) must keep their home.
    if fs::remove_dir(&bin).is_ok() {
        println!("Removed empty shim dir {}", bin.display());
    }
    if fs::remove_dir(&dir).is_ok() {
        println!("Removed empty data dir {}", dir.display());
    }

    if !deferred.is_empty() {
        match schedule_delete(&deferred, &[bin, dir]) {
            Ok(()) => println!(
                "Deferred delete of {} file(s) until this process exits.",
                deferred.len()
            ),
            Err(err) => {
                for path in &deferred {
                    warnings.push(format!("could not remove {}: {err}", path.display()));
                }
            }
        }
    }

    for warning in &warnings {
        println!("[warn] {warning}");
    }
    println!("Uninstall complete.");
    Ok(())
}

/// Remove `path` now; anything still present (running image, the shim
/// `cmd.exe` is reading) is queued for the deferred helper.
fn remove_now_or_defer(path: &Path, deferred: &mut Vec<PathBuf>) {
    match fs::remove_file(path) {
        Ok(()) => println!("Removed {}", path.display()),
        // Nothing to do — but a *missing* file is only fine when we did
        // not intend to rename it (callers check existence themselves).
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => deferred.push(path.to_path_buf()),
    }
}

/// A hidden `cmd` runs this helper in `%TEMP%`: wait for this process to
/// exit, remove the queued paths, remove the helper itself, then drop the
/// data directory if it ended up empty. Only ever names Cascade's own
/// files (AGENTS §3.12).
fn schedule_delete(paths: &[PathBuf], dirs: &[PathBuf]) -> Result<(), String> {
    let bat = std::env::temp_dir().join(format!("cascade-cleanup-{}.bat", std::process::id()));
    let mut script = String::from("@echo off\r\nping -n 3 127.0.0.1 >nul\r\n");
    for path in paths {
        script.push_str(&format!("del /f /q \"{}\" >nul 2>&1\r\n", path.display()));
    }
    for dir in dirs {
        script.push_str(&format!("rmdir \"{}\" >nul 2>&1\r\n", dir.display()));
    }
    script.push_str(&format!("del /f /q \"{}\" >nul 2>&1\r\n", bat.display()));
    fs::write(&bat, script).map_err(|err| format!("write {}: {err}", bat.display()))?;
    let mut command = Command::new("cmd");
    command
        .args(["/d", "/c"])
        .arg(&bat)
        .creation_flags(CREATE_NO_WINDOW)
        // Null stdio: the helper outlives this process; inheriting our
        // pipes would keep a caller's stdout open until the helper exits.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .map_err(|err| format!("spawn cleanup helper: {err}"))?;
    Ok(())
}

/// Remove leftovers from a previous deferred delete: the renamed image and
/// any helper script still sitting in `%TEMP%`.
fn sweep_leftovers(dir: &Path) {
    let stale = dir.join(STALE_NAME);
    if stale.exists() {
        match fs::remove_file(&stale) {
            Ok(()) => println!("Removed leftover {}", stale.display()),
            Err(err) => println!("[warn] leftover {} still present: {err}", stale.display()),
        }
    }
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("cascade-cleanup-") && name.ends_with(".bat") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_is_exact_ascii_and_uses_runtime_expansion() {
        // D11: cmd waits for this GUI child; AGENTS §3.14: stem stays 7.
        assert_eq!(
            shim_content(),
            "@echo off\r\n\"%LOCALAPPDATA%\\Cascade\\cascade.exe\" %*\r\n"
        );
        assert!(shim_content().is_ascii());
        assert_eq!(EXE_NAME, "cascade.exe");
        assert_eq!(SHIM_NAME, "cascade.cmd");
        // The shim sits in `bin\`: PATHEXT tries `.EXE` before `.CMD`, so a
        // same-directory exe would shadow it (measured, probe-s6.ps1).
        assert_eq!(SHIM_DIR_NAME, "bin");
        assert_eq!(PATH_LITERAL, "%LOCALAPPDATA%\\Cascade\\bin");
    }

    #[test]
    fn data_dir_name_matches_the_literal_forms() {
        // Guards drift between `state::data_dir`, the shim, and PATH
        // detection: all three must name the same directory.
        if let Some(dir) = state::data_dir() {
            assert!(dir.ends_with("Cascade"), "{}", dir.display());
        }
    }

    #[test]
    fn path_entries_split_trim_and_drop_empties() {
        assert_eq!(path_entries("a;; b ;"), vec!["a", "b"]);
        assert_eq!(path_entries(""), Vec::<String>::new());
    }

    #[test]
    fn path_detects_both_forms_case_and_slash_insensitively() {
        let dir = r"C:\Users\Zahraan Dzakii\AppData\Local\Cascade\bin";
        let literal = PATH_LITERAL;
        assert!(path_has_dir(&format!("X;{dir}"), dir, literal));
        assert!(path_has_dir(&format!("{dir}\\"), dir, literal));
        assert!(path_has_dir(&dir.to_uppercase(), dir, literal));
        assert!(path_has_dir(&format!("X;{literal}"), dir, literal));
        assert!(path_has_dir(&format!("{literal}\\"), dir, literal));
        assert!(!path_has_dir(r"C:\Windows;C:\Tools", dir, literal));
        assert!(!path_has_dir("", dir, literal));
    }

    #[test]
    fn path_append_keeps_existing_text_and_dedupes_by_precheck() {
        let dir = r"C:\A\Cascade\bin";
        assert_eq!(path_with_dir("", dir), dir);
        assert_eq!(path_with_dir(r"C:\X;", dir), format!(r"C:\X;{dir}"));
        // The caller skips the write when already present, so append never
        // has to defend against duplicates itself.
        assert!(path_has_dir(
            &path_with_dir(r"C:\X", dir),
            dir,
            PATH_LITERAL
        ));
    }

    #[test]
    fn path_removal_targets_only_our_entries() {
        let dir = r"C:\Users\Zahraan Dzakii\AppData\Local\Cascade\bin";
        let literal = PATH_LITERAL;
        let raw = format!(r"C:\X;{dir}\;C:\Y;{literal};C:\Z");
        assert_eq!(path_without_dir(&raw, dir, literal), r"C:\X;C:\Y;C:\Z");
        assert_eq!(path_without_dir(r"C:\X", dir, literal), r"C:\X");
        // Nothing to remove must round-trip to a still-valid PATH.
        assert_eq!(path_without_dir("", dir, literal), "");
    }
}
