//! Total Commander Content Plugin (WDX) implementation shared by the `wdx` and `combo` crates.
//!
//! Both crates expose it through [`export_content_plugin!`], so the FFI surface is defined once.

use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::cache::{get_cache, CachedMedia};
use crate::ffi::{pstr_to_path, pwstr_to_path, write_ansi, write_wide};
use crate::probe::{detect_extensions, probe_file, MediaType};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    ImageDHash,
    ImagePHash,
    ImageCoarseHash,
    ImageDimensions,
    ImageAspectRatio,
    VideoFingerprint,
    VideoDHashMid,
    VideoDurationSec,
    VideoDimensions,
    MediaTypeName,
    PluginVersion,
}

/// Field order is the public WDX index TC stores in user configurations — append only.
const FIELDS: &[(Field, &str, c_int)] = &[
    (Field::ImageDHash, "Image_dHash", FT_STRINGW),
    (Field::ImagePHash, "Image_pHash", FT_STRINGW),
    (Field::ImageCoarseHash, "Image_CoarseHash", FT_STRINGW),
    (Field::ImageDimensions, "Image_Dimensions", FT_STRINGW),
    (Field::ImageAspectRatio, "Image_AspectRatio", FT_STRINGW),
    (Field::VideoFingerprint, "Video_Fingerprint", FT_STRINGW),
    (Field::VideoDHashMid, "Video_dHash_Mid", FT_STRINGW),
    (Field::VideoDurationSec, "Video_Duration_Sec", FT_NUMERIC_32),
    (Field::VideoDimensions, "Video_Dimensions", FT_STRINGW),
    (Field::MediaTypeName, "Media_Type", FT_STRINGW),
    (Field::PluginVersion, "Plugin_Version", FT_STRINGW),
];

fn field_at(index: c_int) -> Option<&'static (Field, &'static str, c_int)> {
    FIELDS.get(usize::try_from(index).ok()?)
}

/// Kinds the WDX analyzes (everything except audio).
pub fn wdx_detect_string() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| {
        detect_extensions(&[MediaType::StandardImage, MediaType::RawImage, MediaType::PsdImage, MediaType::Video])
    })
}

/// # Safety
/// `field_name` and `units` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_supported_field(
    field_index: c_int,
    field_name: *mut c_char,
    units: *mut c_char,
    max_len: c_int,
) -> c_int {
    let Some(&(_, name, field_type)) = field_at(field_index) else {
        return FT_NOMOREFIELDS;
    };
    let max_len = max_len.max(0) as usize;
    write_ansi(field_name, max_len, name);
    write_ansi(units, max_len, "");
    field_type
}

/// # Safety
/// `file_name` must be null or NUL-terminated; `field_value` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_value_w(
    file_name: *const u16,
    field_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
) -> c_int {
    get_value(pwstr_to_path(file_name), field_index, field_value, max_len, flags, true)
}

/// # Safety
/// `file_name` must be null or NUL-terminated; `field_value` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_value_a(
    file_name: *const c_char,
    field_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
) -> c_int {
    get_value(pstr_to_path(file_name), field_index, field_value, max_len, flags, false)
}

pub fn content_stop_get_value() {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
}

/// # Safety
/// `detect_string` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_detect_string(detect_string: *mut c_char, max_len: c_int) {
    write_ansi(detect_string, max_len.max(0) as usize, wdx_detect_string());
}

pub fn content_plugin_unloading() {
    get_cache().clear();
    crate::mf_init::shutdown_mf();
}

unsafe fn get_value(
    path: Option<PathBuf>,
    field_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
    unicode: bool,
) -> c_int {
    reset_stop_flag();
    let Some(path) = path else { return FT_FILEERROR };
    let Some(&(field, _, _)) = field_at(field_index) else { return FT_FIELDEMPTY };
    if field_value.is_null() {
        return FT_FIELDEMPTY;
    }

    if (flags & CONTENT_DELAYIFSLOW) != 0 && probe_file(&path).is_slow_kind() && !get_cache().is_cached(&path) {
        return FT_DELAYED;
    }

    let out = Output { dest: field_value, max_bytes: max_len.max(0) as usize, unicode };
    match compute(&path, field) {
        Some(Value::Text(text)) => out.text(&text),
        Some(Value::Int(n)) => out.int(n),
        None => FT_FIELDEMPTY,
    }
}

enum Value {
    Text(String),
    Int(i32),
}

