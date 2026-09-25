//! Combo Total Commander Plugin (WDX + WLX in a single binary).

pub mod image_view;
pub mod wlx_state;
pub mod wlx_window;

use std::os::raw::{c_char, c_int, c_void};
use windows::Win32::Foundation::HWND;
use mediares_core::tc_api::*;
use mediares_core::wdx_api::*;
use wlx_window::{create_viewer_window, destroy_viewer_window, get_viewer_state};

// ==========================================
// Total Commander WDX (Content Plugin) API
// ==========================================

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

// ==========================================
// Total Commander WLX (Lister Plugin) API
// ==========================================

const WLX_DETECT_STRING: &str = concat!(
    "MULTIMEDIA & ext=\"JPG\"|ext=\"JPEG\"|ext=\"PNG\"|ext=\"GIF\"|ext=\"WEBP\"|",
    "ext=\"BMP\"|ext=\"TIFF\"|ext=\"TIF\"|ext=\"ICO\"|ext=\"CR2\"|ext=\"NEF\"|",
    "ext=\"ARW\"|ext=\"DNG\"|ext=\"ORF\"|ext=\"RW2\"|ext=\"PSD\"|ext=\"CR3\"|",
    "ext=\"RAF\"|ext=\"PEF\"|ext=\"MP4\"|ext=\"MKV\"|ext=\"AVI\"|ext=\"MOV\"|",
    "ext=\"WMV\"|ext=\"WEBM\"|ext=\"M4V\"|ext=\"FLV\"|ext=\"TS\"|ext=\"MP3\"|",
    "ext=\"FLAC\"|ext=\"WAV\"|ext=\"OGG\""
);

#[no_mangle]
pub unsafe extern "system" fn ListGetDetectString(detect_string: *mut c_char, max_len: c_int) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !detect_string.is_null() && max_len > 0 {
            let bytes = WLX_DETECT_STRING.as_bytes();
            let len = bytes.len().min(max_len as usize - 1);
            let slice = std::slice::from_raw_parts_mut(detect_string as *mut u8, max_len as usize);
            slice[..len].copy_from_slice(&bytes[..len]);
            slice[len] = 0;
        }
    }));
}

#[no_mangle]
pub unsafe extern "system" fn ListLoadW(
    parent_win: HWND,
    file_to_load: *const u16,
    _show_flags: c_int,
) -> HWND {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path = match pwstr_to_path(file_to_load) {
            Some(p) => p,
            None => return HWND::default(),
        };
        create_viewer_window(parent_win, &path).unwrap_or_default()
    }))
    .unwrap_or_default()
}

#[no_mangle]
pub unsafe extern "system" fn ListLoad(
    parent_win: HWND,
    file_to_load: *const c_char,
    _show_flags: c_int,
) -> HWND {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path = match pstr_to_path(file_to_load) {
            Some(p) => p,
            None => return HWND::default(),
        };
        create_viewer_window(parent_win, &path).unwrap_or_default()
    }))
    .unwrap_or_default()
}

#[no_mangle]
pub unsafe extern "system" fn ListLoadNextW(
    _parent_win: HWND,
    list_win: HWND,
    file_to_load: *const u16,
    _show_flags: c_int,
) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path = match pwstr_to_path(file_to_load) {
            Some(p) => p,
            None => return LISTPLUGIN_ERROR,
        };

        if let Some(state) = get_viewer_state(list_win) {
            state.file_path = path;
            state.media_type = mediares_core::probe::probe_file(&state.file_path);
            if !state.media_type.is_image_kind() {
                return LISTPLUGIN_ERROR;
            }
            state.load_media();
            if state.image.is_some() {
                state.zoom_factor = 1.0;
                state.pan_x = 0;
                state.pan_y = 0;
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(list_win), None, true);
                LISTPLUGIN_OK
            } else {
                LISTPLUGIN_ERROR
            }
        } else {
            LISTPLUGIN_ERROR
        }
    }))
    .unwrap_or(LISTPLUGIN_ERROR)
}

#[no_mangle]
pub unsafe extern "system" fn ListLoadNext(
    _parent_win: HWND,
    list_win: HWND,
    file_to_load: *const c_char,
    _show_flags: c_int,
) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path = match pstr_to_path(file_to_load) {
            Some(p) => p,
            None => return LISTPLUGIN_ERROR,
        };

        if let Some(state) = get_viewer_state(list_win) {
            state.file_path = path;
            state.media_type = mediares_core::probe::probe_file(&state.file_path);
            if !state.media_type.is_image_kind() {
                return LISTPLUGIN_ERROR;
            }
            state.load_media();
            if state.image.is_some() {
                state.zoom_factor = 1.0;
                state.pan_x = 0;
                state.pan_y = 0;
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(list_win), None, true);
                LISTPLUGIN_OK
            } else {
                LISTPLUGIN_ERROR
            }
        } else {
            LISTPLUGIN_ERROR
        }
    }))
    .unwrap_or(LISTPLUGIN_ERROR)
}

#[no_mangle]
pub unsafe extern "system" fn ListCloseWindow(list_win: HWND) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        destroy_viewer_window(list_win);
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(list_win);
    }));
}

#[no_mangle]
pub unsafe extern "system" fn ListNotificationReceived(
    _list_win: HWND,
    _message: c_int,
    _w_param: usize,
    _l_param: isize,
) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "system" fn ListSendCommand(
    _list_win: HWND,
    _command: c_int,
    _parameter: c_int,
) -> c_int {
    0
}
