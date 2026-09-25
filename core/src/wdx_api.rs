//! Centralized Total Commander WDX implementation shared by `wdx` and `combo` crates.

use std::ffi::OsString;
use std::os::raw::{c_char, c_int, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::cache::{get_cache, CachedMedia};
use crate::probe::probe_file;
use crate::tc_api::*;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn is_stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::Relaxed)
}

#[inline]
pub fn reset_stop_flag() {
    STOP_REQUESTED.store(false, Ordering::Relaxed);
}

const FIELDS: &[(&str, c_int)] = &[
    ("Image_dHash", FT_STRINGW),
    ("Image_pHash", FT_STRINGW),
    ("Image_CoarseHash", FT_STRINGW),
    ("Image_Dimensions", FT_STRINGW),
    ("Image_AspectRatio", FT_STRINGW),
    ("Video_Fingerprint", FT_STRINGW),
    ("Video_dHash_Mid", FT_STRINGW),
    ("Video_Duration_Sec", FT_NUMERIC_32),
    ("Video_Dimensions", FT_STRINGW),
    ("Media_Type", FT_STRINGW),
    ("Plugin_Version", FT_STRINGW),
];

pub const WDX_DETECT_STRING: &str =
    r#"EXT="JPG" | EXT="JPEG" | EXT="PNG" | EXT="GIF" | EXT="WEBP" | EXT="BMP" | EXT="TIFF" | EXT="TIF" | EXT="ICO" | EXT="CR2" | EXT="NEF" | EXT="ARW" | EXT="DNG" | EXT="ORF" | EXT="RW2" | EXT="PSD" | EXT="MP4" | EXT="MKV" | EXT="AVI" | EXT="MOV" | EXT="WMV" | EXT="WEBM" | EXT="M4V" | EXT="FLV" | EXT="TS""#;

pub unsafe fn content_get_supported_field(
    field_index: c_int,
    field_name: *mut c_char,
    units: *mut c_char,
    max_len: c_int,
) -> c_int {
    if field_index < 0 || field_index as usize >= FIELDS.len() {
        return FT_NOMOREFIELDS;
    }

    let (name, field_type) = FIELDS[field_index as usize];

    if !field_name.is_null() && max_len > 0 {
        write_c_string(field_name, max_len as usize, name);
    }

    if !units.is_null() && max_len > 0 {
        *units = 0;
    }

    field_type
}

pub unsafe fn content_get_value_w(
    file_name: *const u16,
    field_index: c_int,
    _unit_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
) -> c_int {
    reset_stop_flag();

    let path = match pwstr_to_path(file_name) {
        Some(p) => p,
        None => return FT_FILEERROR,
    };

    if (flags & CONTENT_DELAYIFSLOW) != 0
        && !get_cache().is_cached(&path)
        && probe_file(&path).is_video_kind()
    {
        return FT_DELAYED;
    }

    get_field_value_internal(&path, field_index, field_value, max_len, true)
}

pub unsafe fn content_get_value_a(
    file_name: *const c_char,
    field_index: c_int,
    _unit_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
) -> c_int {
    reset_stop_flag();

    let path = match pstr_to_path(file_name) {
        Some(p) => p,
        None => return FT_FILEERROR,
    };

    if (flags & CONTENT_DELAYIFSLOW) != 0
        && !get_cache().is_cached(&path)
        && probe_file(&path).is_video_kind()
    {
        return FT_DELAYED;
    }

    get_field_value_internal(&path, field_index, field_value, max_len, false)
}

pub unsafe fn content_stop_get_value() {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
}

pub unsafe fn content_get_detect_string(detect_string: *mut c_char, max_len: c_int) {
    if !detect_string.is_null() && max_len > 0 {
        write_c_string(detect_string, max_len as usize, WDX_DETECT_STRING);
    }
}

pub unsafe fn content_plugin_unloading() {
    get_cache().clear();
}

