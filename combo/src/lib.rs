//! Combo Total Commander plugin: WDX content fields + WLX Lister viewer in one DLL.

// Exported `unsafe extern` functions follow the TC plugin API contract (valid pointers/handles from TC).
#![allow(clippy::missing_safety_doc)]

mod audio_view;
mod config;
mod dialog;
mod exif_dialog;
mod file_actions;
mod fullscreen;
mod gdi;
mod image_cache;
mod image_view;
mod media_view;
mod osd_template;
mod osd_template_dialog;
mod overlay;
mod playback_audio;
mod playback_video;
mod playlist;
mod print;
mod resume;
mod settings_dialog;
mod snapshot;
mod state;
mod tc_register;
mod transport_bar;
mod video_view;
mod window;

use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::OnceLock;

use mediares_core::ffi::{guard, pstr_to_path, pwstr_to_path, write_ansi};
use mediares_core::probe::{detect_extensions, MediaType};
use mediares_core::tc_api::*;
use windows::core::BOOL;
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, RECT};
use windows::Win32::Graphics::Gdi::HBITMAP;

const DLL_PROCESS_ATTACH: u32 = 1;

static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Handle of this DLL (window classes are registered against it, not against the host exe).
pub(crate) fn module() -> HINSTANCE {
    HINSTANCE(MODULE.load(Ordering::Relaxed))
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(hinst: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(hinst.0, Ordering::Relaxed);
    }
    BOOL(1)
}

// ==========================================
// Total Commander WDX (Content Plugin) API
// ==========================================

mediares_core::export_content_plugin!();

// ==========================================
// Total Commander WLX (Lister Plugin) API
// ==========================================

/// Photos, video, audio and M3U playlists.
fn wlx_detect_string() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| {
        let exts = detect_extensions(&[
            MediaType::StandardImage,
            MediaType::RawImage,
            MediaType::PsdImage,
            MediaType::Video,
            MediaType::Audio,
            MediaType::Playlist,
        ]);
        format!("MULTIMEDIA & ({})", exts)
    })
}

#[no_mangle]
pub unsafe extern "system" fn ListGetDetectString(detect_string: *mut c_char, max_len: c_int) {
    guard((), || unsafe {
        write_ansi(detect_string, max_len.max(0) as usize, wlx_detect_string());
    })
}

