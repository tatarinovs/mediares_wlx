//! Video content: a child surface the media engine renders into, filling the area above
//! the transport bar (the bar itself belongs to [`crate::media_view`]).
//!
//! The surface is transparent to mouse input (`HTTRANSPARENT`), so the viewer window receives
//! every click and decides between the video area and the bar.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};

use mediares_core::cache::{get_video_meta, get_video_tags};
use mediares_core::video_frame::{KeyframeIndex, VideoMeta};
use mediares_core::video_tags::VideoTags;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
use windows::Win32::Media::MediaFoundation::{
    MF_MEDIA_ENGINE_EVENT_DURATIONCHANGE, MF_MEDIA_ENGINE_EVENT_ENDED, MF_MEDIA_ENGINE_EVENT_ERROR,
    MF_MEDIA_ENGINE_EVENT_FORMATCHANGE, MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA,
    MF_MEDIA_ENGINE_EVENT_PAUSE, MF_MEDIA_ENGINE_EVENT_PLAY, MF_MEDIA_ENGINE_EVENT_PLAYING,
    MF_MEDIA_ENGINE_EVENT_SEEKED, MF_MEDIA_ENGINE_EVENT_TIMEUPDATE,
    MF_MEDIA_ENGINE_EVENT_VOLUMECHANGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, IsWindow, KillTimer, MoveWindow, SetTimer,
    HTTRANSPARENT, WINDOW_EX_STYLE, WINDOW_STYLE, WM_NCHITTEST, WS_CHILD, WS_CLIPSIBLINGS,
    WS_VISIBLE,
};

use crate::i18n::tr;
use crate::media_view::EventEffect;
use crate::module;
use crate::playback_video::{Osd, VideoPlayer, FALLBACK_FRAME_SEC};
use crate::resume;
use crate::transport_bar::{self, format_time, Transport};

const SURFACE_CLASS: PCWSTR = w!("MediaresVideoSurface");
/// Viewer timer that pumps frames from the engine to the swap chain.
pub const RENDER_TIMER_ID: usize = 0x4D52;
/// USER_TIMER_MINIMUM; the effective period is the system tick (~15 ms, 60+ fps).
const RENDER_INTERVAL_MS: u32 = 10;
/// "Previous key frame" looks this far behind the current position, so repeated presses keep
/// stepping back while the video plays on from the frame it just landed on.
const KEYFRAME_BACK_SLACK_SEC: f64 = 0.3;

/// Child window the engine presents into; destroyed on drop.
pub struct Surface(pub HWND);

impl Surface {
    /// A hidden surface serves the engine when it only plays audio.
    pub unsafe fn new(viewer: HWND, visible: bool) -> Option<Self> {
        register_surface_class();
        let style = WS_CHILD | WS_CLIPSIBLINGS | if visible { WS_VISIBLE } else { WINDOW_STYLE(0) };
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            SURFACE_CLASS,
            None,
            style,
            0,
            0,
            1,
            1,
            Some(viewer),
            None,
            Some(module()),
            None,
        )
        .ok()
        .map(Self)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(Some(self.0)).as_bool() {
                let _ = DestroyWindow(self.0);
            }
        }
    }
}

unsafe extern "system" fn surface_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn register_surface_class() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
        crate::gdi::register_class(SURFACE_CLASS, Some(surface_proc), Default::default(), black);
    });
}

/// Field order matters: the player (engine) shuts down before its surface window is destroyed.
pub struct VideoView {
    player: VideoPlayer,
    surface: Surface,
    viewer: HWND,
    path: PathBuf,
    /// Opened on first key-frame step; reset when the file changes.
    keyframes: Option<KeyframeIndex>,
    /// Stream properties; the size is updated once the engine knows it.
    pub info: VideoMeta,
    error: Option<String>,
    osd: Option<VideoOsd>,
    rate: f64,
    /// Remember where long videos are left and continue from there.
    resume: bool,
    /// Position to jump to once the file is loaded.
    resume_at: Option<f64>,
    /// Just continued from here (the bar says so).
    resumed_to: Option<f64>,
    /// Metadata arrived: the position is meaningful and may be remembered.
    loaded: bool,
    /// A forward frame step in progress.
    stepping: Option<Step>,
    /// Seeks land on the requested frame. MPEG program streams (and files found out at run time)
    /// snap to the next group of pictures, so stepping back is not offered for them.
    precise_seek: bool,
    /// A step back was requested to here; checked when the seek completes.
    back_target: Option<f64>,
}

