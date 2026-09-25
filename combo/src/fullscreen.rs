//! Fullscreen toggle: the viewer is detached from the Lister into a topmost popup covering the
//! Lister's monitor, and re-attached on exit. TC's own window is never restyled, so closing
//! the Lister or switching plugins while fullscreen leaves nothing to restore.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowLongPtrW, SetForegroundWindow, SetParent, SetWindowLongPtrW, SetWindowPos,
    GWLP_HWNDPARENT, GWL_STYLE, HWND_NOTOPMOST, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, WS_CHILD, WS_POPUP,
};

use crate::state::ViewerState;

pub unsafe fn toggle(state: &mut ViewerState) {
    if state.fullscreen {
        exit(state);
    } else {
        enter(state);
    }
}

unsafe fn enter(state: &mut ViewerState) {
    let hwnd = state.hwnd;
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    let monitor = MonitorFromWindow(state.lister, MONITOR_DEFAULTTONEAREST);
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return;
    }
    let rc = info.rcMonitor;

    // Detach first, then switch WS_CHILD -> WS_POPUP (order required by SetParent docs).
    let _ = SetParent(hwnd, None);
    set_style(hwnd, WS_POPUP.0, WS_CHILD.0);
    // Owned by the Lister: no taskbar button, closes with it, stays above it.
    SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, state.lister.0 as isize);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        rc.left,
        rc.top,
        rc.right - rc.left,
        rc.bottom - rc.top,
        SWP_FRAMECHANGED | SWP_SHOWWINDOW,
    );
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(Some(hwnd));
    state.fullscreen = true;
}

unsafe fn exit(state: &mut ViewerState) {
    let hwnd = state.hwnd;
    let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, 0);
    // Switch WS_POPUP -> WS_CHILD before re-parenting.
    set_style(hwnd, WS_CHILD.0, WS_POPUP.0);
    let _ = SetParent(hwnd, Some(state.lister));

    let mut rc = RECT::default();
    let _ = GetClientRect(state.lister, &mut rc);
    let _ = SetWindowPos(hwnd, None, 0, 0, rc.right - rc.left, rc.bottom - rc.top, SWP_NOZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW);
    let _ = SetForegroundWindow(state.lister);
    let _ = SetFocus(Some(hwnd));
    state.fullscreen = false;
}

unsafe fn set_style(hwnd: HWND, add: u32, remove: u32) {
    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
    SetWindowLongPtrW(hwnd, GWL_STYLE, ((style & !remove) | add) as isize);
}
