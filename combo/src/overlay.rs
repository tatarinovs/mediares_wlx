//! Fullscreen extras: a panel floating over the picture — the transport bar for video, or ◀ ▶
//! buttons for photos — and hiding the idle cursor.
//!
//! The panel is a semi-transparent layered popup owned by the fullscreen viewer, so it floats
//! above the picture (including the video swap chain, which GDI in the viewer could not paint
//! over) without moving it. It never takes focus: keys keep going to the viewer. With auto-hide
//! it shows up only when the mouse comes near the bottom edge and hides after a few idle seconds
//! (not while the cursor is over it or a slider is dragged). Audio keeps its bar pinned instead.

use std::sync::Once;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateRoundRectRgn,
    CreateSolidBrush, DeleteDC, DeleteObject, FillRect, Polygon, SelectObject, SetWindowRgn, HDC,
    PS_NULL, SRCCOPY,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetForegroundWindow,
    GetGUIThreadInfo, GetWindowLongPtrW, GetWindowRect, KillTimer, LoadCursorW, RegisterClassExW,
    SetCursor, SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, ShowWindow, GUITHREADINFO,
    GUI_INMENUMODE, GUI_POPUPMENUMODE, GWLP_USERDATA, IDC_ARROW, LWA_ALPHA, MA_NOACTIVATE, SW_HIDE,
    SW_SHOWNOACTIVATE, WM_ERASEBKGND, WM_MOUSEACTIVATE, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::module;
use crate::transport_bar;

const CLASS_NAME: PCWSTR = w!("MediaresOverlay");
/// Viewer timer: hides the idle cursor and the panel.
pub const OVERLAY_TIMER_ID: usize = 0x4D55;
const IDLE_MS: u32 = 2500;
const OPACITY: u8 = 215;
const BG: u32 = 0x00202020;
const ICON: u32 = 0x00E8E8E8;
/// The panel appears when the cursor is this many panel heights from the bottom edge.
const APPROACH_HEIGHTS: i32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    /// The transport bar across the bottom.
    Video,
    /// A small ⏮ ⏯ ⏭ box at the bottom centre.
    Photo,
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
        .find(|(_, r)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
        .map(|(b, _)| *b)
}

pub fn dpi_scale(hwnd: HWND) -> f32 {
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => 1.0,
        dpi => dpi as f32 / 96.0,
    }
}

/// The floating panel window.
pub struct Panel {
    pub hwnd: HWND,
    pub kind: PanelKind,
    pub visible: bool,
}

impl Drop for Panel {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Fullscreen state of the viewer: the optional panel and the cursor.
pub struct Fullscreen {
    monitor: RECT,
    pub panel: Option<Panel>,
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
            crate::window::overlay_message(viewer, hwnd, msg, lparam)
        };
        handled.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
    })
}

unsafe fn register_class() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(panel_proc),
            hInstance: module(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    });
}

unsafe fn create_panel(viewer: HWND, monitor: RECT, kind: PanelKind) -> Option<Panel> {
    register_class();
    let scale = dpi_scale(viewer);
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
    };
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
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
        if let Some(panel) = self.panel.as_mut().filter(|p| !p.visible) {
            panel.visible = true;
            unsafe {
                let _ = ShowWindow(panel.hwnd, SW_SHOWNOACTIVATE);
            }
        }
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
        let zone = transport_bar::height(dpi_scale(viewer)) * APPROACH_HEIGHTS;
        if pt.y >= self.monitor.bottom - zone {
            self.show_panel();
        }
        SetTimer(Some(viewer), OVERLAY_TIMER_ID, IDLE_MS, None);
    }

    /// Use of the panel itself: keep it (and the cursor) on screen.
    pub unsafe fn panel_used(&mut self, viewer: HWND) {
        self.cursor_hidden = false;
        self.show_panel();
        SetTimer(Some(viewer), OVERLAY_TIMER_ID, IDLE_MS, None);
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
            if let Some(panel) = self.panel.as_mut().filter(|p| p.visible) {
                panel.visible = false;
                let _ = ShowWindow(panel.hwnd, SW_HIDE);
            }
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
        let Some(panel) = self.panel.as_ref().filter(|p| p.visible) else {
            return false;
        };
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mut rc = RECT::default();
        let _ = GetWindowRect(panel.hwnd, &mut rc);
        pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom
    }

    pub unsafe fn invalidate_panel(&self) {
        if let Some(panel) = &self.panel {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(panel.hwnd), None, false);
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

/// Double-buffered painting of a whole window.
pub unsafe fn with_buffer(hdc: HDC, rc: RECT, paint: impl FnOnce(HDC)) {
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        return;
    }
    let mem = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(mem, bmp.into());
    paint(mem);
    let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
    SelectObject(mem, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem);
}

/// Paints the photo box; `playing`: the slideshow runs (the middle button shows pause).
pub unsafe fn paint_photo_panel(dc: HDC, rc: RECT, dpi_scale: f32, playing: bool) {
    let brush = CreateSolidBrush(COLORREF(BG));
    FillRect(dc, &rc, brush);
    let _ = DeleteObject(brush.into());

    let icon = CreateSolidBrush(COLORREF(ICON));
    let pen = CreatePen(PS_NULL, 0, COLORREF(0));
    let (ob, op) = (SelectObject(dc, icon.into()), SelectObject(dc, pen.into()));
    let u = (7.0 * dpi_scale).round() as i32;
    let bar = (2.0 * dpi_scale).round().max(1.0) as i32;
    let block = |l: i32, t: i32, r: i32, b: i32| {
        FillRect(
            dc,
            &RECT {
                left: l,
                top: t,
                right: r,
                bottom: b,
            },
            icon,
        );
    };
    let (_, buttons) = photo_layout(dpi_scale);
    for (button, r) in buttons {
        let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        let p = |x: i32, y: i32| POINT { x, y };
        match button {
            // Previous / next: a triangle ending in a bar, as on the transport bar.
            PhotoButton::Previous => {
                let _ = Polygon(
                    dc,
                    &[
                        p(cx + u / 2, cy - u),
                        p(cx + u / 2, cy + u),
                        p(cx - u / 2, cy),
                    ],
                );
                block(cx - u / 2 - bar, cy - u, cx - u / 2, cy + u);
            }
            PhotoButton::Next => {
                let _ = Polygon(
                    dc,
                    &[
                        p(cx - u / 2, cy - u),
                        p(cx - u / 2, cy + u),
                        p(cx + u / 2, cy),
                    ],
                );
                block(cx + u / 2, cy - u, cx + u / 2 + bar, cy + u);
            }
            PhotoButton::Slideshow if playing => {
                block(cx - u * 2 / 3, cy - u, cx - u / 5, cy + u);
                block(cx + u / 5, cy - u, cx + u * 2 / 3, cy + u);
            }
            PhotoButton::Slideshow => {
                let _ = Polygon(
                    dc,
                    &[
                        p(cx - u * 2 / 3, cy - u),
                        p(cx - u * 2 / 3, cy + u),
                        p(cx + u, cy),
                    ],
                );
            }
        }
    }
    SelectObject(dc, op);
    SelectObject(dc, ob);
    let _ = DeleteObject(pen.into());
    let _ = DeleteObject(icon.into());
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
