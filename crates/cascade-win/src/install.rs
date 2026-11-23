//! `cascade install` / `cascade uninstall`.
//!
//! Install runs from wherever the exe currently is (Downloads, a USB stick,
//! the build output) and:
//!
//! 1. copies itself to `%LOCALAPPDATA%\Cascade\cascade.exe`,
//! 2. generates the `cascade.cmd` shim so `cascade...` typed from PATH is
//!    synchronous in PowerShell 5.1,
//! 3. appends the directory to the **user** `Path` (kind preserved) and
//!    broadcasts `WM_SETTINGCHANGE`,
//! 4. points the `Run` key at the installed copy, creates per-user Start Menu
//!    and Desktop `Cascade.lnk` shortcuts, and registers one per-user
//!    Installed-apps key,
//! 5. starts the engine from the installed copy if none is running.
//!
//! `uninstall` reverses the steps and deletes only files this module
//! created. Anything else in the data directory (`measurements/`, logs,
//! user JSON without `--purge`) and any other shell link or registry key
//! is left in place — Cascade never deletes what it cannot recognize as
//! its own.

use std::ffi::c_void;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::core::{Interface, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHChangeNotify, SHGetKnownFolderPath,
    ShellLink, KF_FLAG_DEFAULT, SHCNE_ASSOCCHANGED, SHCNF_IDLIST,
};
use windows_sys::Win32::Foundation::LPARAM;
use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, SendMessageTimeoutW, HWND_BROADCAST, IDYES, MB_ICONQUESTION, MB_YESNO,
    SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

use crate::{control, registry, state, wide};

/// The shipped binary name: the stem must stay exactly
/// `cascade` (7 chars) — renaming it requires a re-measurement first, so
/// this assertion is a guard, not documentation.
pub const EXE_NAME: &str = "cascade.exe";
/// The generated PowerShell 5.1 shim (step 2).
///
/// It lives in its own `bin\` subdirectory because a same-directory
/// `cascade.exe` shadows it: PATHEXT tries `.EXE` before `.CMD`, so *that*
/// directory — not the exe directory — is what goes on PATH (measured
/// 2026-11-09, `probe-s6.ps1`: `Get-Command cascade` resolved to
/// `cascade.exe`; see correction).
pub const SHIM_NAME: &str = "cascade.cmd";
/// Subdirectory of the data dir holding only the shim; the PATH entry.
pub const SHIM_DIR_NAME: &str = "bin";
/// Rename target for the self-delete Windows refuses on a running image.
const STALE_NAME: &str = "cascade.stale.exe";
/// The PATH entry in its literal form; we detect both this and the
/// expanded `%LOCALAPPDATA%`-resolved directory, case-insensitively.
const PATH_LITERAL: &str = "%LOCALAPPDATA%\\Cascade\\bin";
/// Per-user shell link created by install. Only
/// this exact name in the two shell folders may be touched by uninstall.
pub const SHORTCUT_NAME: &str = "Cascade.lnk";
/// The shortcut is a thin launcher over the existing GUI CLI role
/// CLI first, tray and GUI are layers over the same paths).
const SHORTCUT_ARGS: &str = "ui";
const SHORTCUT_DESCRIPTION: &str = "Cascade library";
/// The one per-user Apps-entry key this module owns.
/// Only this exact subkey may be deleted by uninstall.
pub const APPS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Cascade";
/// Display values written to the Apps entry. No invented company name:
/// the project is published under its own name.
pub const APPS_DISPLAY_NAME: &str = "Cascade";
pub const APPS_PUBLISHER: &str = "Cascade";

/// The `%LOCALAPPDATA%\Cascade` directory, or `None` when the environment
/// is unusable : no `unwrap()` in library code).
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

/// `true` when this process **is** the installed copy :
/// which bare-invocation behaviour applies, and whether the installed exe
/// can be deleted outright or only deferred).
pub fn is_installed_here() -> bool {
    let (Some(dest), Ok(current)) = (installed_exe(), std::env::current_exe()) else {
        return false;
    };
    same_path(&current, &dest)
}

