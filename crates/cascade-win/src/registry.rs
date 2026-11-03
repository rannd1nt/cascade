//! Minimal registry helpers.
//!
//! Read/write only what Cascade needs: autostart and a few diagnostics values.
//! Nothing here touches another application's data.

// Win32 `HKEY` is a raw pointer. Every caller passes one of the predefined
// `HKEY_*` constants; the OS validates handles, so a safe wrapper would only
// add noise without adding a safety guarantee.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

use crate::wide;

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const STARTUP_APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
pub const AUTOSTART_VALUE: &str = "Cascade";

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
    let key = open_for_write(subkey)?;
    let val = wide::to_wide(value);
    let data_w = wide::to_wide(data);
    // SAFETY: the buffer is valid UTF-16 with a trailing NUL; length is in bytes.
    let rc = unsafe {
        RegSetValueExW(
            key,
            val.as_ptr(),
            0,
            REG_SZ,
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
