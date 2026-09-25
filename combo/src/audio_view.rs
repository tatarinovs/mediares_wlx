//! Audio content: album art (embedded, or cover.jpg / folder.jpg next to the file) and tags,
//! painted into the area above the transport bar.
//!
//! Playback is pure Rust ([`AudioPlayer`]); formats symphonia can't decode (WMA, Opus) fall back to
//! the Media Foundation engine with a hidden surface.

use std::path::Path;
use std::sync::Arc;

use mediares_core::audio_tags::{read_tags, AudioTags};
use mediares_core::probe::MediaType;
use mediares_core::video_frame::probe_audio;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, SelectObject, SetBkMode, SetTextColor, DRAW_TEXT_FORMAT,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, TRANSPARENT,
};
use windows::Win32::Media::MediaFoundation::{
    MF_MEDIA_ENGINE_EVENT_ENDED, MF_MEDIA_ENGINE_EVENT_ERROR, MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA,
};
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

use crate::dialog::{self, Font};
use crate::image_cache::{self, DecodedImage, BACKGROUND_GRAY};
use crate::image_view::draw_fitted;
use crate::media_view::EventEffect;
use crate::playback_audio::AudioPlayer;
use crate::playback_video::VideoPlayer;
use crate::transport_bar::{self, format_time, Transport};
use crate::video_view::{engine_error_text, is_progress_event, Surface};

/// Viewer timer that refreshes the bar and notices the end of the track.
pub const PROGRESS_TIMER_ID: usize = 0x4D53;
const PROGRESS_INTERVAL_MS: u32 = 200;

/// Picture files used as album art when the track has none embedded, in order of preference.
const COVER_STEMS: &[&str] = &["cover", "folder", "front", "albumart", "album"];
const COVER_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp"];

const TITLE_COLOR: u32 = 0x00F0F0F0;
const TEXT_COLOR: u32 = 0x00C8C8C8;
const DIM_COLOR: u32 = 0x00909090;
const PLACEHOLDER: u32 = 0x00282828;

enum Backend {
    Native(AudioPlayer),
    /// Field order matters: the engine shuts down before its surface is destroyed.
    Engine { player: VideoPlayer, _surface: Surface, duration: f64 },
}

impl Backend {
    /// The pure-Rust player first, Media Foundation for what it can't decode.
    unsafe fn create(viewer: HWND, path: &Path) -> Option<Self> {
        if let Some(mut player) = AudioPlayer::new() {
            transport_bar::restore_audio_level(&player);
            if player.open(path) {
                return Some(Backend::Native(player));
            }
        }
        let duration = probe_audio(path)?;
        let surface = Surface::new(viewer, false)?;
        let player = VideoPlayer::new(surface.0, viewer).ok()?;
        transport_bar::restore_audio_level(&player);
        player.open(path).ok()?;
        Some(Backend::Engine { player, _surface: surface, duration })
    }

    fn transport(&self) -> &dyn Transport {
        match self {
            Backend::Native(p) => p,
            Backend::Engine { player, .. } => player,
        }
    }
}

struct Fonts {
    dpi_key: i32,
    title: Font,
    text: Font,
    small: Font,
    note: Font,
}

pub struct AudioView {
    backend: Backend,
    viewer: HWND,
    pub tags: AudioTags,
    file_stem: String,
    /// "MP3", "FLAC"...
    format: String,
    cover: Option<Arc<DecodedImage>>,
    error: Option<String>,
    end_reported: bool,
    fonts: Option<Fonts>,
}

impl AudioView {
    /// Starts playing `path`; `None` if neither decoder can play it.
    pub unsafe fn new(viewer: HWND, path: &Path) -> Option<Self> {
        let backend = Backend::create(viewer, path)?;
        SetTimer(Some(viewer), PROGRESS_TIMER_ID, PROGRESS_INTERVAL_MS, None);
        let mut view = Self {
            backend,
            viewer,
            tags: AudioTags::default(),
            file_stem: String::new(),
            format: String::new(),
            cover: None,
            error: None,
            end_reported: false,
            fonts: None,
        };
        view.load_meta(path);
        Some(view)
    }

    /// Switches to another file, reusing the output device when possible.
    pub unsafe fn open(&mut self, path: &Path) -> bool {
        let reused = match &mut self.backend {
            Backend::Native(player) => player.open(path),
            Backend::Engine { .. } => false,
        };
        if !reused {
            match Backend::create(self.viewer, path) {
                Some(backend) => self.backend = backend,
                None => return false,
            }
        }
        self.load_meta(path);
        true
    }

