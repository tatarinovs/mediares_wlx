//! Video content: a child surface the media engine renders into, filling the area above
//! the transport bar (the bar itself belongs to [`crate::media_view`]).
//!
//! The surface is transparent to mouse input (`HTTRANSPARENT`), so the viewer window receives
//! every click and decides between the video area and the bar.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

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
use crate::playback_mpv::{self, MpvPlayer};
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

/// The media engine, or libmpv when it is installed.
enum Player {
    Mf(VideoPlayer),
    Mpv(MpvPlayer),
}

impl Player {
    unsafe fn new(surface: HWND, viewer: HWND) -> Option<Self> {
        if playback_mpv::available() {
            if let Some(p) = MpvPlayer::new(surface, viewer) {
                return Some(Self::Mpv(p));
            }
        }
        VideoPlayer::new(surface, viewer).ok().map(Self::Mf)
    }

    fn transport(&self) -> &dyn Transport {
        match self {
            Self::Mf(p) => p,
            Self::Mpv(p) => p,
        }
    }

    unsafe fn open(&self, path: &Path) -> bool {
        match self {
            Self::Mf(p) => p.open(path).is_ok(),
            Self::Mpv(p) => p.open(path),
        }
    }

    unsafe fn render(&self, osd: Option<Osd<'_>>) {
        match self {
            Self::Mf(p) => p.render(osd),
            Self::Mpv(p) => p.render(osd),
        }
    }

    unsafe fn resize(&self, width: i32, height: i32) {
        match self {
            Self::Mf(p) => p.resize(width, height),
            Self::Mpv(p) => p.resize(width, height),
        }
    }

    unsafe fn native_size(&self) -> Option<(u32, u32)> {
        match self {
            Self::Mf(p) => p.native_size(),
            Self::Mpv(p) => p.native_size(),
        }
    }

    unsafe fn capture_frame(&self) -> Option<(u32, u32, Vec<u8>)> {
        match self {
            Self::Mf(p) => p.capture_frame(),
            Self::Mpv(p) => p.capture_frame(),
        }
    }

    unsafe fn set_rate(&self, rate: f64) {
        match self {
            Self::Mf(p) => p.set_rate(rate),
            Self::Mpv(p) => p.set_rate(rate),
        }
    }

    unsafe fn set_current_rate(&self, rate: f64) {
        match self {
            Self::Mf(p) => p.set_current_rate(rate),
            Self::Mpv(p) => p.set_rate(rate),
        }
    }

    unsafe fn is_paused(&self) -> bool {
        match self {
            Self::Mf(p) => p.is_paused(),
            Self::Mpv(p) => p.is_paused(),
        }
    }

    unsafe fn is_seeking(&self) -> bool {
        match self {
            Self::Mf(p) => p.is_seeking(),
            Self::Mpv(p) => p.is_seeking(),
        }
    }

    fn presented_pts(&self) -> Option<i64> {
        match self {
            Self::Mf(p) => p.presented_pts(),
            Self::Mpv(_) => None,
        }
    }

    unsafe fn step_back(&self, frame_rate: f64) -> f64 {
        match self {
            Self::Mf(p) => p.step_back(frame_rate),
            Self::Mpv(p) => {
                p.frame_step(false);
                p.position()
            }
        }
    }

    unsafe fn error_code(&self) -> Option<u16> {
        match self {
            Self::Mf(p) => p.error_code(),
            Self::Mpv(p) => p.error_code(),
        }
    }

    fn is_playing(&self) -> bool {
        self.transport().is_playing()
    }
    fn play(&self) {
        self.transport().play()
    }
    fn pause(&self) {
        self.transport().pause()
    }
    fn position(&self) -> f64 {
        self.transport().position()
    }
    fn duration(&self) -> f64 {
        self.transport().duration()
    }
    fn seek(&self, seconds: f64, approximate: bool) {
        self.transport().seek(seconds, approximate)
    }
    fn volume(&self) -> f64 {
        self.transport().volume()
    }
    fn set_volume(&self, volume: f64) {
        self.transport().set_volume(volume)
    }
    fn is_muted(&self) -> bool {
        self.transport().is_muted()
    }
    fn set_muted(&self, muted: bool) {
        self.transport().set_muted(muted)
    }
}

/// Field order matters: the player (engine) shuts down before its surface window is destroyed.
pub struct VideoView {
    player: Player,
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
    /// ±5 s seeking with the arrow keys.
    key_seek: Option<KeySeek>,
}

/// Arrow-key seeking, from the first press until the key is released. Playback pauses while the
/// key is held; only one seek is sent to the engine at a time: steps arriving meanwhile just move
/// the target, sent when the engine is done. The bar shows the target meanwhile.
struct KeySeek {
    target: f64,
    /// When the target last stepped.
    at: Instant,
    /// Last target handed to the engine (NaN: none yet).
    sent: f64,
    /// Playback resumes once the key is released (play / pause while seeking change it).
    was_playing: Cell<bool>,
}