//
// Pure helpers (unit-tested): PATH editing, shim content.
//

/// The `cascade.cmd` body (step 2). Pure ASCII on purpose:
/// cmd.exe parses batch files in the OEM code page, so a non-ASCII username
/// must never enter this file — `%LOCALAPPDATA%` expands when cmd *runs* it
/// (cmd's own environment, always present on Windows 11).
pub fn shim_content() -> String {
    "@echo off\r\n\"%LOCALAPPDATA%\\Cascade\\cascade.exe\" %*\r\n".to_string()
}

/// Command-line arguments the shell links pass: the GUI role only.
pub fn shortcut_arguments() -> &'static str {
    SHORTCUT_ARGS
}

/// Description shown in Start search and in the link's properties.
pub fn shortcut_description() -> &'static str {
    SHORTCUT_DESCRIPTION
}

/// The only file name this module creates in a shell folder.
pub fn shortcut_path(folder: &Path) -> PathBuf {
    folder.join(SHORTCUT_NAME)
}

/// `true` only for the exact name we own — an unrelated `Cascade Notes.lnk`
/// must never be removed by uninstall.
pub fn is_our_shortcut(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(SHORTCUT_NAME))
}

/// The `UninstallString` (and `QuietUninstallString`) written to the Apps
/// entry. `--quiet` selects the silent stdio strategy so Windows Settings,
/// which launches us with no console, never flashes a terminal window
/// Pure so the exact command is unit-tested.
pub fn uninstall_command(exe: &Path) -> String {
    format!("\"{}\" uninstall --quiet", exe.display())
}

/// The `DisplayIcon` value: the exe holds the Cascade icon at index 0
/// (build.rs); Windows expects `<path>,0` for a PE resource.
pub fn display_icon(exe: &Path) -> String {
    format!("{},0", exe.display())
}

/// `EstimatedSize` is the exe size in KiB, rounded up. Saturates at DWORD
/// range so a pathological file never wraps into a wrong, smaller value.
pub fn estimated_size_kb(bytes: u64) -> u32 {
    bytes.div_ceil(1024).min(u32::MAX as u64) as u32
}

/// Windows paths are UTF-16; `to_string_lossy` would corrupt a path with
/// unpaired surrogates, so encode the `OsStr` directly.
fn path_wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
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

//
// Win32 side effects.
//

/// Ask every top-level window to re-read the environment (
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

//
// Per-user shell links.
//

/// A COM apartment for the short-lived shell operations in install and
/// uninstall. `IShellLinkW` is an STA object; no COM call happens on this
/// CLI thread before `init`, so a successful S_OK is always ours to release.
struct ComApartment {
    owned: bool,
}

impl ComApartment {
    fn init() -> Result<Self, String> {
        // SAFETY: the reserved argument must be null;
        // COINIT_APARTMENTTHREADED is the documented STA mode ShellLink
        // expects, and this one-shot CLI thread has no prior apartment.
        let hr =
            unsafe { CoInitializeEx(std::ptr::null::<c_void>(), COINIT_APARTMENTTHREADED as u32) };
        if hr < 0 {
            return Err(format!("CoInitializeEx failed (0x{hr:08X})"));
        }
        // S_FALSE means an apartment already existed: it is not ours and
        // must not be uninitialized below.
        Ok(Self { owned: hr == 0 })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balances the S_OK CoInitializeEx above on the same
            // thread, after every shell object has gone out of scope.
            unsafe { CoUninitialize() };
        }
    }
}