fn compute(path: &Path, field: Field) -> Option<Value> {
    use Field::*;
    if field == PluginVersion {
        return Some(Value::Text(env!("CARGO_PKG_VERSION").to_string()));
    }
    let text = |s: String| Some(Value::Text(s));
    match (field, get_cache().get_or_analyze(path, &is_stop_requested)) {
        (ImageDHash, CachedMedia::Image(img)) => text(img.dhash_hex()),
        (ImagePHash, CachedMedia::Image(img)) => text(img.phash_hex()),
        (ImageCoarseHash, CachedMedia::Image(img)) => text(img.coarse_hash_hex()),
        (ImageDimensions, CachedMedia::Image(img)) => text(img.dimensions_str()),
        (ImageAspectRatio, CachedMedia::Image(img)) => text(img.aspect_ratio.clone()),
        (VideoFingerprint, CachedMedia::Video(vid)) => text(vid.fingerprint.clone()),
        (VideoDHashMid, CachedMedia::Video(vid)) => text(vid.dhash_mid_hex()),
        (VideoDurationSec, CachedMedia::Video(vid)) => Some(Value::Int(vid.duration_sec.min(i32::MAX as u32) as i32)),
        (VideoDimensions, CachedMedia::Video(vid)) => text(vid.dimensions_str()),
        (MediaTypeName, CachedMedia::Image(_)) => text("Image".into()),
        (MediaTypeName, CachedMedia::Video(_)) => text("Video".into()),
        _ => None,
    }
}

struct Output {
    dest: *mut c_void,
    max_bytes: usize,
    unicode: bool,
}

impl Output {
    unsafe fn text(&self, text: &str) -> c_int {
        if self.unicode {
            if write_wide(self.dest as *mut u16, self.max_bytes, text) { FT_STRINGW } else { FT_FIELDEMPTY }
        } else if write_ansi(self.dest as *mut c_char, self.max_bytes, text) {
            FT_STRING
        } else {
            FT_FIELDEMPTY
        }
    }

    unsafe fn int(&self, n: i32) -> c_int {
        if self.max_bytes < 4 {
            return FT_FIELDEMPTY;
        }
        (self.dest as *mut i32).write_unaligned(n);
        FT_NUMERIC_32
    }
}

/// Defines the `#[no_mangle]` WDX exports in the calling cdylib crate, each wrapped in a panic guard.
#[macro_export]
macro_rules! export_content_plugin {
    () => {
        #[no_mangle]
        pub unsafe extern "system" fn ContentGetSupportedField(
            field_index: ::std::os::raw::c_int,
            field_name: *mut ::std::os::raw::c_char,
            units: *mut ::std::os::raw::c_char,
            max_len: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            $crate::ffi::guard($crate::tc_api::FT_NOMOREFIELDS, || unsafe {
                $crate::wdx_api::content_get_supported_field(field_index, field_name, units, max_len)
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentGetValueW(
            file_name: *const u16,
            field_index: ::std::os::raw::c_int,
            _unit_index: ::std::os::raw::c_int,
            field_value: *mut ::std::os::raw::c_void,
            max_len: ::std::os::raw::c_int,
            flags: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            $crate::ffi::guard($crate::tc_api::FT_FILEERROR, || unsafe {
                $crate::wdx_api::content_get_value_w(file_name, field_index, field_value, max_len, flags)
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentGetValue(
            file_name: *const ::std::os::raw::c_char,
            field_index: ::std::os::raw::c_int,
            _unit_index: ::std::os::raw::c_int,
            field_value: *mut ::std::os::raw::c_void,
            max_len: ::std::os::raw::c_int,
            flags: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            $crate::ffi::guard($crate::tc_api::FT_FILEERROR, || unsafe {
                $crate::wdx_api::content_get_value_a(file_name, field_index, field_value, max_len, flags)
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentStopGetValueW(_file_name: *const u16) {
            $crate::ffi::guard((), $crate::wdx_api::content_stop_get_value)
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentStopGetValue(_file_name: *const ::std::os::raw::c_char) {
            $crate::ffi::guard((), $crate::wdx_api::content_stop_get_value)
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentGetDetectString(
            detect_string: *mut ::std::os::raw::c_char,
            max_len: ::std::os::raw::c_int,
        ) {
            $crate::ffi::guard((), || unsafe {
                $crate::wdx_api::content_get_detect_string(detect_string, max_len)
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentSetDefaultParams(_dps: *mut $crate::tc_api::ContentDefaultParamStruct) {}

        #[no_mangle]
        pub unsafe extern "system" fn ContentPluginUnloading() {
            $crate::ffi::guard((), $crate::wdx_api::content_plugin_unloading)
        }
    };
}