/// No step for this long: the key was released.
const KEY_SEEK_QUIET: Duration = Duration::from_millis(250);

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
        let info = video_meta(path)?;
        let surface = Surface::new(viewer, true)?;
        let player = Player::new(surface.0, viewer)?;
        transport_bar::restore_audio_level(player.transport());
        if !player.open(path) {
            return None;
        }

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
            key_seek: None,
        })
    }

    /// Switches to another file, reusing the engine (and its speed).
    pub unsafe fn open(&mut self, path: &Path, resume: bool) -> bool {
        let Some(info) = video_meta(path) else {
            return false;
        };
        self.remember_position();
        self.finish_step();
        if !self.player.open(path) {
            return false;
        }
        self.precise_seek = seeks_precisely(path);
        self.back_target = None;
        self.key_seek = None;
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
        self.key_seek = None;
        if let Player::Mpv(p) = &self.player {
            p.frame_step(forward);
            return true;
        }
        if !forward {
            if !self.precise_seek {
                return false;
            }
            self.finish_step();
            self.back_target = Some(self.player.step_back(self.info.frame_rate));
            return true;
        }
        match &mut self.stepping {
            // Key auto-repeat outruns a slow decoder: queue at most one more frame, so the
            // picture stops as soon as the key is released.
            Some(step) => step.remaining = (step.remaining + 1).min(2),
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
        self
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// `WM_TIMER` with [`RENDER_TIMER_ID`].
    pub unsafe fn render(&mut self) {
        match &self.osd {
            Some(osd) => {
                let position = format_time(self.position());
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
        self.advance_key_seek();
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
        self.cancel_key_seek();
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

    /// ±5 s (see [`KeySeek`]). While the key is held, steps continue from the previous target,
    /// not from the position: that lags behind while seeking.
    pub unsafe fn seek_by(&mut self, forward: bool) {
        self.finish_step();
        let (base, sent, was_playing) = match self.key_seek.take() {
            Some(s) => {
                // Held down: pause, or the engine would try to play between the seeks.
                if self.player.is_playing() {
                    self.player.pause();
                }
                (s.target, s.sent, s.was_playing)
            }
            None => (
                self.player.position(),
                f64::NAN,
                Cell::new(self.player.is_playing()),
            ),
        };
        self.key_seek = Some(KeySeek {
            target: transport_bar::step_target(base, self.player.duration(), forward),
            at: Instant::now(),
            sent,
            was_playing,
        });
        self.send_key_seek();
    }

    /// Ends arrow-key seeking (released, or another kind of seek takes over); playback resumes
    /// if it was on.
    pub fn cancel_key_seek(&mut self) {
        if let Some(s) = self.key_seek.take() {
            if s.was_playing.get() && !self.player.is_playing() {
                self.player.play();
            }
        }
    }

    /// Hands the arrow-key target to the engine unless it is still busy with the previous seek.
    /// The first press seeks exactly; held-down steps snap to key frames (seeking by eye, and
    /// only one frame to decode).
    unsafe fn send_key_seek(&mut self) {
        let Some(s) = &mut self.key_seek else { return };
        if s.sent != s.target && !self.player.is_seeking() {
            self.player.seek(s.target, !s.sent.is_nan());
            s.sent = s.target;
        }
    }

    /// Render tick / seek completed: sends a target that waited for the engine, and ends arrow-key
    /// seeking once the key is released and the last seek is done.
    unsafe fn advance_key_seek(&mut self) {
        let Some(s) = &self.key_seek else { return };
        if s.sent != s.target {
            self.send_key_seek();
        } else if s.at.elapsed() >= KEY_SEEK_QUIET && !self.player.is_seeking() {
            self.cancel_key_seek();
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
            // A target that waited for this seek goes out now rather than on the next tick.
            self.advance_key_seek();
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

/// Stream properties from Media Foundation. mpv plays containers Media Foundation can't read;
/// their size and duration come from the player once the file is open.
fn video_meta(path: &Path) -> Option<VideoMeta> {
    match get_video_meta(path) {
        Some(info) => Some((*info).clone()),
        None if playback_mpv::available() => Some(VideoMeta::default()),
        None => None,
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

/// The player, as seen through arrow-key seeking: its pause doesn't count and the bar shows the
/// target.
impl Transport for VideoView {
    fn is_playing(&self) -> bool {
        match &self.key_seek {
            Some(s) => s.was_playing.get(),
            None => self.player.is_playing(),
        }
    }

    fn play(&self) {
        if let Some(s) = &self.key_seek {
            s.was_playing.set(true);
        }
        self.player.play()
    }

    fn pause(&self) {
        if let Some(s) = &self.key_seek {
            s.was_playing.set(false);
        }
        self.player.pause()
    }

    fn position(&self) -> f64 {
        match &self.key_seek {
            Some(s) => s.target,
            None => self.player.position(),
        }
    }

    fn duration(&self) -> f64 {
        self.player.duration()
    }

    fn seek(&self, seconds: f64, approximate: bool) {
        self.player.seek(seconds, approximate)
    }

    fn volume(&self) -> f64 {
        self.player.volume()
    }

    fn set_volume(&self, volume: f64) {
        self.player.set_volume(volume)
    }

    fn is_muted(&self) -> bool {
        self.player.is_muted()
    }

    fn set_muted(&self, muted: bool) {
        self.player.set_muted(muted)
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