    fn load_meta(&mut self, path: &Path) {
        self.tags = read_tags(path, true).unwrap_or_default();
        let embedded = self.tags.cover.take().and_then(|bytes| image_cache::decode_picture(&bytes)).map(Arc::new);
        self.cover = embedded.or_else(|| folder_cover(path));
        self.file_stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.format = path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
        self.error = None;
        self.end_reported = false;
    }

    pub fn transport(&self) -> &dyn Transport {
        self.backend.transport()
    }

    /// Duration from the tags (or Media Foundation's probe), while the player doesn't know it.
    pub fn known_duration(&self) -> f64 {
        match &self.backend {
            Backend::Engine { duration, .. } => duration.max(self.tags.duration_sec),
            Backend::Native(_) => self.tags.duration_sec,
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// "FLAC, 1411 кбит/с, 3:45"
    pub fn title_info(&self) -> String {
        let mut parts = vec![self.format.clone()];
        if let Some(kbps) = self.tags.bitrate_kbps {
            parts.push(format!("{} кбит/с", kbps));
        }
        parts.push(format_time(self.transport().duration().max(self.known_duration())));
        parts.join(", ")
    }

    /// "Artist — Title" for the window caption, if tagged.
    pub fn display_title(&self) -> Option<String> {
        self.tags.display_title()
    }

    /// `WM_TIMER` with [`PROGRESS_TIMER_ID`].
    pub fn on_tick(&mut self) -> EventEffect {
        let Backend::Native(player) = &self.backend else { return EventEffect::None };
        let ended = player.is_ended();
        if !ended {
            self.end_reported = false;
        } else if !self.end_reported {
            self.end_reported = true;
            return EventEffect::Ended;
        }
        if player.is_playing() { EventEffect::RepaintBar } else { EventEffect::None }
    }

    /// Media Foundation events (engine backend only).
    pub fn on_event(&mut self, event: i32, param1: isize) -> EventEffect {
        let Backend::Engine { player, .. } = &self.backend else { return EventEffect::None };
        match event {
            e if e == MF_MEDIA_ENGINE_EVENT_ENDED.0 => EventEffect::Ended,
            e if e == MF_MEDIA_ENGINE_EVENT_ERROR.0 => {
                let code = unsafe { player.error_code() }.unwrap_or(param1 as u16);
                self.error = Some(engine_error_text(code));
                EventEffect::RepaintBar
            }
            // The caption shows the duration.
            e if e == MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA.0 => EventEffect::Relayout,
            e if is_progress_event(e) => EventEffect::RepaintBar,
            _ => EventEffect::None,
        }
    }

    fn fonts(&mut self, scale: f32) -> &Fonts {
        let key = (scale * 100.0).round() as i32;
        if self.fonts.as_ref().is_none_or(|f| f.dpi_key != key) {
            let px = |pt: f32| -(pt * scale).round() as i32;
            self.fonts = Some(unsafe {
                Fonts {
                    dpi_key: key,
                    title: dialog::create_font(w!("Segoe UI Semibold"), px(24.0)),
                    text: dialog::create_font(w!("Segoe UI"), px(17.0)),
                    small: dialog::create_font(w!("Segoe UI"), px(13.0)),
                    note: dialog::create_font(w!("Segoe UI Symbol"), px(96.0)),
                }
            });
        }
        self.fonts.as_ref().expect("just created")
    }

    /// Paints art and tags into `area`: side by side in a wide window, stacked in a tall one.
    pub unsafe fn paint(&mut self, dc: HDC, area: RECT, scale: f32) {
        let bg = BACKGROUND_GRAY as u32;
        fill(dc, area, bg | bg << 8 | bg << 16);
        let s = |v: f32| (v * scale).round() as i32;
        let m = s(24.0);
        let inner = RECT { left: area.left + m, top: area.top + m, right: area.right - m, bottom: area.bottom - m };
        let (w, h) = (inner.right - inner.left, inner.bottom - inner.top);
        if w < s(40.0) || h < s(20.0) {
            return;
        }

        let lines = self.text_lines();
        let cover = self.cover.clone();
        let fonts = self.fonts(scale);
        let (title_font, text_font, small_font, note_font) = (fonts.title.0, fonts.text.0, fonts.small.0, fonts.note.0);
        let line_h = |kind: LineKind| match kind {
            LineKind::Title => s(34.0),
            LineKind::Text => s(26.0),
            LineKind::Small => s(22.0),
        };
        let text_h: i32 = lines.iter().map(|(_, k)| line_h(*k)).sum();
        let gap = s(24.0);

        let wide = w as f32 > h as f32 * 1.4;
        let (art, text_rect, align) = if wide {
            let side = h.min(w * 2 / 5);
            let art = RECT { left: inner.left, top: inner.top + (h - side) / 2, right: inner.left + side, bottom: inner.top + (h - side) / 2 + side };
            let top = inner.top + (h - text_h).max(0) / 2;
            (art, RECT { left: art.right + gap, top, right: inner.right, bottom: top + text_h }, DT_LEFT)
        } else {
            let side = w.min(h - text_h - gap).max(0);
            let side = if side < s(64.0) { 0 } else { side };
            let block = side + if side > 0 { gap } else { 0 } + text_h;
            let top = inner.top + (h - block).max(0) / 2;
            let left = inner.left + (w - side) / 2;
            let art = RECT { left, top, right: left + side, bottom: top + side };
            let text_top = art.bottom + if side > 0 { gap } else { 0 };
            (art, RECT { left: inner.left, top: text_top, right: inner.right, bottom: text_top + text_h }, DT_CENTER)
        };

        if art.right > art.left {
            match &cover {
                Some(img) => draw_fitted(dc, img, art),
                None => {
                    fill(dc, art, PLACEHOLDER);
                    text(dc, art, "♫", note_font, DIM_COLOR, DT_CENTER);
                }
            }
        }

        let mut y = text_rect.top;
        for (line, kind) in &lines {
            let (font, color) = match kind {
                LineKind::Title => (title_font, TITLE_COLOR),
                LineKind::Text => (text_font, TEXT_COLOR),
                LineKind::Small => (small_font, DIM_COLOR),
            };
            let r = RECT { left: text_rect.left, top: y, right: text_rect.right, bottom: y + line_h(*kind) };
            text(dc, r, line, font, color, align);
            y = r.bottom;
        }
    }

    fn text_lines(&self) -> Vec<(String, LineKind)> {
        let t = &self.tags;
        let mut lines = vec![(t.title.clone().unwrap_or_else(|| self.file_stem.clone()), LineKind::Title)];
        if let Some(artist) = t.any_artist() {
            lines.push((artist.to_string(), LineKind::Text));
        }
        let album = match (&t.album, t.year) {
            (Some(a), Some(y)) => Some(format!("{} ({})", a, y)),
            (Some(a), None) => Some(a.clone()),
            (None, Some(y)) => Some(y.to_string()),
            (None, None) => None,
        };
        if let Some(album) = album {
            lines.push((album, LineKind::Text));
        }
        let format = [self.format.clone(), t.format_line()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
        lines.push((format, LineKind::Small));
        lines
    }
}

impl Drop for AudioView {
    fn drop(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.viewer), PROGRESS_TIMER_ID);
        }
    }
}

