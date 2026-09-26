//! Shared plumbing for the plugin's modal dialogs (EXIF viewer, settings).

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, CLIP_DEFAULT_PRECIS, COLOR_BTNFACE, DEFAULT_CHARSET,
    DEFAULT_QUALITY, FW_NORMAL, HBRUSH, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetAncestor, GetMessageW, GetWindow,
    GetWindowRect, IsDialogMessageW, IsWindow, LoadCursorW, PostQuitMessage, RegisterClassExW,
    SendMessageW, SetForegroundWindow, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    GA_ROOT, GW_OWNER, HMENU, IDC_ARROW, MSG, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_SETFONT,
    WNDCLASSEXW, WNDPROC, WS_CAPTION, WS_CHILD, WS_EX_DLGMODALFRAME, WS_POPUP, WS_SYSMENU,
    WS_VISIBLE,
};

use crate::module;

/// Owned GDI object, deleted on drop.
pub struct GdiObject<T: Into<HGDIOBJ> + Copy>(pub T);

impl<T: Into<HGDIOBJ> + Copy> Drop for GdiObject<T> {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}

pub type Font = GdiObject<HFONT>;
pub type Brush = GdiObject<HBRUSH>;

/// Registers a dialog window class on this DLL. Repeated calls fail harmlessly.
pub unsafe fn register_class(name: PCWSTR, proc: WNDPROC) {
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: proc,
        hInstance: module(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        // System color index + 1: never deleted, unlike a brush we create ourselves.
        hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
        lpszClassName: name,
        ..Default::default()
    };
    RegisterClassExW(&wc);
}

pub unsafe fn create_font(face: PCWSTR, height: i32) -> Font {
    GdiObject(CreateFontW(
        height,
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        DEFAULT_QUALITY,
        0,
        face,
    ))
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