/// Resolve a per-user shell folder. The returned string is shell-owned.
fn known_folder(id: &windows::core::GUID) -> Result<PathBuf, String> {
    // SAFETY: `id` is a static FOLDERID_* GUID; on success the shell
    // returns a CoTaskMem-owned, NUL-terminated PWSTR, freed below while
    // this COM apartment is still alive.
    let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }
        .map_err(|err| format!("known folder lookup: {}", err.message()))?;
    // SAFETY: the shell returned a NUL-terminated PWSTR, valid until the
    // CoTaskMemFree call below.
    let result = PathBuf::from(unsafe { wide::from_wide_ptr(path.as_ptr()) });
    // SAFETY: `path` came from SHGetKnownFolderPath's CoTaskMem
    // allocation and is passed to CoTaskMemFree exactly once.
    unsafe { CoTaskMemFree(Some(path.as_ptr().cast::<c_void>())) };
    Ok(result)
}

/// Tell the shell that link files changed, so Start search refreshes
/// without requiring a logoff. Non-fatal: the files are already correct.
fn notify_shell() {
    // SAFETY: SHCNF_IDLIST means neither item pointer is read, so passing
    // None for both is the documented "associations changed" broadcast.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// Create or overwrite one `.lnk` in `folder`, pointing at the installed
/// exe with the GUI role. `Save(..., true)` makes re-install idempotent.
fn make_shortcut(exe: &Path, folder: &Path) -> Result<PathBuf, String> {
    let target = path_wide(exe);
    let working = path_wide(exe.parent().unwrap_or_else(|| Path::new("")));
    let args = wide::to_wide(SHORTCUT_ARGS);
    let description = wide::to_wide(SHORTCUT_DESCRIPTION);
    let lnk = shortcut_path(folder);
    let saved = path_wide(&lnk);

    // SAFETY: both arguments are documented shell constants; the object
    // is created inside ComApartment and released with the local values
    // before that apartment is dropped.
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|err| format!("create shell link: {}", err.message()))?;
    // SAFETY: every PCWSTR points at a NUL-terminated buffer that outlives
    // the call; IShellLinkW copies the strings into the link. Index 0 is
    // the exe's embedded Cascade resource (build.rs).
    unsafe {
        link.SetPath(PCWSTR::from_raw(target.as_ptr()))
            .map_err(|err| format!("shortcut SetPath: {}", err.message()))?;
        link.SetArguments(PCWSTR::from_raw(args.as_ptr()))
            .map_err(|err| format!("shortcut SetArguments: {}", err.message()))?;
        link.SetWorkingDirectory(PCWSTR::from_raw(working.as_ptr()))
            .map_err(|err| format!("shortcut SetWorkingDirectory: {}", err.message()))?;
        link.SetDescription(PCWSTR::from_raw(description.as_ptr()))
            .map_err(|err| format!("shortcut SetDescription: {}", err.message()))?;
        link.SetIconLocation(PCWSTR::from_raw(target.as_ptr()), 0)
            .map_err(|err| format!("shortcut SetIconLocation: {}", err.message()))?;
    }
    // QueryInterface on the same object; both wrappers release on drop.
    let persist: IPersistFile = link
        .cast()
        .map_err(|err| format!("shortcut IPersistFile: {}", err.message()))?;
    // SAFETY: `saved` is NUL-terminated and alive for the call; Save's
    // `true` commits the file we just created/overwrote.
    unsafe { persist.Save(PCWSTR::from_raw(saved.as_ptr()), true) }
        .map_err(|err| format!("save {}: {}", lnk.display(), err.message()))?;
    Ok(lnk)
}

/// Install both links. Failure is surfaced to `cascade install`: a
/// half-installed shell integration must not be reported as success.
fn install_shortcuts(exe: &Path) -> Result<(), String> {
    let _apartment = ComApartment::init()?;
    let targets: [(&windows::core::GUID, &str); 2] = [
        (&FOLDERID_Programs, "Start Menu"),
        (&FOLDERID_Desktop, "Desktop"),
    ];
    for (id, label) in targets {
        let folder = known_folder(id)?;
        let lnk = make_shortcut(exe, &folder)?;
        println!("{label} shortcut: {}", lnk.display());
    }
    notify_shell();
    Ok(())
}

