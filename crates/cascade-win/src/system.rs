//! OS and session information used by `doctor` and diagnostics.

use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};

use crate::registry;

const CURRENT_VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
const ENVIRONMENT_KEY: &str = r"SYSTEM\CurrentControlSet\Control\ProductOptions";

#[derive(Clone, Debug, Default)]
pub struct OsInfo {
    pub major: u32,
    pub build: u32,
    pub ubr: Option<u32>,
    pub display_version: Option<String>,
    pub product_name: Option<String>,
    pub edition: Option<String>,
}

impl OsInfo {
    pub fn build_string(&self) -> String {
        match self.ubr {
            Some(ubr) if ubr > 0 => format!("{}.{}", self.build, ubr),
            _ => self.build.to_string(),
        }
    }
}

impl std::fmt::Display for OsInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Windows {}", self.major)?;
        if let Some(v) = &self.display_version {
            write!(f, " {v}")?;
        }
        write!(f, " (build {})", self.build_string())?;
        if let Some(p) = &self.product_name {
            // Windows 11 keeps reporting "Windows 10 ..." in this value; only
            // show it when it does not contradict the build number.
            let misleading = self.major >= 11 && p.contains("Windows 10");
            if !misleading {
                write!(f, " [{p}]")?;
            }
        }
        Ok(())
    }
}

pub fn os_info() -> OsInfo {
    let build_text = registry::read_string(HKEY_LOCAL_MACHINE, CURRENT_VERSION_KEY, "CurrentBuild")
        .or_else(|| {
            registry::read_string(
                HKEY_LOCAL_MACHINE,
                CURRENT_VERSION_KEY,
                "CurrentBuildNumber",
            )
        })
        .unwrap_or_default();
    let build = build_text
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    let major = if build >= 22_000 {
        11
    } else if build >= 10_240 {
        10
    } else if build >= 7_600 {
        6
    } else {
        0
    };

    OsInfo {
        major,
        build,
        ubr: registry::read_u32(HKEY_LOCAL_MACHINE, CURRENT_VERSION_KEY, "UBR"),
        display_version: registry::read_string(
            HKEY_LOCAL_MACHINE,
            CURRENT_VERSION_KEY,
            "DisplayVersion",
        ),
        product_name: registry::read_string(HKEY_LOCAL_MACHINE, CURRENT_VERSION_KEY, "ProductName"),
        edition: registry::read_string(HKEY_LOCAL_MACHINE, ENVIRONMENT_KEY, "ProductType"),
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionInfo {
    pub id: u32,
    pub remote: bool,
}

pub fn session_info() -> SessionInfo {
    let mut id = 0u32;
    // SAFETY: `id` is a valid out pointer.
    let _ok = unsafe {
        ProcessIdToSessionId(
            windows_sys::Win32::System::Threading::GetCurrentProcessId(),
            &mut id,
        )
    };
    let remote = unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 };
    SessionInfo { id, remote }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(build: u32, ubr: Option<u32>) -> OsInfo {
        OsInfo {
            major: 11,
            build,
            ubr,
            ..OsInfo::default()
        }
    }

    #[test]
    fn build_string_appends_ubr_when_present() {
        assert_eq!(info(26_200, Some(9_711)).build_string(), "26200.9711");
    }

    #[test]
    fn build_string_is_plain_build_without_ubr() {
        assert_eq!(info(26_200, None).build_string(), "26200");
        // UBR present but zero means "no update component" on some builds.
        assert_eq!(info(26_200, Some(0)).build_string(), "26200");
    }

    #[test]
    fn default_os_info_is_zeroed_not_panicking() {
        assert_eq!(OsInfo::default().build_string(), "0");
    }
}
