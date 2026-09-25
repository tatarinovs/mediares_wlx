//! Helpers for the C ABI boundary with Total Commander: panic guard and string marshalling.

use std::ffi::OsString;
use std::os::raw::c_char;
use std::os::windows::ffi::OsStringExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use windows::Win32::Globalization::{MultiByteToWideChar, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};

/// Runs `f`, turning a panic into `fallback`. A panic must never unwind into Total Commander.
#[inline]
pub fn guard<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

/// # Safety
/// `ptr` must be null or point to a NUL-terminated UTF-16 string.
pub unsafe fn pwstr_to_path(ptr: *const u16) -> Option<PathBuf> {
    if ptr.is_null() {
        return None;
    }
    let len = (0..).take_while(|&i| *ptr.add(i) != 0).count();
    if len == 0 {
        return None;
    }
    let wide = std::slice::from_raw_parts(ptr, len);
    Some(PathBuf::from(OsString::from_wide(wide)))
}

/// # Safety
/// `ptr` must be null or point to a NUL-terminated string in the ANSI code page.
pub unsafe fn pstr_to_path(ptr: *const c_char) -> Option<PathBuf> {
    if ptr.is_null() {
        return None;
    }
    let len = (0..).take_while(|&i| *ptr.add(i) != 0).count();
    if len == 0 {
        return None;
    }
    let ansi = std::slice::from_raw_parts(ptr as *const u8, len);
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    let wide_len = MultiByteToWideChar(CP_ACP, flags, ansi, None);
    if wide_len <= 0 {
        return None;
    }
    let mut wide = vec![0u16; wide_len as usize];
    let written = MultiByteToWideChar(CP_ACP, flags, ansi, Some(&mut wide));
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    Some(PathBuf::from(OsString::from_wide(&wide)))
}

/// Writes `text` as a NUL-terminated byte string, truncating to `max_bytes` (including the NUL).
///
/// # Safety
/// `dest` must be null or valid for writes of `max_bytes` bytes.
pub unsafe fn write_ansi(dest: *mut c_char, max_bytes: usize, text: &str) -> bool {
    if dest.is_null() || max_bytes == 0 {
        return false;
    }
    let bytes = text.as_bytes();
    let len = bytes.len().min(max_bytes - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), dest as *mut u8, len);
    *dest.add(len) = 0;
    true
}

/// Writes `text` as a NUL-terminated UTF-16 string into a buffer of `max_bytes` bytes.
///
/// # Safety
/// `dest` must be null or valid for writes of `max_bytes` bytes.
pub unsafe fn write_wide(dest: *mut u16, max_bytes: usize, text: &str) -> bool {
    let max_chars = max_bytes / 2;
    if dest.is_null() || max_chars == 0 {
        return false;
    }
    let mut i = 0;
    for ch in text.encode_utf16().take(max_chars - 1) {
        *dest.add(i) = ch;
        i += 1;
    }
    *dest.add(i) = 0;
    true
}
