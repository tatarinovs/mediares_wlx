//! Shared plumbing for the plugin's modal dialogs (EXIF viewer, settings).
//!
//! Layouts and font sizes are written for 96 DPI; [`create_frame`], [`control`] and [`font`] scale
//! them to the DPI of the window they go into.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_BTNFACE, HBRUSH, HFONT};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetAncestor, GetDlgItem, GetDlgItemTextW,
    GetMessageW, GetWindow, GetWindowRect, GetWindowTextLengthW, IsDialogMessageW, IsWindow,
    PostQuitMessage, SendMessageW, SetForegroundWindow, ShowWindow, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, GA_ROOT, GW_OWNER, HMENU, MSG, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_SETFONT,
    WNDPROC, WS_CAPTION, WS_CHILD, WS_EX_DLGMODALFRAME, WS_POPUP, WS_SYSMENU, WS_VISIBLE,
};

use crate::gdi::{self, Font};
use crate::module;

pub const SS_LEFT: u32 = 0x0000;
pub const ES_AUTOHSCROLL: u32 = 0x0080;

/// `v` (at 96 DPI) in pixels of `hwnd`'s monitor.
pub fn px(hwnd: HWND, v: i32) -> i32 {
    (v as f32 * gdi::dpi_scale(hwnd)).round() as i32
}

/// A font for the controls of `dlg`; `height` at 96 DPI, as `CreateFontW` takes it.
pub unsafe fn font(dlg: HWND, face: &str, height: i32) -> Font {
    gdi::create_font(face, px(dlg, height), false)
}

/// The text of the control `id`.
pub unsafe fn item_text(dlg: HWND, id: i32) -> String {
    let len = GetDlgItem(Some(dlg), id).map_or(0, |item| GetWindowTextLengthW(item));
    let mut buf = vec![0u16; usize::try_from(len).unwrap_or(0) + 1];
    let len = GetDlgItemTextW(dlg, id, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..len])
}

/// Registers a dialog window class on this DLL. Repeated calls fail harmlessly.
pub unsafe fn register_class(name: PCWSTR, proc: WNDPROC) {
    // System color index + 1: never deleted, unlike a brush we create ourselves.
    let background = HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _);
    crate::gdi::register_class(name, proc, CS_HREDRAW | CS_VREDRAW, background);
}

/// The top-level window a dialog opened from `hwnd` should be modal to.
pub unsafe fn modal_owner(hwnd: HWND) -> HWND {
    GetAncestor(hwnd, GA_ROOT)
}

/// Creates a dialog frame centered over `owner`.
pub unsafe fn create_frame(
    owner: HWND,
    class: PCWSTR,
    title: &str,
    width: i32,
    height: i32,
) -> Option<HWND> {
    let (width, height) = (px(owner, width), px(owner, height));
    let mut rc = RECT::default();
    let (x, y) = if GetWindowRect(owner, &mut rc).is_ok() {
        (
            (rc.left + rc.right - width) / 2,
            (rc.top + rc.bottom - height) / 2,
        )
    } else {
        (200, 200)
    };
    CreateWindowExW(
        WS_EX_DLGMODALFRAME,
        class,
        &HSTRING::from(title),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        x.max(0),
        y.max(0),
        width,
        height,
        Some(owner),
        None,
        Some(module()),
        None,
    )
    .ok()
}

/// Creates a child control with the given font. `style` is added to `WS_CHILD | WS_VISIBLE`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn control(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: u32,
    (x, y, w, h): (i32, i32, i32, i32),
    id: usize,
    font: HFONT,
) -> HWND {
    let [x, y, w, h] = [x, y, w, h].map(|v| px(parent, v));
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        &HSTRING::from(text),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
        x,
        y,
        w,
        h,
        Some(parent),
        Some(HMENU(id as *mut _)),
        Some(module()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(
        hwnd,
        WM_SETFONT,
        Some(WPARAM(font.0 as usize)),
        Some(LPARAM(1)),
    );
    hwnd
}

/// Shows `dlg` modally: disables its owner and pumps messages until the dialog is destroyed.
pub unsafe fn run_modal(dlg: HWND, focus: HWND) {
    let owner = GetWindow(dlg, GW_OWNER).unwrap_or_default();
    if !owner.is_invalid() {
        let _ = EnableWindow(owner, false);
    }
    let _ = ShowWindow(dlg, SW_SHOW);
    let _ = SetFocus(Some(focus));

    let mut msg = MSG::default();
    while IsWindow(Some(dlg)).as_bool() {
        match GetMessageW(&mut msg, None, 0, 0).0 {
            // WM_QUIT belongs to the host's main loop: hand it back.
            0 => {
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            -1 => break,
            _ => {}
        }
        if !IsDialogMessageW(dlg, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    if IsWindow(Some(dlg)).as_bool() {
        close(dlg);
    }
    if IsWindow(Some(owner)).as_bool() {
        let _ = EnableWindow(owner, true);
        let _ = SetForegroundWindow(owner);
    }
}

/// Closes a modal dialog, re-enabling its owner first so activation returns to it.
pub unsafe fn close(dlg: HWND) {
    if let Ok(owner) = GetWindow(dlg, GW_OWNER) {
        let _ = EnableWindow(owner, true);
    }
    let _ = DestroyWindow(dlg);
}

/// Low word of `WPARAM` (control/command id).
pub fn loword(wparam: WPARAM) -> usize {
    wparam.0 & 0xFFFF
}