#[no_mangle]
pub unsafe extern "system" fn ListSetDefaultParams(dps: *mut ListDefaultParamStruct) {
    guard((), || unsafe {
        if let Some(dps) = dps.as_ref() {
            if let Some(ini) = pstr_to_path(dps.default_ini_name.as_ptr()) {
                config::set_tc_ini_path(&ini);
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn ListLoadW(
    parent_win: HWND,
    file_to_load: *const u16,
    show_flags: c_int,
) -> HWND {
    guard(HWND::default(), || unsafe {
        load(parent_win, pwstr_to_path(file_to_load), show_flags)
    })
}

#[no_mangle]
pub unsafe extern "system" fn ListLoad(
    parent_win: HWND,
    file_to_load: *const c_char,
    show_flags: c_int,
) -> HWND {
    guard(HWND::default(), || unsafe {
        load(parent_win, pstr_to_path(file_to_load), show_flags)
    })
}

#[no_mangle]
pub unsafe extern "system" fn ListLoadNextW(
    parent_win: HWND,
    list_win: HWND,
    file_to_load: *const u16,
    show_flags: c_int,
) -> c_int {
    guard(LISTPLUGIN_ERROR, || unsafe {
        load_next(
            parent_win,
            list_win,
            pwstr_to_path(file_to_load),
            show_flags,
        )
    })
}

#[no_mangle]
pub unsafe extern "system" fn ListLoadNext(
    parent_win: HWND,
    list_win: HWND,
    file_to_load: *const c_char,
    show_flags: c_int,
) -> c_int {
    guard(LISTPLUGIN_ERROR, || unsafe {
        load_next(parent_win, list_win, pstr_to_path(file_to_load), show_flags)
    })
}

#[no_mangle]
pub unsafe extern "system" fn ListCloseWindow(list_win: HWND) {
    guard((), || unsafe { window::close_viewer(list_win) })
}

/// TC consumes some Lister hotkeys itself (e.g. `F` = "fit image to window") and reports them
/// here as `LC_NEWPARAMS` instead of passing the key to the plugin window.
#[no_mangle]
pub unsafe extern "system" fn ListSendCommand(
    list_win: HWND,
    command: c_int,
    parameter: c_int,
) -> c_int {
    guard(LISTPLUGIN_ERROR, || unsafe {
        if window::send_command(list_win, command, parameter) {
            LISTPLUGIN_OK
        } else {
            LISTPLUGIN_ERROR
        }
    })
}

/// Lister's File > Print (Ctrl+P): the picture on screen, with the margins TC passes.
#[no_mangle]
pub unsafe extern "system" fn ListPrintW(
    list_win: HWND,
    _file_to_print: *const u16,
    _def_printer: *const u16,
    _print_flags: c_int,
    margins: *const RECT,
) -> c_int {
    guard(LISTPLUGIN_ERROR, || unsafe {
        print_result(window::print(list_win, margins.as_ref().copied()))
    })
}

#[no_mangle]
pub unsafe extern "system" fn ListPrint(
    list_win: HWND,
    _file_to_print: *const c_char,
    _def_printer: *const c_char,
    _print_flags: c_int,
    margins: *const RECT,
) -> c_int {
    guard(LISTPLUGIN_ERROR, || unsafe {
        print_result(window::print(list_win, margins.as_ref().copied()))
    })
}

fn print_result(printed: bool) -> c_int {
    if printed {
        LISTPLUGIN_OK
    } else {
        LISTPLUGIN_ERROR
    }
}

/// Thumbnail view of TC: a picture for the file fitted into `width` x `height` (the photo, a video
/// frame, the album art). Called on a background thread; TC takes ownership of the bitmap.
#[no_mangle]
pub unsafe extern "system" fn ListGetPreviewBitmapW(
    file_to_load: *const u16,
    width: c_int,
    height: c_int,
    _content_buf: *const c_char,
    _content_buf_len: c_int,
) -> HBITMAP {
    guard(HBITMAP::default(), || unsafe {
        preview_bitmap(pwstr_to_path(file_to_load), width, height)
    })
}

#[no_mangle]
pub unsafe extern "system" fn ListGetPreviewBitmap(
    file_to_load: *const c_char,
    width: c_int,
    height: c_int,
    _content_buf: *const c_char,
    _content_buf_len: c_int,
) -> HBITMAP {
    guard(HBITMAP::default(), || unsafe {
        preview_bitmap(pstr_to_path(file_to_load), width, height)
    })
}

unsafe fn preview_bitmap(path: Option<PathBuf>, width: c_int, height: c_int) -> HBITMAP {
    let (Some(path), Ok(w), Ok(h)) = (path, u32::try_from(width), u32::try_from(height)) else {
        return HBITMAP::default();
    };
    snapshot::thumbnail(&path, w, h)
        .and_then(|img| snapshot::to_hbitmap(&img))
        .unwrap_or_default()
}

unsafe fn load(parent: HWND, path: Option<PathBuf>, show_flags: c_int) -> HWND {
    path.and_then(|p| window::create_viewer(parent, &p, show_flags))
        .unwrap_or_default()
}

unsafe fn load_next(
    parent: HWND,
    list_win: HWND,
    path: Option<PathBuf>,
    show_flags: c_int,
) -> c_int {
    match path {
        Some(p) if window::load_next(parent, list_win, &p, show_flags) => LISTPLUGIN_OK,
        _ => LISTPLUGIN_ERROR,
    }
}
