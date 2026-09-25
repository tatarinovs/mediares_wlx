//! A playable file in the viewer — video or audio — with the transport bar along the bottom.
//!
//! The content ([`VideoView`] / [`AudioView`]) owns the area above the bar and the player; this
//! module owns the bar: layout, painting, mouse input, and the commands the window forwards.

use std::path::Path;
use std::sync::Arc;

use mediares_core::probe::MediaType;
use windows::core::w;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, FillRect, GetStockObject,
    BeginPaint, EndPaint, InvalidateRect, IntersectClipRect, SelectObject, BLACK_BRUSH, HBRUSH, HDC, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, KillTimer, SetTimer};

use crate::audio_view::{AudioView, PROGRESS_TIMER_ID};
use crate::dialog::{self, Font};
use crate::image_cache::DecodedImage;
use crate::transport_bar::{self, BarControl, Click, Layout, Message, Transport};
use crate::video_view::{VideoView, RENDER_TIMER_ID};

/// Clears a status message from the bar.
pub const STATUS_TIMER_ID: usize = 0x4D54;
const STATUS_MS: u32 = 3000;

/// What the viewer should do after an engine event or a timer tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventEffect {
    None,
    RepaintBar,
    /// Video size known/changed: re-layout and refresh the title.
    Relayout,
    /// Played to the end: the play queue decides what comes next.
    Ended,
}

pub enum Content {
    Video(Box<VideoView>),
    Audio(Box<AudioView>),
}

pub struct MediaView {
    pub content: Content,
    viewer: HWND,
    bar: BarControl,
    /// Show previous / next buttons (there are other playable files around).
    skip: bool,
    /// Bar font and the DPI it was made for.
    font: Option<(u32, Font)>,
    /// Short-lived message in place of the timeline ("frame saved...").
    status: Option<String>,
    /// Fullscreen: the bar lives in this overlay window and the content takes the whole viewer.
    bar_host: Option<HWND>,
}

impl MediaView {
    /// Opens `path` (`kind` must be playable), reusing `previous` when it shows the same kind of
    /// content. `None` if the file can't be played.
    pub unsafe fn open(viewer: HWND, previous: Option<MediaView>, path: &Path, kind: MediaType) -> Option<MediaView> {
        if let Some(mut view) = previous {
            let reused = match &mut view.content {
                Content::Video(v) if kind == MediaType::Video => v.open(path),
                Content::Audio(a) if kind == MediaType::Audio => a.open(path),
                _ => false,
            };
            if reused {
                view.bar.cancel();
                view.layout();
                return Some(view);
            }
            // Stop the old player before starting the new one.
            drop(view);
        }
        let content = match kind {
            MediaType::Video => Content::Video(Box::new(VideoView::new(viewer, path)?)),
            MediaType::Audio => Content::Audio(Box::new(AudioView::new(viewer, path)?)),
            _ => return None,
        };
        let view = MediaView { content, viewer, bar: BarControl::default(), skip: false, font: None, status: None, bar_host: None };
        view.layout();
        Some(view)
    }

    pub fn transport(&self) -> &dyn Transport {
        match &self.content {
            Content::Video(v) => v.transport(),
            Content::Audio(a) => a.transport(),
        }
    }

    pub fn is_video(&self) -> bool {
        matches!(self.content, Content::Video(_))
    }

    fn known_duration(&self) -> f64 {
        match &self.content {
            Content::Video(v) => v.info.duration_sec,
            Content::Audio(a) => a.known_duration(),
        }
    }

    fn error(&self) -> Option<&str> {
        match &self.content {
            Content::Video(v) => v.error(),
            Content::Audio(a) => a.error(),
        }
    }

    pub fn set_skip(&mut self, skip: bool) {
        if self.skip != skip {
            self.skip = skip;
            unsafe { self.invalidate_bar() };
        }
    }

    fn dpi_scale(&self) -> f32 {
        match unsafe { GetDpiForWindow(self.viewer) } {
            0 => 1.0,
            dpi => dpi as f32 / 96.0,
        }
    }

    /// Moves the bar into an overlay window (`Some`) or back to the bottom of the viewer.
    pub unsafe fn set_bar_host(&mut self, host: Option<HWND>) {
        if self.bar_host != host {
            self.bar_host = host;
            self.bar.cancel();
            self.layout();
        }
    }

    /// The bar in the coordinates of the window that shows it.
    unsafe fn bar_layout(&self) -> Layout {
        let mut rc = RECT::default();
        let _ = GetClientRect(self.bar_host.unwrap_or(self.viewer), &mut rc);
        transport_bar::layout(rc.right, rc.bottom, self.dpi_scale(), self.skip)
    }

