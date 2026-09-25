//! Lightweight Total Commander Content Plugin (WDX).

use std::os::raw::{c_char, c_int, c_void};
use mediares_core::tc_api::*;
use mediares_core::wdx_api::*;

#[no_mangle]
pub unsafe extern "system" fn ContentGetSupportedField(
    field_index: c_int,
    field_name: *mut c_char,
    units: *mut c_char,
    max_len: c_int,
) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        content_get_supported_field(field_index, field_name, units, max_len)
    }))
    .unwrap_or(FT_NOMOREFIELDS)
}

#[no_mangle]
pub unsafe extern "system" fn ContentGetValueW(
    file_name: *const u16,
    field_index: c_int,
    unit_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        content_get_value_w(file_name, field_index, unit_index, field_value, max_len, flags)
    }))
    .unwrap_or(FT_FILEERROR)
}

#[no_mangle]
pub unsafe extern "system" fn ContentGetValue(
    file_name: *const c_char,
    field_index: c_int,
    unit_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        content_get_value_a(file_name, field_index, unit_index, field_value, max_len, flags)
    }))
    .unwrap_or(FT_FILEERROR)
}

#[no_mangle]
pub unsafe extern "system" fn ContentStopGetValueW(_file_name: *const u16) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        content_stop_get_value();
    }));
}

#[no_mangle]
pub unsafe extern "system" fn ContentStopGetValue(_file_name: *const c_char) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        content_stop_get_value();
    }));
}

#[no_mangle]
pub unsafe extern "system" fn ContentGetDetectString(detect_string: *mut c_char, max_len: c_int) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        content_get_detect_string(detect_string, max_len);
    }));
}

#[no_mangle]
pub unsafe extern "system" fn ContentSetDefaultParams(_dps: *mut ContentDefaultParamStruct) {}

#[no_mangle]
pub unsafe extern "system" fn ContentPluginUnloading() {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        content_plugin_unloading();
    }));
}