unsafe fn get_field_value_internal(
    path: &Path,
    field_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    is_unicode: bool,
) -> c_int {
    if field_index < 0 || field_index as usize >= FIELDS.len() || field_value.is_null() {
        return FT_FIELDEMPTY;
    }

    if field_index == 10 {
        return write_string_val(field_value, max_len, env!("CARGO_PKG_VERSION"), is_unicode);
    }

    let cached = get_cache().get_or_analyze(path);

    match (field_index, cached) {
        (0, CachedMedia::Image(img)) => {
            write_string_val(field_value, max_len, &img.dhash_hex(), is_unicode)
        }
        (1, CachedMedia::Image(img)) => {
            write_string_val(field_value, max_len, &img.phash_hex(), is_unicode)
        }
        (2, CachedMedia::Image(img)) => {
            write_string_val(field_value, max_len, &img.coarse_hash_hex(), is_unicode)
        }
        (3, CachedMedia::Image(img)) => {
            write_string_val(field_value, max_len, &img.dimensions_str(), is_unicode)
        }
        (4, CachedMedia::Image(img)) => {
            write_string_val(field_value, max_len, &img.aspect_ratio, is_unicode)
        }

        (5, CachedMedia::Video(vid)) => {
            write_string_val(field_value, max_len, &vid.fingerprint, is_unicode)
        }
        (6, CachedMedia::Video(vid)) => {
            write_string_val(field_value, max_len, &vid.dhash_mid_hex(), is_unicode)
        }
        (7, CachedMedia::Video(vid)) => {
            if max_len >= 4 {
                *(field_value as *mut i32) = vid.duration_sec as i32;
                FT_NUMERIC_32
            } else {
                FT_FIELDEMPTY
            }
        }
        (8, CachedMedia::Video(vid)) => {
            write_string_val(field_value, max_len, &vid.dimensions_str(), is_unicode)
        }

        (9, CachedMedia::Image(_)) => write_string_val(field_value, max_len, "Image", is_unicode),
        (9, CachedMedia::Video(_)) => write_string_val(field_value, max_len, "Video", is_unicode),

        _ => FT_FIELDEMPTY,
    }
}

unsafe fn write_string_val(
    dest: *mut c_void,
    max_bytes: c_int,
    text: &str,
    is_unicode: bool,
) -> c_int {
    if is_unicode {
        write_string_w(dest, max_bytes, text)
    } else {
        write_string_a(dest, max_bytes, text)
    }
}

unsafe fn write_string_w(dest: *mut c_void, max_bytes: c_int, text: &str) -> c_int {
    if dest.is_null() || max_bytes < 2 {
        return FT_FIELDEMPTY;
    }
    let dest_u16 = dest as *mut u16;
    let max_chars = (max_bytes as usize) / 2;
    let mut i = 0;
    for ch in text.encode_utf16() {
        if i + 1 >= max_chars {
            break;
        }
        *dest_u16.add(i) = ch;
        i += 1;
    }
    *dest_u16.add(i) = 0;
    FT_STRINGW
}

unsafe fn write_string_a(dest: *mut c_void, max_bytes: c_int, text: &str) -> c_int {
    if dest.is_null() || max_bytes < 1 {
        return FT_FIELDEMPTY;
    }
    let dest_u8 = dest as *mut u8;
    let max_chars = max_bytes as usize;
    let mut i = 0;
    for byte in text.bytes() {
        if i + 1 >= max_chars {
            break;
        }
        *dest_u8.add(i) = byte;
        i += 1;
    }
    *dest_u8.add(i) = 0;
    FT_STRING
}

unsafe fn write_c_string(dest: *mut c_char, max_bytes: usize, text: &str) {
    let dest_u8 = dest as *mut u8;
    let mut i = 0;
    for byte in text.bytes() {
        if i + 1 >= max_bytes {
            break;
        }
        *dest_u8.add(i) = byte;
        i += 1;
    }
    *dest_u8.add(i) = 0;
}

pub unsafe fn pwstr_to_path(ptr: *const u16) -> Option<PathBuf> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    let os_str = OsString::from_wide(slice);
    Some(PathBuf::from(os_str))
}

pub unsafe fn pstr_to_path(ptr: *const c_char) -> Option<PathBuf> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    if len == 0 {
        return Some(PathBuf::new());
    }

    use windows::Win32::Globalization::{
        MultiByteToWideChar, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS,
    };

    let wide_len = MultiByteToWideChar(
        CP_ACP,
        MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
        std::slice::from_raw_parts(ptr as *const u8, len),
        None,
    );

    if wide_len <= 0 {
        return None;
    }

    let mut wide_buf = vec![0u16; wide_len as usize];
    let written = MultiByteToWideChar(
        CP_ACP,
        MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
        std::slice::from_raw_parts(ptr as *const u8, len),
        Some(&mut wide_buf),
    );

    if written <= 0 {
        return None;
    }

    let os_str = OsString::from_wide(&wide_buf);
    Some(PathBuf::from(os_str))
}