    /// The viewer area for the picture: above the bar, or all of it when the bar is an overlay.
    unsafe fn content_area(&self) -> RECT {
        let mut rc = RECT::default();
        let _ = GetClientRect(self.viewer, &mut rc);
        if self.bar_host.is_none() {
            rc.bottom = self.bar_layout().bar.top;
        }
        RECT { left: 0, top: 0, right: rc.right.max(1), bottom: rc.bottom.max(1) }
    }

    /// After a resize.
    pub unsafe fn layout(&self) {
        if let Content::Video(v) = &self.content {
            v.layout(self.content_area());
        }
    }

    pub unsafe fn invalidate_bar(&self) {
        match self.bar_host {
            Some(host) => {
                let _ = InvalidateRect(Some(host), None, false);
            }
            None => {
                let _ = InvalidateRect(Some(self.viewer), Some(&self.bar_layout().bar), false);
            }
        }
    }

    /// Paints the part of the client area in `dirty` (a video surface is clipped out by the viewer).
    pub unsafe fn paint(&mut self, hdc: HDC, width: i32, height: i32, dirty: RECT) {
        if width <= 0 || height <= 0 {
            return;
        }
        let mem_dc = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, width, height);
        let old = SelectObject(mem_dc, bmp.into());
        // Progress updates repaint only the bar: skip redrawing the cover then.
        IntersectClipRect(mem_dc, dirty.left, dirty.top, dirty.right, dirty.bottom);

        let scale = self.dpi_scale();
        let area = self.content_area();
        match &mut self.content {
            Content::Video(_) => {
                FillRect(mem_dc, &RECT { left: 0, top: 0, right: width, bottom: height }, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            }
            Content::Audio(a) => a.paint(mem_dc, area, scale),
        }

        if self.bar_host.is_none() {
            self.paint_bar(mem_dc, width, height, scale);
        }

        let (dw, dh) = (dirty.right - dirty.left, dirty.bottom - dirty.top);
        let _ = BitBlt(hdc, dirty.left, dirty.top, dw, dh, Some(mem_dc), dirty.left, dirty.top, SRCCOPY);
        SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
    }

    /// The bar along the bottom of a `width` x `height` surface.
    unsafe fn paint_bar(&mut self, dc: HDC, width: i32, height: i32, scale: f32) {
        let bar = transport_bar::bar_state(self.transport(), self.known_duration());
        let error = self.error().map(str::to_owned);
        let status = self.status.clone();
        let dpi_key = (scale * 96.0) as u32;
        if self.font.as_ref().is_none_or(|(k, _)| *k != dpi_key) {
            self.font = Some((dpi_key, dialog::create_font(w!("Segoe UI"), -(13.0 * scale).round() as i32)));
        }
        let font = self.font.as_ref().map(|(_, f)| f.0).unwrap_or_default();
        let layout = transport_bar::layout(width, height, scale, self.skip);
        let message = match (&error, &status) {
            (Some(e), _) => Some(Message::Error(e)),
            (None, Some(s)) => Some(Message::Info(s)),
            (None, None) => None,
        };
        transport_bar::paint(dc, &layout, &bar, font, message, scale);
    }

