//! Fullscreen extras: a panel floating over the picture — the transport bar for video, or ◀ ▶
//! buttons for photos — and hiding the idle cursor.
//!
//! The panel is a semi-transparent layered popup owned by the fullscreen viewer, so it floats
//! above the picture (including the video swap chain, which GDI in the viewer could not paint
//! over) without moving it. It never takes focus: keys keep going to the viewer. With auto-hide
//! it shows up only when the mouse comes near the bottom edge and hides after a few idle seconds
//! (not while the cursor is over it or a slider is dragged). Audio keeps its bar pinned instead.
//!
//! A close button of the same kind sits in the top right corner for any content: it shows up
//! when the mouse comes near the top edge and always hides with the idle cursor.

use std::sync::Once;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateRoundRectRgn, InvalidateRect, LineTo, MoveToEx, SelectObject, SetWindowRgn,
    HDC, PS_SOLID,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetForegroundWindow,
    GetGUIThreadInfo, GetWindowLongPtrW, GetWindowRect, KillTimer, LoadCursorW, SetCursor,
    SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, ShowWindow, GUITHREADINFO,
    GUI_INMENUMODE, GUI_POPUPMENUMODE, GWLP_USERDATA, IDC_ARROW, LWA_ALPHA, MA_NOACTIVATE, SW_HIDE,
    SW_SHOWNOACTIVATE, WM_ERASEBKGND, WM_MOUSEACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::gdi;
use crate::module;
use crate::transport_bar;

const CLASS_NAME: PCWSTR = w!("MediaresOverlay");
/// Viewer timer: hides the idle cursor and the panel.
pub const OVERLAY_TIMER_ID: usize = 0x4D55;
const IDLE_MS: u32 = 2500;
const OPACITY: u8 = 215;
/// The panel appears when the cursor is this many panel heights from the bottom edge (the close
/// button: from the top edge).
const APPROACH_HEIGHTS: i32 = 3;
/// Close button under the cursor, as Windows paints it (#C42B1C; COLORREF is BGR).
const CLOSE_HOT: u32 = 0x001C2BC4;
const CLOSE_HOT_ICON: u32 = 0x00FFFFFF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    /// The transport bar across the bottom.
    Video,
    /// A small ⏮ ⏯ ⏭ box at the bottom centre.
    Photo,
    /// The close button in the top right corner.
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhotoButton {
    Previous,
    /// Start / stop the slideshow.
    Slideshow,
    Next,
}

/// Photo box geometry: (outer size, button rectangles in its client coordinates).
pub fn photo_layout(dpi_scale: f32) -> ((i32, i32), [(PhotoButton, RECT); 3]) {
    let h = transport_bar::height(dpi_scale);
    let pad = (8.0 * dpi_scale).round() as i32;
    let button = |i: i32| RECT {
        left: pad + h * i,
        top: 0,
        right: pad + h * (i + 1),
        bottom: h,
    };
    (
        (3 * h + 2 * pad, h),
        [
            (PhotoButton::Previous, button(0)),
            (PhotoButton::Slideshow, button(1)),
            (PhotoButton::Next, button(2)),
        ],
    )
}

pub fn hit_photo(dpi_scale: f32, x: i32, y: i32) -> Option<PhotoButton> {
    let (_, buttons) = photo_layout(dpi_scale);
    buttons
        .iter()
        .find(|(_, r)| gdi::contains(r, x, y))
        .map(|(b, _)| *b)
}

/// The floating panel window.
pub struct Panel {
    pub hwnd: HWND,
    pub kind: PanelKind,
    pub visible: bool,
}