#[derive(Clone, Copy)]
enum LineKind {
    Title,
    Text,
    Small,
}

/// cover.jpg, folder.jpg... in the track's folder.
fn folder_cover(track: &Path) -> Option<Arc<DecodedImage>> {
    let entries = std::fs::read_dir(track.parent()?).ok()?;
    let rank = |p: &Path| -> Option<usize> {
        let stem = p.file_stem()?.to_str()?.to_ascii_lowercase();
        let ext = p.extension()?.to_str()?.to_ascii_lowercase();
        if !COVER_EXTS.contains(&ext.as_str()) {
            return None;
        }
        COVER_STEMS.iter().position(|s| *s == stem)
    };
    let best = entries.flatten().map(|e| e.path()).filter_map(|p| Some((rank(&p)?, p))).min_by_key(|(r, _)| *r)?;
    image_cache::load(&best.1, MediaType::StandardImage, false)
}

unsafe fn fill(dc: HDC, r: RECT, color: u32) {
    let brush = CreateSolidBrush(COLORREF(color));
    FillRect(dc, &r, brush);
    let _ = DeleteObject(brush.into());
}

unsafe fn text(dc: HDC, r: RECT, s: &str, font: HFONT, color: u32, align: DRAW_TEXT_FORMAT) {
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    let mut rc = r;
    let old = SelectObject(dc, font.into());
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, COLORREF(color));
    DrawTextW(dc, &mut wide, &mut rc, align | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
    SelectObject(dc, old);
}