/// Forward frame step: the video plays slowly and muted until the next frame is on screen.
struct Step {
    from_pts: Option<i64>,
    /// Frames still to go (the key can be pressed again, or held, before a step finishes).
    remaining: u32,
    muted: bool,
}

/// Playback speeds `[` / `]` step through.
const RATES: &[f64] = &[0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
/// Speed while stepping: frames arrive slower than the render tick, so it stops on the next one.
const STEP_RATE: f64 = 0.25;

/// Containers whose Media Foundation source can't seek to an exact frame.
fn seeks_precisely(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    !matches!(ext.as_str(), "mpg" | "mpeg" | "vob")
}

/// Font and colour of the OSD (the viewer config's OSD settings).
#[derive(Clone, PartialEq)]
pub struct OsdStyle {
    pub face: String,
    pub size_pt: i32,
    pub color: u32,
}

struct VideoOsd {
    style: OsdStyle,
    font: crate::gdi::Font,
    text: OsdText,
}

/// What the OSD shows: a template and the values of its fields, except the playback position
/// and duration, which are filled per frame.
pub struct OsdText {
    pub template: String,
    pub fields: HashMap<&'static str, String>,
}

impl VideoView {
    /// Creates the surface inside `viewer` and starts playing `path`. `None` if Media Foundation
    /// cannot open the file (so TC can fall back to another plugin).
    pub unsafe fn new(viewer: HWND, path: &Path, resume: bool) -> Option<Self> {
        let info = (*get_video_meta(path)?).clone();
        let surface = Surface::new(viewer, true)?;
        let player = VideoPlayer::new(surface.0, viewer).ok()?;
        transport_bar::restore_audio_level(&player);
        player.open(path).ok()?;

        SetTimer(Some(viewer), RENDER_TIMER_ID, RENDER_INTERVAL_MS, None);
        let resume_at = resume_point(path, &info, resume);
        Some(Self {
            player,
            surface,
            viewer,
            path: path.to_path_buf(),
            keyframes: None,
            info,
            error: None,
            osd: None,
            rate: 1.0,
            resume,
            resume_at,
            resumed_to: None,
            loaded: false,
            stepping: None,
            precise_seek: seeks_precisely(path),
            back_target: None,
        })
    }

    /// Switches to another file, reusing the engine (and its speed).
    pub unsafe fn open(&mut self, path: &Path, resume: bool) -> bool {
        let Some(info) = get_video_meta(path) else {
            return false;
        };
        let info = (*info).clone();
        self.remember_position();
        self.finish_step();
        if self.player.open(path).is_err() {
            return false;
        }
        self.precise_seek = seeks_precisely(path);
        self.back_target = None;
        self.resume = resume;
        self.resume_at = resume_point(path, &info, resume);
        self.resumed_to = None;
        self.loaded = false;
        self.info = info;
        self.path = path.to_path_buf();
        self.keyframes = None;
        self.error = None;
        true
    }

    fn remember_position(&self) {
        if self.resume && self.loaded {
            let duration = self.player.duration().max(self.info.duration_sec);
            resume::store(&self.path, self.player.position(), duration);
        }
    }

    /// Where playback continued from, once (for a status message).
    pub fn take_resumed(&mut self) -> Option<f64> {
        self.resumed_to.take()
    }

    /// One step slower / faster through [`RATES`], or back to normal (`None`). Returns the speed.
    pub unsafe fn change_rate(&mut self, faster: Option<bool>) -> f64 {
        self.finish_step();
        let at = RATES
            .iter()
            .position(|&r| r >= self.rate)
            .unwrap_or(RATES.len() - 1);
        self.rate = match faster {
            None => 1.0,
            Some(true) => RATES[(at + 1).min(RATES.len() - 1)],
            Some(false) => RATES[at.saturating_sub(1)],
        };
        self.player.set_rate(self.rate);
        self.rate
    }

    /// One frame forward / back, ending paused. False if this file can't step back.
    pub unsafe fn frame_step(&mut self, forward: bool) -> bool {
        if !forward {
            if !self.precise_seek {
                return false;
            }
            self.finish_step();
            self.back_target = Some(self.player.step_back(self.info.frame_rate));
            return true;
        }
        match &mut self.stepping {
            Some(step) => step.remaining += 1,
            None => {
                let muted = self.player.is_muted();
                self.player.set_muted(true);
                self.player.set_current_rate(STEP_RATE);
                self.stepping = Some(Step {
                    from_pts: self.player.presented_pts(),
                    remaining: 1,
                    muted,
                });
                self.player.play();
            }
        }
        true
    }

    /// Render tick of a forward step: pause once enough new frames were shown.
    unsafe fn advance_step(&mut self) {
        let Some(step) = &mut self.stepping else {
            return;
        };
        if self.player.is_paused() {
            // Paused by the user (or the end reached) mid-step.
            self.finish_step();
            return;
        }
        let pts = self.player.presented_pts();
        if pts != step.from_pts {
            step.from_pts = pts;
            step.remaining -= 1;
            if step.remaining == 0 {
                self.player.pause();
                self.finish_step();
            }
        }
    }

    /// Restores the speed and sound a forward step changed.
    unsafe fn finish_step(&mut self) {
        if let Some(step) = self.stepping.take() {
            self.player.set_current_rate(self.rate);
            self.player.set_muted(step.muted);
        }
    }

    pub fn transport(&self) -> &dyn Transport {
        &self.player
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// `WM_TIMER` with [`RENDER_TIMER_ID`].
    pub unsafe fn render(&mut self) {
        match &self.osd {
            Some(osd) => {
                let position = format_time(self.player.position());
                let duration = format_time(self.player.duration().max(self.info.duration_sec));
                let text = crate::osd_template::render(&osd.text.template, |key| match key {
                    "time" => Some(position.clone()),
                    "duration" => Some(duration.clone()),
                    _ => osd.text.fields.get(key).cloned(),
                });
                self.player.render(Some(Osd {
                    text: &text,
                    font: osd.font.0,
                    color: osd.style.color,
                }));
            }
            None => self.player.render(None),
        }
        self.advance_step();
    }

    /// Tags of the current file (read once, then cached).
    pub fn tags(&self) -> Option<Arc<VideoTags>> {
        get_video_tags(&self.path)
    }

    /// Shows (`Some`) or hides the OSD.
    pub unsafe fn set_osd(&mut self, style: Option<OsdStyle>, text: OsdText) {
        self.osd = match style {
            None => None,
            Some(style) => match self.osd.take() {
                Some(osd) if osd.style == style => Some(VideoOsd { text, ..osd }),
                _ => {
                    let dpi = (crate::gdi::dpi_scale(self.viewer) * 96.0).round() as i32;
                    let font = crate::image_view::create_osd_font(&style.face, style.size_pt, dpi);
                    Some(VideoOsd { style, font, text })
                }
            },
        };
    }

    /// Stretches the surface over `area` (viewer client coordinates). The engine letterboxes the
    /// frame inside it, so the OSD sits in the corner of the area, on the black bars if any.
    pub unsafe fn layout(&self, area: RECT) {
        let (w, h) = (
            (area.right - area.left).max(1),
            (area.bottom - area.top).max(1),
        );
        let _ = MoveWindow(self.surface.0, area.left, area.top, w, h, true);
        self.player.resize(w, h);
    }

    /// Jumps to the next / previous key frame (exact seek, so the frame shows immediately).
    pub unsafe fn seek_keyframe(&mut self, forward: bool) {
        if self.keyframes.is_none() {
            self.keyframes = KeyframeIndex::open(&self.path);
        }
        let Some(index) = &self.keyframes else { return };
        let now = self.player.position();
        let target = if forward {
            index.next_after(now + 0.01)
        } else {
            index.previous_before(now - KEYFRAME_BACK_SLACK_SEC)
        };
        if let Some(t) = target {
            self.player.seek(t, false);
        }
    }

    /// The frame on screen, at its native size.
    pub unsafe fn capture_frame(&self) -> Option<crate::image_cache::DecodedImage> {
        let (width, height, bgra) = self.player.capture_frame()?;
        Some(crate::image_cache::DecodedImage {
            width,
            height,
            bgra,
            is_preview: false,
            exif: None,
        })
    }

    /// "1920x1080, 1:23:45"
    pub fn title_info(&self) -> String {
        format!(
            "{}x{}, {}",
            self.info.width,
            self.info.height,
            format_time(self.info.duration_sec)
        )
    }

    pub unsafe fn on_event(&mut self, event: i32, param1: isize) -> EventEffect {
        if event == MF_MEDIA_ENGINE_EVENT_SEEKED.0 {
            if let Some(target) = self.back_target.take() {
                let frame = if self.info.frame_rate > 0.0 {
                    1.0 / self.info.frame_rate
                } else {
                    FALLBACK_FRAME_SEC
                };
                if (self.player.position() - target).abs() > 1.5 * frame {
                    self.precise_seek = false;
                }
            }
        }
        match event {
            e if e == MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA.0
                || e == MF_MEDIA_ENGINE_EVENT_FORMATCHANGE.0 =>
            {
                if let Some((w, h)) = self.player.native_size() {
                    self.info.width = w;
                    self.info.height = h;
                }
                if e == MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA.0 {
                    self.loaded = true;
                    if let Some(t) = self.resume_at.take() {
                        self.player.seek(t, false);
                        self.resumed_to = Some(t);
                    }
                }
                EventEffect::Relayout
            }
            e if e == MF_MEDIA_ENGINE_EVENT_ENDED.0 => EventEffect::Ended,
            e if e == MF_MEDIA_ENGINE_EVENT_ERROR.0 => {
                self.error = Some(engine_error_text(
                    self.player.error_code().unwrap_or(param1 as u16),
                ));
                EventEffect::RepaintBar
            }
            e if is_progress_event(e) => EventEffect::RepaintBar,
            _ => EventEffect::None,
        }
    }
}

fn resume_point(path: &Path, info: &VideoMeta, enabled: bool) -> Option<f64> {
    (enabled && info.duration_sec >= resume::MIN_DURATION_SEC)
        .then(|| resume::load(path))
        .flatten()
}

impl Drop for VideoView {
    fn drop(&mut self) {
        self.remember_position();
        unsafe {
            let _ = KillTimer(Some(self.viewer), RENDER_TIMER_ID);
        }
    }
}

/// Engine events after which the bar shows something new.
pub fn is_progress_event(event: i32) -> bool {
    [
        MF_MEDIA_ENGINE_EVENT_TIMEUPDATE,
        MF_MEDIA_ENGINE_EVENT_PLAY,
        MF_MEDIA_ENGINE_EVENT_PLAYING,
        MF_MEDIA_ENGINE_EVENT_PAUSE,
        MF_MEDIA_ENGINE_EVENT_SEEKED,
        MF_MEDIA_ENGINE_EVENT_DURATIONCHANGE,
        MF_MEDIA_ENGINE_EVENT_VOLUMECHANGE,
    ]
    .iter()
    .any(|x| x.0 == event)
}

pub fn engine_error_text(code: u16) -> String {
    let reason = match code {
        3 => tr("ошибка декодирования", "decoding error"),
        4 => tr(
            "формат или кодек не поддерживается",
            "format or codec not supported",
        ),
        5 => tr("файл зашифрован", "file is encrypted"),
        _ => tr("ошибка воспроизведения", "playback error"),
    };
    format!(
        "{}: {} ({} {})",
        tr("Не удалось воспроизвести", "Cannot play"),
        reason,
        tr("код", "code"),
        code
    )
}
