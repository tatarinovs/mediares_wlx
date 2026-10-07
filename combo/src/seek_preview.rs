//! The frame under the cursor on the video timeline: a small popup above the bar with the key
//! frame at that time and the time itself.
//!
//! Frames come from a thread holding the file open (`FrameGrabber`); only the latest position
//! asked for is decoded, so moving the mouse along the bar never queues work. The time shows at
//! once, the picture follows (about a tenth of a second for 1080p).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once};
use std::thread::JoinHandle;

use mediares_core::image::DynamicImage;
use mediares_core::video_frame::FrameGrabber;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, EndPaint, InvalidateRect, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, HBRUSH, PAINTSTRUCT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW, PostMessageW,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, GWLP_USERDATA, HWND_TOPMOST, MA_NOACTIVATE,
    SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, WM_APP, WM_ERASEBKGND, WM_MOUSEACTIVATE,
    WM_NCDESTROY, WM_PAINT, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::gdi::{self, Font};
use crate::image_cache::{to_bgra, DecodedImage};
use crate::image_view::draw_fitted;
use crate::module;
use crate::transport_bar::format_time;

/// Posted to the viewer when a frame for the popup is ready.
pub const WM_SEEK_PREVIEW: u32 = WM_APP + 0x14;

const CLASS_NAME: PCWSTR = w!("MediaresSeekPreview");
/// Picture box at 96 DPI (16:9); the time goes in a strip below it.
const PICTURE: (f32, f32) = (240.0, 135.0);
const TEXT_HEIGHT: f32 = 22.0;
/// Gap between the popup and the bar.
const GAP: f32 = 6.0;
const BACKGROUND: u32 = 0x0020_2020;
const TEXT_COLOR: u32 = 0x00E0_E0E0;

/// What the frame thread is asked for and what it delivered.
#[derive(Default)]
struct Exchange {
    /// The latest time asked for, not taken up yet.
    wanted: Option<f64>,
    /// The latest frame.
    done: Option<DecodedImage>,
    stop: bool,
}