    /// `WM_PAINT` of the overlay window hosting the bar.
    pub unsafe fn paint_bar_host(&mut self, host: HWND) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(host, &mut ps);
        if hdc.is_invalid() {
            return;
        }
        let mut rc = RECT::default();
        let _ = GetClientRect(host, &mut rc);
        let scale = self.dpi_scale();
        crate::overlay::with_buffer(hdc, rc, |dc| unsafe { self.paint_bar(dc, rc.right, rc.bottom, scale) });
        let _ = EndPaint(host, &ps);
    }

    /// A click in the viewer. With the bar in an overlay, the whole viewer is the picture.
    pub unsafe fn mouse_down(&mut self, x: i32, y: i32) -> Click {
        if self.bar_host.is_some() {
            return Click::Outside;
        }
        self.bar_mouse_down(x, y)
    }

    /// A click in the window showing the bar, in its coordinates.
    pub unsafe fn bar_mouse_down(&mut self, x: i32, y: i32) -> Click {
        let layout = self.bar_layout();
        let duration = self.known_duration();
        let click = {
            let transport = match &self.content {
                Content::Video(v) => v.transport(),
                Content::Audio(a) => a.transport(),
            };
            self.bar.mouse_down(transport, &layout, duration, x, y)
        };
        if click == Click::Bar {
            self.invalidate_bar();
        }
        click
    }

    /// True while a bar slider is being dragged (the caller keeps mouse capture).
    pub fn is_dragging(&self) -> bool {
        self.bar.is_dragging()
    }

    pub unsafe fn mouse_move(&mut self, x: i32) {
        let layout = self.bar_layout();
        let duration = self.known_duration();
        let transport = match &self.content {
            Content::Video(v) => v.transport(),
            Content::Audio(a) => a.transport(),
        };
        if self.bar.mouse_move(transport, &layout, duration, x) {
            self.invalidate_bar();
        }
    }

    pub unsafe fn mouse_up(&mut self, x: i32) {
        let layout = self.bar_layout();
        let duration = self.known_duration();
        let transport = match &self.content {
            Content::Video(v) => v.transport(),
            Content::Audio(a) => a.transport(),
        };
        self.bar.mouse_up(transport, &layout, duration, x);
        self.invalidate_bar();
    }

    /// Video OSD on or off (`style` = `None`); no effect for audio.
    pub unsafe fn set_video_osd(&mut self, style: Option<crate::video_view::OsdStyle>, label: String) {
        if let Content::Video(v) = &mut self.content {
            v.set_osd(style, label);
        }
    }

    /// Shows `text` on the bar for a few seconds.
    pub unsafe fn show_status(&mut self, text: String) {
        self.status = Some(text);
        SetTimer(Some(self.viewer), STATUS_TIMER_ID, STATUS_MS, None);
        self.invalidate_bar();
    }

    /// What Ctrl+C copies: the current video frame, or the album art.
    pub unsafe fn current_picture(&self) -> Option<Arc<DecodedImage>> {
        match &self.content {
            Content::Video(v) => v.capture_frame().map(Arc::new),
            Content::Audio(a) => a.cover(),
        }
    }

    /// Frame size of a video ((0, 0) for audio).
    pub fn video_size(&self) -> (u32, u32) {
        match &self.content {
            Content::Video(v) => (v.info.width, v.info.height),
            Content::Audio(_) => (0, 0),
        }
    }

    pub fn position(&self) -> f64 {
        self.transport().position()
    }

    pub fn toggle_play(&self) {
        self.transport().toggle_play();
    }

    pub fn pause(&self) {
        self.transport().pause();
    }

    /// From the start (repeat one / a one-file loop).
    pub fn replay(&self) {
        let t = self.transport();
        t.seek(0.0, false);
        t.play();
    }

    pub fn seek_by(&self, forward: bool) {
        transport_bar::seek_by(self.transport(), forward);
    }

    pub unsafe fn seek_keyframe(&mut self, forward: bool) {
        match &mut self.content {
            Content::Video(v) => v.seek_keyframe(forward),
            // Audio has no key frames: a finer step instead.
            Content::Audio(a) => {
                let t = a.transport();
                t.seek((t.position() + if forward { 1.0 } else { -1.0 }).max(0.0), false);
            }
        }
    }

    pub fn change_volume(&self, up: bool) {
        transport_bar::change_volume(self.transport(), up);
    }

    pub fn toggle_mute(&self) {
        transport_bar::toggle_mute(self.transport());
    }

    /// `WM_MEDIA_EVENT` from a Media Foundation engine.
    pub unsafe fn on_event(&mut self, event: i32, param1: isize) -> EventEffect {
        match &mut self.content {
            Content::Video(v) => v.on_event(event, param1),
            Content::Audio(a) => a.on_event(event, param1),
        }
    }

    /// `WM_TIMER`; `None` if the timer isn't ours.
    pub unsafe fn on_timer(&mut self, id: usize) -> Option<EventEffect> {
        match (&mut self.content, id) {
            (Content::Video(v), RENDER_TIMER_ID) => {
                v.render();
                Some(EventEffect::None)
            }
            (Content::Audio(a), PROGRESS_TIMER_ID) => Some(a.on_tick()),
            (_, STATUS_TIMER_ID) => {
                let _ = KillTimer(Some(self.viewer), STATUS_TIMER_ID);
                self.status = None;
                Some(EventEffect::RepaintBar)
            }
            _ => None,
        }
    }

    /// Caption details: "[1920x1080, 1:23]" / "[FLAC, 1411 кбит/с, 3:45]".
    pub fn title_info(&self) -> String {
        match &self.content {
            Content::Video(v) => v.title_info(),
            Content::Audio(a) => a.title_info(),
        }
    }

    /// "Artist — Title" for tagged audio.
    pub fn display_title(&self) -> Option<String> {
        match &self.content {
            Content::Audio(a) => a.display_title(),
            Content::Video(_) => None,
        }
    }
}