/// Remove only the two links this module owns. A missing link is a no-op;
/// unrelated files in either folder are never enumerated or deleted.
fn remove_shortcuts() -> Result<(), String> {
    let _apartment = ComApartment::init()?;
    let targets: [(&windows::core::GUID, &str); 2] = [
        (&FOLDERID_Programs, "Start Menu"),
        (&FOLDERID_Desktop, "Desktop"),
    ];
    for (id, label) in targets {
        let folder = known_folder(id)?;
        let lnk = shortcut_path(&folder);
        if !is_our_shortcut(&lnk) {
            return Err("refusing to remove a shortcut that is not ours".to_string());
        }
        match fs::remove_file(&lnk) {
            Ok(()) => println!("Removed {label} shortcut: {}", lnk.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                println!("{label} shortcut: not present");
            }
            Err(err) => return Err(format!("remove {}: {err}", lnk.display())),
        }
    }
    notify_shell();
    Ok(())
}

//
// Per-user "Installed apps" entry.
//

/// Create/refresh Cascade's one Apps entry from the installed exe.
///
/// Only `APPS_KEY` is touched; another application's `Uninstall` subkey is
/// never enumerated or modified. Values mirror the real
/// install: version comes from this crate's manifest, size from the exe on
/// disk (so a reinstall corrects a stale number), and the removal command
/// points at the same CLI path that already verified.
pub fn write_apps_entry(exe: &Path, dir: &Path) -> Result<(), String> {
    registry::create_key(APPS_KEY).map_err(|rc| format!("Apps entry: create key failed ({rc})"))?;

    let write = |value: &str, data: &str| {
        registry::write_string(APPS_KEY, value, data)
            .map_err(|rc| format!("Apps entry: {value} write failed ({rc})"))
    };
    write("DisplayName", APPS_DISPLAY_NAME)?;
    write("DisplayVersion", env!("CARGO_PKG_VERSION"))?;
    write("Publisher", APPS_PUBLISHER)?;
    write("InstallLocation", &dir.to_string_lossy())?;
    write("DisplayIcon", &display_icon(exe))?;

    let cmd = uninstall_command(exe);
    write("UninstallString", &cmd)?;
    write("QuietUninstallString", &cmd)?;

    let bytes = fs::metadata(exe)
        .map_err(|err| format!("Apps entry: stat {}: {err}", exe.display()))?
        .len();
    for (value, data) in [
        ("EstimatedSize", estimated_size_kb(bytes)),
        ("NoModify", 1),
        ("NoRepair", 1),
    ] {
        registry::write_u32(APPS_KEY, value, data)
            .map_err(|rc| format!("Apps entry: {value} write failed ({rc})"))?;
    }
    Ok(())
}

