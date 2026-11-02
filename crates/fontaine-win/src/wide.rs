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