impl Panel {
    fn show(&mut self) {
        if !self.visible {
            self.visible = true;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
    }

    fn hide(&mut self) {
        if self.visible {
            self.visible = false;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }

    unsafe fn under_cursor(&self) -> bool {
        if !self.visible {
            return false;
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mut rc = RECT::default();
        let _ = GetWindowRect(self.hwnd, &mut rc);
        gdi::contains(&rc, pt.x, pt.y)
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Fullscreen state of the viewer: the optional panel, the close button and the cursor.
pub struct Fullscreen {
    monitor: RECT,
    pub panel: Option<Panel>,
    close: Option<Panel>,
    /// The cursor is over the close button (painted red).
    close_hot: bool,
    autohide: bool,
    cursor_hidden: bool,
    last_cursor: POINT,
}

unsafe extern "system" fn panel_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    mediares_core::ffi::guard(LRESULT(0), || unsafe {
        match msg {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_ERASEBKGND => return LRESULT(1),
            _ => {}
        }
        // Mouse input and painting are handled by the viewer window module.
        let viewer = HWND(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut _);
        let handled = if viewer.is_invalid() {
            None
        } else {
            crate::window::overlay_message(viewer, hwnd, msg, wparam, lparam)
        };
        handled.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
    })
}

unsafe fn register_class() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        gdi::register_class(
            CLASS_NAME,
            Some(panel_proc),
            Default::default(),
            Default::default(),
        );
    });
}

unsafe fn create_panel(viewer: HWND, monitor: RECT, kind: PanelKind) -> Option<Panel> {
    register_class();
    let scale = gdi::dpi_scale(viewer);
    let h = transport_bar::height(scale);
    let rect = match kind {
        PanelKind::Video => RECT {
            top: monitor.bottom - h,
            ..monitor
        },
        PanelKind::Photo => {
            let ((w, h), _) = photo_layout(scale);
            let margin = (24.0 * scale).round() as i32;
            let left = (monitor.left + monitor.right - w) / 2;
            RECT {
                left,
                top: monitor.bottom - margin - h,
                right: left + w,
                bottom: monitor.bottom - margin,
            }
        }
        // Flush with the corner, so a flick of the mouse into it hits the button.
        PanelKind::Close => RECT {
            left: monitor.right - h * 3 / 2,
            bottom: monitor.top + h,
            ..monitor
        },
    };
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        CLASS_NAME,
        None,
        WS_POPUP,
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        // Owned by the viewer: always above it, destroyed with it.
        Some(viewer),
        None,
        Some(module()),
        None,
    )
    .ok()?;
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, viewer.0 as isize);
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), OPACITY, LWA_ALPHA);
    if kind == PanelKind::Photo {
        let radius = (12.0 * scale).round() as i32;
        let region = CreateRoundRectRgn(
            0,
            0,
            rect.right - rect.left + 1,
            rect.bottom - rect.top + 1,
            radius,
            radius,
        );
        // The window owns the region from here on.
        SetWindowRgn(hwnd, Some(region), false);
    }
    Some(Panel {
        hwnd,
        kind,
        visible: false,
    })
}

impl Fullscreen {
    pub unsafe fn new(viewer: HWND, monitor: RECT) -> Self {
        let mut fs = Self {
            monitor,
            panel: None,
            close: create_panel(viewer, monitor, PanelKind::Close),
            close_hot: false,
            autohide: true,
            cursor_hidden: false,
            last_cursor: POINT::default(),
        };
        let _ = GetCursorPos(&mut fs.last_cursor);
        SetTimer(Some(viewer), OVERLAY_TIMER_ID, IDLE_MS, None);
        fs
    }

    /// Creates, replaces or removes the panel (after switching files or changing options).
    /// Returns the panel window if there is one.
    pub unsafe fn set_panel(
        &mut self,
        viewer: HWND,
        kind: Option<PanelKind>,
        autohide: bool,
    ) -> Option<HWND> {
        self.autohide = autohide;
        if self.panel.as_ref().map(|p| p.kind) != kind {
            self.panel = kind.and_then(|k| create_panel(viewer, self.monitor, k));
            // A new panel is shown once, so the user sees it's there.
            if self.panel.is_some() {
                self.show_panel();
                SetTimer(Some(viewer), OVERLAY_TIMER_ID, IDLE_MS, None);
            }
        }
        if !autohide {
            self.show_panel();
        }
        self.panel.as_ref().map(|p| p.hwnd)
    }

    fn show_panel(&mut self) {
        if let Some(panel) = self.panel.as_mut() {
            panel.show();
        }
    }

    unsafe fn show_close(&mut self) {
        let Some(close) = self.close.as_mut() else {
            return;
        };
        close.show();
        // A flick into the corner may stop right on it: no mouse move would light it up then.
        let hot = close.under_cursor();
        self.set_close_hot(hot);
    }

    /// Which of the floating windows `window` is.
    pub fn kind_of(&self, window: HWND) -> Option<PanelKind> {
        [&self.panel, &self.close]
            .into_iter()
            .flatten()
            .find(|p| p.hwnd == window)
            .map(|p| p.kind)
    }

    /// Viewer mouse move. Only a real movement counts (Windows also sends WM_MOUSEMOVE when
    /// windows appear or disappear under a still cursor).
    pub unsafe fn mouse_moved(&mut self, viewer: HWND) {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        if pt.x == self.last_cursor.x && pt.y == self.last_cursor.y {
            return;
        }
        self.last_cursor = pt;
        self.cursor_hidden = false;
        let zone = transport_bar::height(gdi::dpi_scale(viewer)) * APPROACH_HEIGHTS;
        if pt.y >= self.monitor.bottom - zone {
            self.show_panel();
        }
        if pt.y < self.monitor.top + zone {
            self.show_close();
        }
        SetTimer(Some(viewer), OVERLAY_TIMER_ID, IDLE_MS, None);
    }

    /// Use of the panel or the close button itself: keep it (and the cursor) on screen.
    pub unsafe fn panel_used(&mut self, viewer: HWND, kind: PanelKind) {
        self.cursor_hidden = false;
        if kind == PanelKind::Close {
            self.show_close();
        } else {
            self.show_panel();
        }
        SetTimer(Some(viewer), OVERLAY_TIMER_ID, IDLE_MS, None);
    }