/// Remove exactly our Apps entry. A missing key is not an error; nothing
/// else under the Uninstall branch is ever touched.
pub fn remove_apps_entry() -> Result<(), String> {
    registry::delete_key(APPS_KEY).map_err(|rc| format!("Apps entry: delete key failed ({rc})"))
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
/// never sleep-and-hope — bounded polling with a deadline.
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

/// Ask a running daemon to exit through its ladder (repaint first,
/// and wait for its control window to disappear — the window
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

/// The double-click prompt. Anything but an explicit
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

//
// Install / uninstall.
//

/// Run the install steps, printing each one.
///
/// Failure at any step leaves a truthful partial state: files land before
/// PATH/autostart, and `uninstall` reverses any subset.
pub fn install_from(source: &Path) -> Result<(), String> {
    let dir = data_dir()?;
    let dest_exe = dir.join(EXE_NAME);

    // Leftovers an earlier deferred delete could not finish (
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

    // Step 2: the PowerShell 5.1 shim, always rewritten. It sits in
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

    // Step 4b: discoverable from Start search and
    // the Desktop, both per-user and both pointing at `cascade.exe ui`.
    install_shortcuts(&dest_exe)?;

    // Step 4c: visible in Settings → Installed apps
    // with a GUI uninstall path. Written after the exe exists so the size
    // and command point at a real file.
    write_apps_entry(&dest_exe, &dir)?;
    println!("Installed apps entry: HKCU\\{APPS_KEY}");

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

/// Reverse the install, printing each step. Deletes only
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

    // 2b: remove exactly our two shell links.
    remove_shortcuts()?;

    // 2c: drop the Apps entry while the installed
    // exe still exists, so Windows never advertises a removal command
    // pointing at a file we have already renamed/deleted.
    remove_apps_entry()?;
    println!("Installed apps entry: removed.");

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
/// files.
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
        // cmd waits for this GUI child;: stem stays 7.
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

    #[test]
    fn shortcut_name_arguments_and_description_are_stable() {
        // The link is a launcher over the same `cascade ui` CLI role;
        // changing any of these changes what users see in Start search.
        assert_eq!(SHORTCUT_NAME, "Cascade.lnk");
        assert_eq!(shortcut_arguments(), "ui");
        assert_eq!(shortcut_description(), "Cascade library");
        let start = Path::new(r"C:\Users\X\AppData\Roaming\Microsoft\Windows\Start Menu\Programs");
        assert_eq!(shortcut_path(start), start.join("Cascade.lnk"));
    }

    #[test]
    fn shortcut_cleanup_matches_only_our_exact_name() {
        assert!(is_our_shortcut(Path::new(r"C:\Desktop\Cascade.lnk")));
        assert!(is_our_shortcut(Path::new(r"C:\Desktop\CASCADE.LNK")));
        // Similar names, backups and other extensions are not ours.
        assert!(!is_our_shortcut(Path::new(r"C:\Desktop\Cascade Notes.lnk")));
        assert!(!is_our_shortcut(Path::new(r"C:\Desktop\Cascade.lnk.bak")));
        assert!(!is_our_shortcut(Path::new(r"C:\Desktop\Cascade.exe")));
        assert!(!is_our_shortcut(Path::new(r"C:\Desktop\Other.lnk")));
        assert!(!is_our_shortcut(Path::new("Cascade")));
    }

    #[test]
    fn path_wide_preserves_non_ascii_components() {
        // Win32 paths are UTF-16: encode the OsStr, never a lossy String.
        let wide = path_wide(Path::new(r"C:\Users\Zahraan Dzakii\Cascade"));
        assert_eq!(wide::from_wide(&wide), r"C:\Users\Zahraan Dzakii\Cascade");
        assert_eq!(wide.last(), Some(&0));
    }

    #[test]
    fn apps_entry_command_is_quoted_and_silent() {
        let exe = Path::new(r"C:\Users\Zahraan Dzakii\AppData\Local\Cascade\cascade.exe");
        assert_eq!(
            uninstall_command(exe),
            r#""C:\Users\Zahraan Dzakii\AppData\Local\Cascade\cascade.exe" uninstall --quiet"#
        );
        assert_eq!(display_icon(exe), format!("{},0", exe.display()));
    }

    #[test]
    fn apps_entry_size_rounds_up_and_saturates() {
        assert_eq!(estimated_size_kb(0), 0);
        assert_eq!(estimated_size_kb(1), 1);
        assert_eq!(estimated_size_kb(1024), 1);
        assert_eq!(estimated_size_kb(1025), 2);
        assert_eq!(estimated_size_kb(u64::MAX), u32::MAX);
    }

    #[test]
    fn apps_key_is_per_user_and_owned_by_us() {
        // Renaming this orphan an existing install on every upgrade and
        // leaves a stale tile in Settings; never change it casually.
        assert_eq!(
            APPS_KEY,
            r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Cascade"
        );
        assert_eq!(APPS_DISPLAY_NAME, "Cascade");
        assert_eq!(APPS_PUBLISHER, "Cascade");
    }
}
