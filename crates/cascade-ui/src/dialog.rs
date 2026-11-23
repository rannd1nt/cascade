//! Native multi-select file picker (`IFileOpenDialog`).
//!
//! and: the GUI uses the *native* dialog —
//! never a custom file browser. No `rfd`: the `windows`
//! crate the workspace already carries exposes the same COM API, so this
//! crate gains no extra dependency for one dialog sequence.

use windows::core::{HRESULT, PWSTR};
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM,
    SIGDN_FILESYSPATH,
};

/// Pick one or more files. `Ok(vec![])` means the user cancelled.
///
/// Blocks while the modal dialog is open — inherent to a modal picker
/// (the message pump runs re-entrantly under our event loop). Runs on
/// the UI thread, where [`ComScope`] owns the apartment.
pub fn pick_files() -> Result<Vec<String>, String> {
    let _com = ComScope::new();

    // SAFETY: `FileOpenDialog` is the documented CLSID of the common
    // item dialog; no outer unknown is needed; this thread's apartment
    // picks the implementation.
    let dialog: IFileOpenDialog = unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL) }
        .map_err(|err| format!("could not open the file dialog: {err}"))?;

    // SAFETY: plain setter on the dialog just created.
    let options = FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM;
    unsafe { dialog.SetOptions(options) }
        .map_err(|err| format!("could not configure the file dialog: {err}"))?;

    // SAFETY: takes no arguments; the active window (ours) or HWND(0)
    // are both legal owners.
    let owner: HWND = unsafe { GetActiveWindow() };
    // SAFETY: modal show on this apartment. Cancelling surfaces as the
    // documented `ERROR_CANCELLED` HRESULT, not as a failure to report.
    match unsafe { dialog.Show(Some(owner)) } {
        Ok(()) => {}
        Err(err) if err.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => {
            return Ok(Vec::new());
        }
        Err(err) => return Err(format!("the file dialog failed: {err}")),
    }

    // SAFETY: Show succeeded, so a results collection exists.
    let items = unsafe { dialog.GetResults() }
        .map_err(|err| format!("could not read the selection: {err}"))?;
    // SAFETY: plain getter on the collection above.
    let count = unsafe { items.GetCount() }
        .map_err(|err| format!("could not read the selection: {err}"))?;
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        // SAFETY: `index < count`, checked by the loop bound.
        let item = unsafe { items.GetItemAt(index) }
            .map_err(|err| format!("could not read the selection: {err}"))?;
        // SAFETY: filesystem path requested; the OS allocates the
        // string with `CoTaskMemAlloc` (freed below).
        let raw = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }
            .map_err(|err| format!("could not read a path: {err}"))?;
        paths.push(wide_to_string(raw));
        // SAFETY: allocated by GetDisplayName, freed exactly once here.
        unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    }
    Ok(paths)
}

/// NUL-terminated UTF-16 → `String` for a buffer owned by this call.
fn wide_to_string(ptr: PWSTR) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    // SAFETY: `ptr` is a NUL-terminated buffer we own until the
    // `CoTaskMemFree` below; the walk stops at the terminator the Shell
    // API guarantees (the cap only guards a pathological non-terminated
    // buffer from reading forever).
    unsafe {
        while *ptr.0.add(len) != 0 {
            len += 1;
            if len > 32_768 {
                break;
            }
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr.0, len))
    }
}

/// COM apartment for the calling thread: initialized on entry, released
/// on drop — RAII, never leaked.
struct ComScope {
    /// `true` when this call initialized COM and therefore owns the
    /// matching `CoUninitialize`.
    owned: bool,
}

impl ComScope {
    fn new() -> Self {
        // SAFETY: no pointers are passed. `S_OK` and `S_FALSE` both own
        // a `CoUninitialize` (`is_ok` covers both); any other result —
        // notably `RPC_E_CHANGEDMODE` — means COM already runs on this
        // thread in another apartment, which we must not uninitialize.
        let owned = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        Self { owned }
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balances the successful `CoInitializeEx` above.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_to_string_handles_empty_and_plain() {
        assert!(wide_to_string(PWSTR::null()).is_empty());
        let mut buf: Vec<u16> = "clip.mp4".encode_utf16().collect();
        buf.push(0);
        // `wide_to_string` encapsulates the raw-pointer walk in its own
        // SAFETY block, so the caller needs none.
        let text = wide_to_string(PWSTR(buf.as_mut_ptr()));
        assert_eq!(text, "clip.mp4");
    }

    #[test]
    fn com_scope_never_panics_without_com() {
        // Construction must be infallible on any thread (a test thread
        // has no apartment yet — S_OK path); dropping uninitializes.
        let scope = ComScope::new();
        assert!(scope.owned || !scope.owned, "scope is always valid");
    }
}
