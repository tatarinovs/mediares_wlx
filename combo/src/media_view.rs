//! A playable file in the viewer — video or audio — with the transport bar along the bottom.
//!
//! The content ([`VideoView`] / [`AudioView`]) owns the area above the bar and the player; this
//! module owns the bar: layout, painting, mouse input, and the commands the window forwards.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use mediares_core::probe::MediaType;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, HDC, PAINTSTRUCT};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, KillTimer, SetTimer};

use crate::audio_view::{AudioView, PROGRESS_TIMER_ID};
use crate::gdi::{self, Font};
use crate::i18n::{self, tr};
use crate::image_cache::DecodedImage;
use crate::seek_preview::SeekPreview;
use crate::transport_bar::{self, BarControl, Click, Hit, Layout, Marks, Message, Transport};
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
    /// Near the end: the next file of the queue can be decoded ahead.
    PreloadNext,
}

pub enum Content {
    Video(Box<VideoView>),
    Audio(Box<AudioView>),
}

impl Content {
    fn transport(&self) -> &dyn Transport {
        match self {
            Content::Video(v) => v.transport(),
            Content::Audio(a) => a.transport(),
        }
    }
}

pub struct MediaView {
    pub content: Content,
    viewer: HWND,
    bar: BarControl,
    /// Show previous / next buttons (there are other playable files around).
    skip: bool,
    /// Bar font and the DPI it was made for.
    font: Option<(u32, Font)>,
    /// Short-lived message in the timeline instead of the name ("frame saved...").
    status: Option<String>,
    /// File name, written in the timeline when the audio has no tags to name it.
    file_name: String,
    /// Fullscreen: the bar lives in this overlay window and the content takes the whole viewer.
    bar_host: Option<HWND>,
    /// Last arrow-key step (key auto-repeat is thinned out).
    last_seek_step: Option<Instant>,
    /// The frame under the cursor on the timeline (video only), made on first hover.
    seek_preview: Option<SeekPreview>,
    /// Whether to show it at all (a setting).
    seek_preview_enabled: bool,
    /// A-B loop: its start once set, then its end (playback jumps back to the start there).
    ab_loop: (Option<f64>, Option<f64>),
}

