//! Small helpers for the UTF-16 strings Win32 expects.

/// Encode a Rust string as a NUL-terminated UTF-16 buffer.
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decode a UTF-16 buffer, stopping at the first NUL.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Read a NUL-terminated UTF-16 string from a pointer.
///
/// # Safety
/// `ptr` must be null or point to a valid NUL-terminated UTF-16 string.
pub unsafe fn from_wide_ptr(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    // SAFETY: caller guarantees a NUL-terminated string.
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
        if len > 32_767 {
            break;
        }
    }
    // SAFETY: we just measured `len` readable elements.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_wide_appends_nul() {
        assert_eq!(to_wide("abc"), vec![0x61, 0x62, 0x63, 0]);
        assert_eq!(to_wide(""), vec![0]);
    }

    #[test]
    fn from_wide_stops_at_first_nul() {
        let buf = [0x68u16, 0x69, 0, 0x78];
        assert_eq!(from_wide(&buf), "hi");
        assert_eq!(from_wide(&[]), "");
    }

    #[test]
    fn roundtrip_unicode_and_emoji() {
        for original in ["Cascade", "wallpaper ✓", "日本語", "🎬🎥"] {
            assert_eq!(from_wide(&to_wide(original)), original);
        }
    }

    #[test]
    fn from_wide_ptr_null_is_empty() {
        // SAFETY: null is explicitly allowed by the function contract.
        let text = unsafe { from_wide_ptr(std::ptr::null()) };
        assert_eq!(text, "");
    }

    #[test]
    fn from_wide_ptr_reads_nul_terminated() {
        let buf = to_wide("workerw");
        // SAFETY: `buf` is a live NUL-terminated UTF-16 buffer.
        let text = unsafe { from_wide_ptr(buf.as_ptr()) };
        assert_eq!(text, "workerw");
    }

    #[test]
    fn from_wide_ptr_caps_runaway_strings() {
        // The safety cap must stop at 32768 units even without a NUL.
        let long = "a".repeat(40_000);
        let buf = to_wide(&long);
        // SAFETY: `buf` is a live buffer of 40001 units with a trailing NUL.
        let text = unsafe { from_wide_ptr(buf.as_ptr()) };
        assert_eq!(text.len(), 32_768);
    }
}
