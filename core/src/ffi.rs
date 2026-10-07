//! Helpers for the C ABI boundary with Total Commander: panic guard and string marshalling.

use std::ffi::OsString;
use std::os::raw::c_char;
use std::os::windows::ffi::OsStringExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use windows::core::PCSTR;
use windows::Win32::Globalization::{
    MultiByteToWideChar, WideCharToMultiByte, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS,
};

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
    ansi_to_os_string(std::slice::from_raw_parts(ptr as *const u8, len)).map(PathBuf::from)
}

/// Converts text in the system ANSI code page (legacy INI/M3U files, `char*` APIs).
pub fn ansi_to_os_string(ansi: &[u8]) -> Option<OsString> {
    if ansi.is_empty() {
        return Some(OsString::new());
    }
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    let wide_len = unsafe { MultiByteToWideChar(CP_ACP, flags, ansi, None) };
    if wide_len <= 0 {
        return None;
    }
    let mut wide = vec![0u16; wide_len as usize];
    let written = unsafe { MultiByteToWideChar(CP_ACP, flags, ansi, Some(&mut wide)) };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    Some(OsString::from_wide(&wide))
}

/// Converts `wide` to the system ANSI code page; unmappable characters become `?`.
fn wide_to_ansi(wide: &[u16]) -> Vec<u8> {
    if wide.is_empty() {
        return Vec::new();
    }
    let len = unsafe { WideCharToMultiByte(CP_ACP, 0, wide, None, PCSTR::null(), None) };
    if len <= 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; len as usize];
    let written =
        unsafe { WideCharToMultiByte(CP_ACP, 0, wide, Some(&mut out), PCSTR::null(), None) };
    out.truncate(written.max(0) as usize);
    out
}

/// Writes `text` in the system ANSI code page as a NUL-terminated string, truncating to
/// `max_bytes` (including the NUL) on a character boundary.
///
/// # Safety
/// `dest` must be null or valid for writes of `max_bytes` bytes.
pub unsafe fn write_ansi(dest: *mut c_char, max_bytes: usize, text: &str) -> bool {
    if dest.is_null() || max_bytes == 0 {
        return false;
    }
    let limit = max_bytes - 1;
    let bytes = if text.is_ascii() {
        text.as_bytes()[..text.len().min(limit)].to_vec()
    } else {
        // Every UTF-16 unit yields at least one byte, so the first `limit` units are an upper
        // bound; drop whole characters until the encoding fits (DBCS code pages vary in width).
        let mut chars: Vec<char> = Vec::new();
        let mut units = 0;
        for ch in text.chars() {
            units += ch.len_utf16();
            if units > limit {
                break;
            }
            chars.push(ch);
        }
        loop {
            let wide: Vec<u16> = chars.iter().collect::<String>().encode_utf16().collect();
            let bytes = wide_to_ansi(&wide);
            if bytes.len() <= limit {
                break bytes;
            }
            chars.pop();
        }
    };
    let len = bytes.len();
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
    let mut units = text.encode_utf16();
    let mut i = 0;
    for ch in units.by_ref().take(max_chars - 1) {
        *dest.add(i) = ch;
        i += 1;
    }
    // Cut short right after the first half of a surrogate pair: drop that half too.
    if i > 0 && (0xD800..0xDC00).contains(&*dest.add(i - 1)) && units.next().is_some() {
        i -= 1;
    }
    *dest.add(i) = 0;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str, max_chars: usize) -> Vec<u16> {
        let mut buf = vec![0xFFFFu16; max_chars];
        assert!(unsafe { write_wide(buf.as_mut_ptr(), max_chars * 2, text) });
        let len = buf.iter().position(|&c| c == 0).expect("terminated");
        buf.truncate(len);
        buf
    }

    #[test]
    fn wide_text_is_cut_on_character_boundaries() {
        let units = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        assert_eq!(wide("abc", 8), units("abc"));
        assert_eq!(wide("abc", 3), units("ab"));
        // The emoji is a surrogate pair, which doesn't fit after "a" into 2 units.
        assert_eq!(wide("a\u{1F600}", 3), units("a"));
        assert_eq!(wide("a\u{1F600}", 4), units("a\u{1F600}"));
    }
}
