//! Minimal registry helpers.
//!
//! Read/write only what Cascade needs: autostart and a few diagnostics values.
//! Nothing here touches another application's data.

// Win32 `HKEY` is a raw pointer. Every caller passes one of the predefined
// `HKEY_*` constants; the OS validates handles, so a safe wrapper would only
// add noise without adding a safety guarantee.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::path::Path;

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegEnumKeyExW, RegGetValueW,
    RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_DWORD,
    REG_SZ, RRF_NOEXPAND, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

use crate::wide;

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const STARTUP_APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
pub const AUTOSTART_VALUE: &str = "Cascade";

/// The per-user environment key (`HKCU\Environment`), where the user `Path`
/// lives (design-m4 §6.1 step 3: user-level only, never the machine PATH).
pub const ENVIRONMENT_KEY: &str = "Environment";
pub const PATH_VALUE: &str = "Path";

/// Read a `REG_SZ` value. Returns `None` when the key or value is absent.
pub fn read_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
    let sub = wide::to_wide(subkey);
    let val = wide::to_wide(value);

    let mut kind = 0u32;
    let mut size = 0u32;
    // SAFETY: all pointers are valid for the duration of the call; a null data
    // pointer with a non-null size is the documented way to query the size.
    let rc = unsafe {
        RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if rc != ERROR_SUCCESS || size == 0 {
        return None;
    }

    let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
    let mut size2 = (buf.len() * 2) as u32;
    // SAFETY: buffer is large enough for `size2` bytes as reported above.
    let rc = unsafe {
        RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            &mut kind,
            buf.as_mut_ptr().cast(),
            &mut size2,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    Some(wide::from_wide(&buf))
}

/// Read a string value **without expanding** it, returning `(kind, text)`.
///
/// Used for `HKCU\Environment\Path`, which may be `REG_SZ` or
/// `REG_EXPAND_SZ`; both the kind and the raw text (`%VAR%` intact) must
/// survive a read-modify-write so we never rewrite another tool's entries
/// in expanded form (design-m4 §6.1 step 3).
pub fn read_string_raw(root: HKEY, subkey: &str, value: &str) -> Option<(u32, String)> {
    let sub = wide::to_wide(subkey);
    let val = wide::to_wide(value);

    let mut kind = 0u32;
    let mut size = 0u32;
    // SAFETY: size query as in `read_string`; RRF_NOEXPAND keeps `%VAR%`
    // literal instead of resolving it against the current environment.
    let rc = unsafe {
        RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ | RRF_NOEXPAND,
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if rc != ERROR_SUCCESS || size == 0 {
        return None;
    }

    let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
    let mut size2 = (buf.len() * 2) as u32;
    // SAFETY: buffer is large enough for `size2` bytes as reported above.
    let rc = unsafe {
        RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ | RRF_NOEXPAND,
            &mut kind,
            buf.as_mut_ptr().cast(),
            &mut size2,
        )
    };
    (rc == ERROR_SUCCESS).then_some((kind, wide::from_wide(&buf)))
}

/// List the direct subkey names of a key (read-only). A missing or
/// unreadable key yields an empty list instead of an error — used by
/// `device::collect` to walk the display driver class key.
pub fn enum_subkeys(root: HKEY, subkey: &str) -> Vec<String> {
    let sub = wide::to_wide(subkey);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: read-only open; `key` is closed on every return path below.
    let rc = unsafe { RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_READ, &mut key) };
    if rc != ERROR_SUCCESS {
        return Vec::new();
    }

    let mut names = Vec::new();
    let mut index = 0u32;
    loop {
        // Registry key names are capped at 255 chars; +1 for the NUL.
        let mut buf = vec![0u16; 257];
        let mut length = (buf.len() - 1) as u32;
        // SAFETY: `key` is open; the buffer/length pair describes itself.
        // Any non-success (including a too-long name or access denied on
        // one subkey) ends the walk instead of spinning.
        let rc = unsafe {
            RegEnumKeyExW(
                key,
                index,
                buf.as_mut_ptr(),
                &mut length,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        if rc != ERROR_SUCCESS {
            break;
        }
        names.push(wide::from_wide(&buf[..length as usize]));
        index += 1;
    }
    // SAFETY: handle came from `RegOpenKeyExW` above and is no longer used.
    unsafe { RegCloseKey(key) };
    names
}

/// Read a `REG_DWORD` value.
pub fn read_u32(root: HKEY, subkey: &str, value: &str) -> Option<u32> {
    let sub = wide::to_wide(subkey);
    let val = wide::to_wide(value);

    let mut kind = 0u32;
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `data` is a properly aligned u32 and `size` describes it.
    let rc = unsafe {
        RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_DWORD,
            &mut kind,
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (rc == ERROR_SUCCESS).then_some(data)
}

/// Read a `REG_BINARY` value.
pub fn read_bytes(root: HKEY, subkey: &str, value: &str) -> Option<Vec<u8>> {
    let sub = wide::to_wide(subkey);
    let val = wide::to_wide(value);

    let mut kind = 0u32;
    let mut size = 0u32;
    // SAFETY: size query, see `read_string`.
    let rc = unsafe {
        RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_BINARY,
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if rc != ERROR_SUCCESS || size == 0 {
        return None;
    }

    let mut buf = vec![0u8; size as usize];
    let mut size2 = size;
    // SAFETY: buffer matches the reported size.
    let rc = unsafe {
        RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_BINARY,
            &mut kind,
            buf.as_mut_ptr().cast(),
            &mut size2,
        )
    };
    (rc == ERROR_SUCCESS).then_some(buf)
}

fn open_for_write(subkey: &str) -> Result<HKEY, u32> {
    let sub = wide::to_wide(subkey);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `key` is a valid out pointer; the key name is NUL-terminated.
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
    if rc == ERROR_SUCCESS {
        Ok(key)
    } else {
        Err(rc)
    }
}

/// Write a `REG_SZ` value under `HKEY_CURRENT_USER`.
pub fn write_string(subkey: &str, value: &str, data: &str) -> Result<(), u32> {
    write_string_typed(subkey, value, REG_SZ, data)
}

/// Write a string value under `HKEY_CURRENT_USER` with an explicit kind
/// (`REG_SZ` or `REG_EXPAND_SZ`), preserving the kind the value already
/// had (design-m4 §6.1 step 3).
pub fn write_string_typed(subkey: &str, value: &str, kind: u32, data: &str) -> Result<(), u32> {
    let key = open_for_write(subkey)?;
    let val = wide::to_wide(value);
    let data_w = wide::to_wide(data);
    // SAFETY: the buffer is valid UTF-16 with a trailing NUL; length is in bytes.
    let rc = unsafe {
        RegSetValueExW(
            key,
            val.as_ptr(),
            0,
            kind,
            data_w.as_ptr().cast(),
            (data_w.len() * 2) as u32,
        )
    };
    // SAFETY: handle came from `RegOpenKeyExW` and is no longer used.
    unsafe { RegCloseKey(key) };
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(rc)
    }
}

/// Create (or open) a key under `HKEY_CURRENT_USER` for writing.
///
/// `RegOpenKeyExW` cannot create the Installed-apps key on a first install;
/// `RegCreateKeyExW` is idempotent when the key already exists.
pub fn create_key(subkey: &str) -> Result<(), u32> {
    let sub = wide::to_wide(subkey);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `sub` is NUL-terminated; all optional arguments are null and
    // `key` is a valid out pointer. No security attributes are needed for a
    // per-user key. The handle is closed before returning.
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(rc);
    }
    // SAFETY: handle came from `RegCreateKeyExW` and is no longer used.
    unsafe { RegCloseKey(key) };
    Ok(())
}

/// Write a `REG_DWORD` value under `HKEY_CURRENT_USER`.
pub fn write_u32(subkey: &str, value: &str, data: u32) -> Result<(), u32> {
    let key = open_for_write(subkey)?;
    let val = wide::to_wide(value);
    // SAFETY: `data` is a properly aligned u32 and the size is its byte
    // length, as REG_DWORD requires.
    let rc = unsafe {
        RegSetValueExW(
            key,
            val.as_ptr(),
            0,
            REG_DWORD,
            (&data as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    // SAFETY: handle came from `RegOpenKeyExW` and is no longer used.
    unsafe { RegCloseKey(key) };
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(rc)
    }
}

/// Delete a value under `HKEY_CURRENT_USER`. Missing values are not an error.
pub fn delete_value(subkey: &str, value: &str) -> Result<(), u32> {
    let key = open_for_write(subkey)?;
    let val = wide::to_wide(value);
    // SAFETY: handle is valid and the name is NUL-terminated.
    let rc = unsafe { RegDeleteValueW(key, val.as_ptr()) };
    // SAFETY: handle came from `RegOpenKeyExW` and is no longer used.
    unsafe { RegCloseKey(key) };
    if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(rc)
    }
}

/// Delete an empty key under `HKEY_CURRENT_USER`. A missing key is a no-op.
/// Only Cascade's own single key (no subkeys) is ever passed here.
pub fn delete_key(subkey: &str) -> Result<(), u32> {
    let sub = wide::to_wide(subkey);
    // SAFETY: `sub` is NUL-terminated and the fixed HKEY is validated by
    // the OS. `RegDeleteKeyW` fails when the key has subkeys; ours has none.
    let rc = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, sub.as_ptr()) };
    if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND || rc == ERROR_PATH_NOT_FOUND {
        Ok(())
    } else {
        Err(rc)
    }
}

