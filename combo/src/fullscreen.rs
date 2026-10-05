//! Fullscreen toggle: the viewer is detached from the Lister into a popup covering the Lister's
//! monitor, and re-attached on exit. The popup is not topmost: Alt+Tab, Task Manager and other
//! programs can still come above it (Windows itself hides the taskbar while a monitor-sized
//! window is active). TC's own window is never restyled, so closing the Lister or switching
//! plugins while fullscreen leaves nothing to restore.
//!
//! TC keeps resizing the plugin window to the Lister's client area (`MoveWindow` in client
//! coordinates); while fullscreen, [`pin`] overrides every such move with the monitor rectangle.
//!
//! DWM draws a window's show / maximize / close animation above all other windows. When the
//! viewer starts fullscreen, TC shows its (white) Lister window only after `ListLoad`, and that
//! animation would play over the picture; so the Lister's DWM transitions are off while
//! fullscreen.

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClientRect, GetWindowLongPtrW, SetForegroundWindow, SetParent,
    SetWindowLongPtrW, SetWindowPos, GA_ROOT, GWLP_HWNDPARENT, GWL_STYLE, HWND_TOP,
    SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, WINDOWPOS, WS_CHILD,
    WS_POPUP,
};

use crate::overlay::{Fullscreen, PanelKind};
use crate::state::ViewerState;

pub unsafe fn toggle(state: &mut ViewerState) {
    if state.fullscreen.is_some() {
        exit(state);
    } else {
        enter(state);
    }
}

unsafe fn enter(state: &mut ViewerState) {
    let hwnd = state.hwnd;
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let monitor = MonitorFromWindow(state.lister, MONITOR_DEFAULTTONEAREST);
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return;
    }
    let rc = info.rcMonitor;
    state.fullscreen = Some(rc);
    set_lister_transitions(state.lister, false);

    // Detach first, then switch WS_CHILD -> WS_POPUP (order required by SetParent docs).
    let _ = SetParent(hwnd, None);
    set_style(hwnd, WS_POPUP.0, WS_CHILD.0);
    // Owned by the Lister: no taskbar button, closes with it, stays above it.
    SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, state.lister.0 as isize);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOP),
        rc.left,
        rc.top,
        rc.right - rc.left,
        rc.bottom - rc.top,
        SWP_FRAMECHANGED | SWP_SHOWWINDOW,
    );
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(Some(hwnd));
    state.overlay = Some(Fullscreen::new(hwnd, rc));
    sync_overlay(state);
}

/// After entering/leaving fullscreen, switching files or changing options: which panel floats
/// over the picture. Video: the transport bar (unless disabled — then it stays below the
/// picture); audio: none, its bar stays pinned; photos: ◀ ▶ buttons.
pub unsafe fn sync_overlay(state: &mut ViewerState) {
    let config = &state.config;
    let kind = match &state.media {
        Some(media) if media.is_video() && config.overlay_video => Some(PanelKind::Video),
        Some(_) => None,
        None if state.shows_photo() && config.overlay_photo => Some(PanelKind::Photo),
        None => None,
    };
    let autohide = config.overlay_autohide;
    let panel = state
        .overlay
        .as_mut()
        .and_then(|fs| fs.set_panel(state.hwnd, kind, autohide));
    let bar_host = if kind == Some(PanelKind::Video) {
        panel
    } else {
        None
    };
    if let Some(media) = state.media.as_mut() {
        media.set_bar_host(bar_host);
    }
}

unsafe fn exit(state: &mut ViewerState) {
    let hwnd = state.hwnd;
    if let Some(fs) = state.overlay.take() {
        fs.stop(hwnd);
    }
    sync_overlay(state);
    // Unpin first so the moves below are not overridden.
    state.fullscreen = None;
    SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, 0);
    // Switch WS_POPUP -> WS_CHILD before re-parenting.
    set_style(hwnd, WS_CHILD.0, WS_POPUP.0);
    let _ = SetParent(hwnd, Some(state.lister));

    let mut rc = RECT::default();
    let _ = GetClientRect(state.lister, &mut rc);
    let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        rc.right - rc.left,
        rc.bottom - rc.top,
        SWP_NOZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
    );
    let _ = SetForegroundWindow(state.lister);
    let _ = SetFocus(Some(hwnd));
    set_lister_transitions(state.lister, true);
}

/// Turns the DWM animations of the Lister's top-level window on or off.
unsafe fn set_lister_transitions(lister: HWND, enabled: bool) {
    let top = GetAncestor(lister, GA_ROOT);
    if top.is_invalid() {
        return;
    }
    let disabled = BOOL::from(!enabled);
    let _ = DwmSetWindowAttribute(
        top,
        DWMWA_TRANSITIONS_FORCEDISABLED,
        (&disabled as *const BOOL).cast(),
        size_of::<BOOL>() as u32,
    );
}

/// `WM_WINDOWPOSCHANGING`: keeps a fullscreen viewer covering its monitor.
pub fn pin(state: &ViewerState, pos: &mut WINDOWPOS) {
    let Some(rc) = state.fullscreen else { return };
    if !pos.flags.contains(SWP_NOMOVE) {
        pos.x = rc.left;
        pos.y = rc.top;
    }
    if !pos.flags.contains(SWP_NOSIZE) {
        pos.cx = rc.right - rc.left;
        pos.cy = rc.bottom - rc.top;
    }
}

unsafe fn set_style(hwnd: HWND, add: u32, remove: u32) {
    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
    SetWindowLongPtrW(hwnd, GWL_STYLE, ((style & !remove) | add) as isize);
}