struct Shared {
    exchange: Mutex<Exchange>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Exchange> {
        self.exchange.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What the popup shows.
#[derive(Default)]
struct Popup {
    time: String,
    picture: Option<DecodedImage>,
    font: Option<Font>,
}

/// The preview of one video file; closed with it.
pub struct SeekPreview {
    path: PathBuf,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    popup: HWND,
    /// Picture box size in pixels (DPI applied).
    picture: (i32, i32),
    text_height: i32,
    gap: i32,
}

impl SeekPreview {
    /// A preview for `path` reporting to `viewer`; `None` if it can't be set up.
    pub unsafe fn new(viewer: HWND, path: &Path) -> Option<Self> {
        register_class();
        let scale = gdi::dpi_scale(viewer);
        let px = |v: f32| (v * scale).round() as i32;
        let popup = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            CLASS_NAME,
            None,
            WS_POPUP,
            0,
            0,
            1,
            1,
            // Owned by the viewer's top-level window: above it and destroyed with it.
            Some(viewer),
            None,
            Some(module()),
            None,
        )
        .ok()?;
        let data = Box::new(Popup {
            font: Some(gdi::create_font("Segoe UI", -px(13.0), true)),
            ..Popup::default()
        });
        SetWindowLongPtrW(popup, GWLP_USERDATA, Box::into_raw(data) as isize);

        let shared = Arc::new(Shared {
            exchange: Mutex::new(Exchange::default()),
            wake: Condvar::new(),
        });
        let worker = {
            let (shared, path, viewer) = (shared.clone(), path.to_path_buf(), viewer.0 as isize);
            let fit = (px(PICTURE.0) as u32, px(PICTURE.1) as u32);
            std::thread::Builder::new()
                .name("mediares-seek-preview".into())
                .spawn(move || grab_loop(&path, fit, &shared, viewer))
                .ok()
        };
        Some(Self {
            path: path.to_path_buf(),
            shared,
            worker,
            popup,
            picture: (px(PICTURE.0), px(PICTURE.1)),
            text_height: px(TEXT_HEIGHT),
            gap: px(GAP),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Shows the preview of `seconds` (in `chapter`, if titled) above the point `x` of the bar
    /// whose top edge is `bar_top`, both in the client coordinates of `window`.
    pub unsafe fn show(
        &self,
        window: HWND,
        x: i32,
        bar_top: i32,
        seconds: f64,
        chapter: Option<&str>,
    ) {
        let mut anchor = POINT { x, y: bar_top };
        let _ = ClientToScreen(window, &mut anchor);
        let (w, h) = (self.picture.0, self.picture.1 + self.text_height);
        let mut bounds = RECT::default();
        let _ = GetClientRect(window, &mut bounds);
        let mut left_edge = POINT { x: 0, y: 0 };
        let mut right_edge = POINT {
            x: bounds.right,
            y: 0,
        };
        let _ = ClientToScreen(window, &mut left_edge);
        let _ = ClientToScreen(window, &mut right_edge);
        // Centred on the cursor, kept within the window.
        let left = (anchor.x - w / 2).clamp(left_edge.x, (right_edge.x - w).max(left_edge.x));
        let top = anchor.y - h - self.gap;
        let _ = SetWindowPos(
            self.popup,
            Some(HWND_TOPMOST),
            left,
            top,
            w,
            h,
            SWP_NOACTIVATE,
        );
        if let Some(popup) = popup_data(self.popup) {
            popup.time = match chapter {
                Some(title) => format!("{} · {}", format_time(seconds), title),
                None => format_time(seconds),
            };
        }
        let _ = ShowWindow(self.popup, SW_SHOWNOACTIVATE);
        let _ = InvalidateRect(Some(self.popup), None, false);
        let mut exchange = self.shared.lock();
        exchange.wanted = Some(seconds);
        self.shared.wake.notify_one();
    }

    pub unsafe fn hide(&self) {
        let _ = ShowWindow(self.popup, SW_HIDE);
        self.shared.lock().wanted = None;
    }

    /// [`WM_SEEK_PREVIEW`]: puts the frame that arrived on the popup.
    pub unsafe fn frame_ready(&self) {
        let Some(picture) = self.shared.lock().done.take() else {
            return;
        };
        if let Some(popup) = popup_data(self.popup) {
            popup.picture = Some(picture);
        }
        let _ = InvalidateRect(Some(self.popup), None, false);
    }
}

impl Drop for SeekPreview {
    fn drop(&mut self) {
        {
            let mut exchange = self.shared.lock();
            exchange.stop = true;
            self.shared.wake.notify_one();
        }
        // A frame being decoded finishes first (a fraction of a second at most).
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        unsafe {
            let _ = DestroyWindow(self.popup);
        }
    }
}

/// The frame thread: opens the file, then decodes whatever time was asked for last.
fn grab_loop(path: &Path, fit: (u32, u32), shared: &Shared, viewer: isize) {
    let grabber = FrameGrabber::open(path);
    loop {
        let seconds = {
            let mut exchange = shared.lock();
            loop {
                if exchange.stop {
                    return;
                }
                if let Some(t) = exchange.wanted.take() {
                    break t;
                }
                exchange = shared
                    .wake
                    .wait(exchange)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        // Without a grabber (a file Media Foundation can't read) the popup shows the time only.
        let Some(frame) = grabber
            .as_ref()
            .and_then(|g| g.frame_at(seconds, Some(fit)))
            .map(|(frame, _)| frame)
        else {
            continue;
        };
        shared.lock().done = Some(to_bgra(DynamicImage::ImageRgba8(frame), BACKGROUND, false));
        unsafe {
            let _ = PostMessageW(
                Some(HWND(viewer as *mut _)),
                WM_SEEK_PREVIEW,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

unsafe fn popup_data<'a>(popup: HWND) -> Option<&'a mut Popup> {
    (GetWindowLongPtrW(popup, GWLP_USERDATA) as *mut Popup).as_mut()
}

unsafe fn register_class() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        gdi::register_class(
            CLASS_NAME,
            Some(wnd_proc),
            Default::default(),
            HBRUSH::default(),
        );
    });
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    mediares_core::ffi::guard(LRESULT(0), || unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let data = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut Popup;
                if !data.is_null() {
                    drop(Box::from_raw(data));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    })
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    if let Some(popup) = popup_data(hwnd) {
        gdi::with_buffer(hdc, rc.right, rc.bottom, rc, |dc| unsafe {
            gdi::fill(dc, rc, BACKGROUND);
            let text_height =
                (rc.bottom * TEXT_HEIGHT as i32 / (PICTURE.1 + TEXT_HEIGHT) as i32).max(1);
            let picture_box = RECT {
                bottom: rc.bottom - text_height,
                ..rc
            };
            if let Some(picture) = &popup.picture {
                draw_fitted(dc, picture, picture_box, false);
            }
            let strip = RECT {
                top: picture_box.bottom,
                ..rc
            };
            gdi::text(
                dc,
                strip,
                &popup.time,
                popup.font.as_ref().map(|f| f.0),
                TEXT_COLOR,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        });
    }
    let _ = EndPaint(hwnd, &ps);
}