/// Registry-visible state of Cascade's autostart entry.
#[derive(Clone, Debug, Default)]
pub struct AutostartState {
    pub registered: bool,
    pub command: Option<String>,
    pub points_to_current_exe: bool,
    pub windows_disabled: bool,
    /// True when `StartupApproved` exists and we could not interpret it.
    pub approval_unknown: bool,
}

/// The command line we install in the `Run` key.
pub fn autostart_command(exe: &Path) -> String {
    format!("\"{}\" --autostart", exe.display())
}

pub fn autostart_state(exe: &Path) -> AutostartState {
    let command = read_string(HKEY_CURRENT_USER, RUN_KEY, AUTOSTART_VALUE);
    let expected = autostart_command(exe);
    let points_to_current_exe = command
        .as_deref()
        .is_some_and(|c| c.eq_ignore_ascii_case(&expected));

    // `StartupApproved\Run` stores a 12-byte structure per entry. The first
    // byte is 0x02 when enabled and 0x03 when the user disabled it in Settings.
    // Treat odd values as disabled; anything else unexpected is reported.
    let mut windows_disabled = false;
    let mut approval_unknown = false;
    if let Some(bytes) = read_bytes(HKEY_CURRENT_USER, STARTUP_APPROVED_KEY, AUTOSTART_VALUE) {
        match bytes.first().copied() {
            Some(flag) => windows_disabled = flag & 1 == 1,
            None => approval_unknown = true,
        }
    }

    AutostartState {
        registered: command.is_some(),
        command,
        points_to_current_exe,
        windows_disabled,
        approval_unknown,
    }
}

/// Enable autostart for `exe` via the per-user `Run` key.
pub fn autostart_enable(exe: &Path) -> Result<(), u32> {
    write_string(RUN_KEY, AUTOSTART_VALUE, &autostart_command(exe))
}

/// Remove Cascade's `Run` value. The video files are untouched.
pub fn autostart_disable() -> Result<(), u32> {
    delete_value(RUN_KEY, AUTOSTART_VALUE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_command_quotes_path_and_adds_flag() {
        let exe = Path::new(r"C:\Users\Zahraan Dzakii\AppData\Local\Cascade\cascade.exe");
        assert_eq!(
            autostart_command(exe),
            r#""C:\Users\Zahraan Dzakii\AppData\Local\Cascade\cascade.exe" --autostart"#
        );
    }

    #[test]
    fn key_and_value_names_are_stable() {
        // Renaming these breaks detection of an existing install.
        assert_eq!(AUTOSTART_VALUE, "Cascade");
        assert!(RUN_KEY.ends_with(r"\CurrentVersion\Run"));
        assert!(STARTUP_APPROVED_KEY.ends_with(r"\StartupApproved\Run"));
    }
}