    /// The cursor came onto (or left) the close button: it turns red under the cursor.
    pub unsafe fn set_close_hot(&mut self, hot: bool) {
        let Some(close) = self.close.as_ref().filter(|_| hot != self.close_hot) else {
            return;
        };
        self.close_hot = hot;
        let _ = InvalidateRect(Some(close.hwnd), None, false);
        if hot {
            // WM_MOUSELEAVE turns it back.
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: close.hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut track);
        }
    }

    pub fn close_hot(&self) -> bool {
        self.close_hot
    }

    /// `OVERLAY_TIMER_ID`: hides the cursor and (with auto-hide) the panel, unless the user is
    /// working with the panel.
    pub unsafe fn idle(&mut self, viewer: HWND, busy: bool) {
        if busy || self.cursor_over_panel() || ui_elsewhere(viewer) {
            return;
        }
        let _ = KillTimer(Some(viewer), OVERLAY_TIMER_ID);
        self.cursor_hidden = true;
        SetCursor(None);
        if self.autohide {
            if let Some(panel) = self.panel.as_mut() {
                panel.hide();
            }
        }
        if let Some(close) = self.close.as_mut() {
            close.hide();
        }
    }

    /// Shows the cursor now (e.g. a context menu opens after a right click that didn't move it).
    pub unsafe fn reveal_cursor(&mut self, viewer: HWND) {
        self.cursor_hidden = false;
        if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
            SetCursor(Some(arrow));
        }
        SetTimer(Some(viewer), OVERLAY_TIMER_ID, IDLE_MS, None);
    }

    /// The viewer shows no cursor while idle.
    pub fn cursor_hidden(&self) -> bool {
        self.cursor_hidden
    }

    unsafe fn cursor_over_panel(&self) -> bool {
        [&self.panel, &self.close]
            .into_iter()
            .flatten()
            .any(|p| p.under_cursor())
    }

    pub unsafe fn invalidate_panel(&self) {
        if let Some(panel) = &self.panel {
            let _ = InvalidateRect(Some(panel.hwnd), None, false);
        }
    }

    pub unsafe fn stop(&self, viewer: HWND) {
        let _ = KillTimer(Some(viewer), OVERLAY_TIMER_ID);
    }
}

/// A menu is open or another window (a dialog, another program) is active: the cursor belongs
/// to it and must not be hidden.
unsafe fn ui_elsewhere(viewer: HWND) -> bool {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let in_menu = GetGUIThreadInfo(0, &mut info).is_ok()
        && (info.flags & (GUI_INMENUMODE | GUI_POPUPMENUMODE)).0 != 0;
    in_menu || GetForegroundWindow() != viewer
}

/// Paints the photo box; `playing`: the slideshow runs (the middle button shows pause).
pub unsafe fn paint_photo_panel(dc: HDC, rc: RECT, dpi_scale: f32, playing: bool) {
    gdi::fill(dc, rc, transport_bar::BG);
    let (_, buttons) = photo_layout(dpi_scale);
    for (button, r) in buttons {
        match button {
            PhotoButton::Previous => transport_bar::skip_icon(dc, r, false, dpi_scale),
            PhotoButton::Slideshow => transport_bar::play_icon(dc, r, playing, dpi_scale),
            PhotoButton::Next => transport_bar::skip_icon(dc, r, true, dpi_scale),
        }
    }
}

/// Paints the close button: a thin cross, on red while `hot` (under the cursor).
pub unsafe fn paint_close_button(dc: HDC, rc: RECT, dpi_scale: f32, hot: bool) {
    let (bg, icon) = if hot {
        (CLOSE_HOT, CLOSE_HOT_ICON)
    } else {
        (transport_bar::BG, transport_bar::ICON)
    };
    gdi::fill(dc, rc, bg);
    let arm = (6.0 * dpi_scale).round() as i32;
    let width = dpi_scale.round().max(1.0) as i32;
    let (cx, cy) = ((rc.left + rc.right) / 2, (rc.top + rc.bottom) / 2);
    let pen = gdi::GdiObject(CreatePen(PS_SOLID, width, COLORREF(icon)));
    let old = SelectObject(dc, pen.0.into());
    // LineTo leaves out the last pixel: one more makes both strokes symmetric.
    for (from_x, to_x) in [(cx - arm, cx + arm + 1), (cx + arm, cx - arm - 1)] {
        let _ = MoveToEx(dc, from_x, cy - arm, None);
        let _ = LineTo(dc, to_x, cy + arm + 1);
    }
    SelectObject(dc, old);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photo_buttons() {
        let ((w, h), _) = photo_layout(1.0);
        assert_eq!((w, h), (136, 40));
        assert_eq!(hit_photo(1.0, 10, 20), Some(PhotoButton::Previous));
        assert_eq!(hit_photo(1.0, 68, 20), Some(PhotoButton::Slideshow));
        assert_eq!(hit_photo(1.0, 125, 20), Some(PhotoButton::Next));
        assert_eq!(hit_photo(1.0, 2, 20), None);
    }
}