/// RealMedia, Ogg and the like are often sound only: those play in the audio view (tags, cover,
/// no black picture).
fn playable_kind(path: &Path, kind: MediaType) -> MediaType {
    if kind != MediaType::Video || !mediares_core::probe::is_mpv_only(path) {
        return kind;
    }
    match mediares_core::cache::get_video_meta(path) {
        Some(meta) if meta.codec.is_none() && meta.width == 0 && meta.audio_codec.is_some() => {
            MediaType::Audio
        }
        _ => kind,
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The viewer configuration a file open needs, bundled so `open` stays under the lint's limit.
pub struct OpenOptions {
    pub resume: bool,
    /// Videos shorter than this (seconds) never offer to resume.
    pub resume_threshold: f64,
    pub replay_gain: bool,
    pub seek_preview: bool,
}

impl MediaView {
    /// Opens `path` (`kind` must be playable), reusing `previous` when it shows the same kind of
    /// content. `None` if the file can't be played.
    pub unsafe fn open(
        viewer: HWND,
        previous: Option<MediaView>,
        path: &Path,
        kind: MediaType,
        options: OpenOptions,
    ) -> Option<MediaView> {
        let kind = playable_kind(path, kind);
        if let Some(mut view) = previous {
            let reused = match &mut view.content {
                Content::Video(v) if kind == MediaType::Video => {
                    v.open(path, options.resume, options.resume_threshold)
                }
                Content::Audio(a) if kind == MediaType::Audio => a.open(path, options.replay_gain),
                _ => false,
            };
            if reused {
                view.bar.cancel();
                view.seek_preview = None;
                view.seek_preview_enabled = options.seek_preview;
                view.ab_loop = (None, None);
                view.file_name = file_name(path);
                view.layout();
                return Some(view);
            }
            // Stop the old player before starting the new one.
            drop(view);
        }
        let content = match kind {
            MediaType::Video => Content::Video(Box::new(VideoView::new(
                viewer,
                path,
                options.resume,
                options.resume_threshold,
            )?)),
            MediaType::Audio => {
                Content::Audio(Box::new(AudioView::new(viewer, path, options.replay_gain)?))
            }
            _ => return None,
        };
        let view = MediaView {
            content,
            viewer,
            bar: BarControl::default(),
            skip: false,
            font: None,
            status: None,
            file_name: file_name(path),
            bar_host: None,
            last_seek_step: None,
            seek_preview: None,
            seek_preview_enabled: options.seek_preview,
            ab_loop: (None, None),
        };
        view.layout();
        Some(view)
    }

    pub fn transport(&self) -> &dyn Transport {
        self.content.transport()
    }

    /// Settings changed while the file is shown: they apply to it right away.
    pub fn apply_options(&mut self, options: &OpenOptions) {
        self.seek_preview_enabled = options.seek_preview;
        if !options.seek_preview {
            // Its popup and frame thread go too.
            self.seek_preview = None;
        }
        match &mut self.content {
            Content::Video(v) => v.set_resume(options.resume, options.resume_threshold),
            Content::Audio(a) => a.set_replay_gain(options.replay_gain),
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
        gdi::dpi_scale(self.viewer)
    }

    /// Moves the bar into an overlay window (`Some`) or back to the bottom of the viewer.
    pub unsafe fn set_bar_host(&mut self, host: Option<HWND>) {
        if self.bar_host != host {
            self.bar_host = host;
            self.bar.cancel();
            // A drag cut short has no final seek.
            if let Content::Video(v) = &self.content {
                v.settle_preview();
            }
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
        RECT {
            left: 0,
            top: 0,
            right: rc.right.max(1),
            bottom: rc.bottom.max(1),
        }
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

    /// Paints the part of the client area in `dirty` (a video surface is clipped out by the viewer);
    /// `smooth`: enlarge album art with bicubic filtering.
    pub unsafe fn paint(&mut self, hdc: HDC, width: i32, height: i32, dirty: RECT, smooth: bool) {
        let scale = self.dpi_scale();
        let area = self.content_area();
        // Progress updates repaint only the bar: the cover isn't redrawn then (clipped out).
        gdi::with_buffer(hdc, width, height, dirty, |dc| unsafe {
            match &mut self.content {
                Content::Video(_) => gdi::fill(dc, gdi::rect(width, height), 0),
                Content::Audio(a) => a.paint(dc, area, scale, smooth),
            }
            if self.bar_host.is_none() {
                self.paint_bar(dc, width, height, scale);
            }
        });
    }

    /// The bar along the bottom of a `width` x `height` surface.
    unsafe fn paint_bar(&mut self, dc: HDC, width: i32, height: i32, scale: f32) {
        let bar = transport_bar::bar_state(self.transport(), self.known_duration());
        let error = self.error().map(str::to_owned);
        let status = self.status.clone();
        let dpi_key = (scale * 96.0) as u32;
        if self.font.as_ref().is_none_or(|(k, _)| *k != dpi_key) {
            self.font = Some((
                dpi_key,
                gdi::create_font("Segoe UI", -(13.0 * scale).round() as i32, false),
            ));
        }
        let font = self.font.as_ref().map(|(_, f)| f.0).unwrap_or_default();
        let layout = transport_bar::layout(width, height, scale, self.skip);
        let name = self.display_title();
        let message = match (&error, &status) {
            (Some(e), _) => Message::Error(e),
            (None, Some(s)) => Message::Info(s),
            (None, None) => Message::Info(name.as_deref().unwrap_or(&self.file_name)),
        };
        let marks = Marks {
            chapters: match &self.content {
                Content::Video(v) => v.chapters().iter().map(|(t, _)| *t).collect(),
                Content::Audio(_) => Vec::new(),
            },
            loop_a: self.ab_loop.0,
            loop_b: self.ab_loop.1,
        };
        transport_bar::paint(dc, &layout, &bar, &marks, font, message, scale);
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
        gdi::with_buffer(hdc, rc.right, rc.bottom, rc, |dc| unsafe {
            self.paint_bar(dc, rc.right, rc.bottom, scale)
        });
        let _ = EndPaint(host, &ps);
    }

    /// The point (viewer client coordinates) is on the bar along the bottom of the viewer.
    pub fn over_bar(&self, x: i32, y: i32) -> bool {
        self.bar_host.is_none() && gdi::contains(&unsafe { self.bar_layout() }.bar, x, y)
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
        if let Content::Video(v) = &mut self.content {
            v.cancel_key_seek();
        }
        let layout = self.bar_layout();
        let duration = self.known_duration();
        let click = self
            .bar
            .mouse_down(self.content.transport(), &layout, duration, x, y);
        if click == Click::Bar {
            self.invalidate_bar();
        }
        click
    }

    /// True while a bar slider is being dragged (the caller keeps mouse capture).
    pub fn is_dragging(&self) -> bool {
        self.bar.is_dragging()
    }

    /// The cursor moved over the viewer (its client coordinates).
    pub unsafe fn mouse_move(&mut self, x: i32, y: i32) {
        self.drag_to(x);
        if self.bar_host.is_none() {
            self.update_preview(self.viewer, x, y);
        }
    }

    /// The cursor moved over the overlay window hosting the bar (its client coordinates).
    pub unsafe fn bar_host_mouse_move(&mut self, x: i32, y: i32) {
        self.drag_to(x);
        if let Some(host) = self.bar_host {
            self.update_preview(host, x, y);
        }
    }

    unsafe fn drag_to(&mut self, x: i32) {
        let layout = self.bar_layout();
        let duration = self.known_duration();
        if self
            .bar
            .mouse_move(self.content.transport(), &layout, duration, x)
        {
            self.invalidate_bar();
        }
    }

    /// Shows the frame under the cursor while it is over the timeline (or drags it), else hides
    /// the preview. `window` shows the bar; `x`, `y` are in its client coordinates.
    unsafe fn update_preview(&mut self, window: HWND, x: i32, y: i32) {
        let Content::Video(video) = &self.content else {
            return;
        };
        if !self.seek_preview_enabled {
            return;
        }
        let layout = self.bar_layout();
        let fraction = if self.bar.is_dragging_timeline() {
            Some(transport_bar::timeline_fraction(&layout, x))
        } else {
            match transport_bar::hit(&layout, x, y) {
                Hit::Timeline(f) => Some(f),
                _ => None,
            }
        };
        let duration = self
            .content
            .transport()
            .duration()
            .max(self.known_duration());
        let Some(fraction) = fraction.filter(|_| duration > 0.0) else {
            self.hide_preview();
            return;
        };
        if self
            .seek_preview
            .as_ref()
            .is_none_or(|p| p.path() != video.path())
        {
            self.seek_preview = SeekPreview::new(self.viewer, video.path());
        }
        if let Some(preview) = &self.seek_preview {
            let seconds = fraction * duration;
            preview.show(
                window,
                x,
                layout.bar.top,
                seconds,
                video.chapter_at(seconds),
            );
            // Hidden again when the cursor leaves the window (WM_MOUSELEAVE).
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: window,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut track);
        }
    }

    pub unsafe fn hide_preview(&self) {
        if let Some(preview) = &self.seek_preview {
            preview.hide();
        }
    }

    /// `WM_SEEK_PREVIEW`: a frame for the timeline preview arrived.
    pub unsafe fn preview_ready(&self) {
        if let Some(preview) = &self.seek_preview {
            preview.frame_ready();
        }
    }

    pub unsafe fn mouse_up(&mut self, x: i32) {
        let layout = self.bar_layout();
        let duration = self.known_duration();
        self.bar
            .mouse_up(self.content.transport(), &layout, duration, x);
        self.invalidate_bar();
    }

    /// Decodes the next audio file ahead for gapless playback; false if it can't.
    pub fn preload(&mut self, path: &Path) -> bool {
        match &mut self.content {
            Content::Audio(a) => a.preload(path),
            Content::Video(_) => false,
        }
    }

    /// The audio shown, if it is that.
    pub fn audio_mut(&mut self) -> Option<&mut AudioView> {
        match &mut self.content {
            Content::Audio(a) => Some(a),
            Content::Video(_) => None,
        }
    }

    /// The audio shown, if it is that.
    pub fn audio(&self) -> Option<&AudioView> {
        match &self.content {
            Content::Audio(a) => Some(a),
            Content::Video(_) => None,
        }
    }

    /// The video shown, if it is one.
    pub fn video_mut(&mut self) -> Option<&mut VideoView> {
        match &mut self.content {
            Content::Video(v) => Some(v),
            Content::Audio(_) => None,
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

    /// ±`step` s (Left / Right).
    pub unsafe fn seek_by(&mut self, forward: bool, step: f64) {
        let now = Instant::now();
        if self
            .last_seek_step
            .is_some_and(|t| now - t < transport_bar::SEEK_REPEAT_INTERVAL)
        {
            return;
        }
        self.last_seek_step = Some(now);
        match &mut self.content {
            Content::Video(v) => v.seek_by(forward, step),
            Content::Audio(a) => transport_bar::seek_by(a.transport(), forward, step),
        }
    }

    pub unsafe fn seek_keyframe(&mut self, forward: bool) {
        match &mut self.content {
            Content::Video(v) => v.seek_keyframe(forward),
            // Audio has no key frames: a finer step instead.
            Content::Audio(a) => {
                transport_bar::seek_by(a.transport(), forward, transport_bar::FINE_STEP_SEC)
            }
        }
    }

    /// Video speed one step slower / faster, or normal (`None`); the bar shows the new speed.
    pub unsafe fn change_rate(&mut self, faster: Option<bool>) {
        let Content::Video(v) = &mut self.content else {
            return;
        };
        let rate = v.change_rate(faster);
        self.show_status(format!(
            "{} {}×",
            tr("Speed"),
            i18n::decimal(rate.to_string())
        ));
    }

    pub unsafe fn frame_step(&mut self, forward: bool) {
        if let Content::Video(v) = &mut self.content {
            if !v.frame_step(forward) {
                self.show_status(
                    tr("Stepping back is not available for this file (inexact seeking)")
                        .to_string(),
                );
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
            Content::Video(v) => {
                let effect = v.on_event(event, param1);
                if let Some(t) = v.take_resumed() {
                    self.show_status(format!(
                        "{} {}",
                        tr("Resuming from"),
                        transport_bar::format_time(t)
                    ));
                }
                effect
            }
            Content::Audio(a) => a.on_event(event, param1),
        }
    }

    /// A: sets the loop start, then its end; a third press ends the loop.
    pub unsafe fn toggle_ab_loop(&mut self) {
        let at = self.position();
        let text = match self.ab_loop {
            (None, _) => {
                self.ab_loop = (Some(at), None);
                format!("{} A: {}", tr("Loop"), transport_bar::format_time(at))
            }
            (Some(a), None) if at > a => {
                self.ab_loop = (Some(a), Some(at));
                format!(
                    "{} A-B: {} - {}",
                    tr("Loop"),
                    transport_bar::format_time(a),
                    transport_bar::format_time(at)
                )
            }
            _ => {
                self.ab_loop = (None, None);
                tr("Loop off").to_string()
            }
        };
        self.show_status(text);
    }

    /// Back to the loop start once its end is reached.
    fn keep_in_loop(&self) {
        if let (Some(a), Some(b)) = self.ab_loop {
            let t = self.transport();
            if t.position() >= b && t.is_playing() {
                t.seek(a, false);
            }
        }
    }

    /// B: the next audio track of a video, announced on the bar.
    pub unsafe fn cycle_audio_track(&mut self) {
        let Content::Video(v) = &self.content else {
            return;
        };
        let text = v
            .cycle_audio_track()
            .unwrap_or_else(|| tr("This file has one audio track").to_string());
        self.show_status(text);
    }

    /// `WM_TIMER`; `None` if the timer isn't ours.
    pub unsafe fn on_timer(&mut self, id: usize) -> Option<EventEffect> {
        if matches!(id, RENDER_TIMER_ID | PROGRESS_TIMER_ID) {
            self.keep_in_loop();
        }
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
