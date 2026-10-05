//! Video content: a child surface the media engine renders into, filling the area above
//! the transport bar (the bar itself belongs to [`crate::media_view`]).
//!
//! The surface is transparent to mouse input (`HTTRANSPARENT`), so the viewer window receives
//! every click and decides between the video area and the bar.

use std::cell::{Cell, OnceCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use mediares_core::cache::{get_video_meta, get_video_tags};
use mediares_core::keyframes::{KeyFrame, KeyframeIndex};
use mediares_core::video_frame::VideoMeta;
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
use crate::playback_video::{frame_sec, Osd, VideoPlayer};
use crate::resume;
use crate::transport_bar::{self, format_time, Transport, SEEK_END_MARGIN_SEC};

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
    /// The media engine, if that is the player.
    fn mf(&self) -> Option<&VideoPlayer> {
        match self {
            Self::Mf(p) => Some(p),
            Self::Mpv(_) => None,
        }
    }

    unsafe fn new(surface: HWND, viewer: HWND) -> Option<Self> {
        if playback_mpv::available() {
            if let Some(p) = MpvPlayer::new(surface, viewer) {
                return Some(Self::Mpv(p));
            }
        }
        VideoPlayer::new(surface, viewer).ok().map(Self::Mf)
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

    fn set_frame_rate(&self, frame_rate: f64) {
        if let Self::Mf(p) = self {
            p.set_frame_rate(frame_rate);
        }
    }

    /// A frame other than `before` (a [`Self::presented_pts`]) is on screen. mpv reports a seek
    /// done only once its frame is ready, so for it the end of the seek is enough.
    fn shown_since(&self, before: Option<i64>) -> bool {
        match self {
            Self::Mf(p) => p.presented_pts() != before,
            Self::Mpv(_) => true,
        }
    }

    unsafe fn error_code(&self) -> Option<u16> {
        match self {
            Self::Mf(p) => p.error_code(),
            Self::Mpv(p) => p.error_code(),
        }
    }
}

/// Play, pause, seek, volume... go straight to the backend's [`Transport`].
impl std::ops::Deref for Player {
    type Target = dyn Transport;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Mf(p) => p,
            Self::Mpv(p) => p,
        }
    }
}

/// Field order matters: the player (engine) shuts down before its surface window is destroyed.
pub struct VideoView {
    player: Player,
    surface: Surface,
    viewer: HWND,
    path: PathBuf,
    /// Opened on first use (holding `None`: the file has none to offer); reset when the file
    /// changes.
    keyframes: OnceCell<Option<KeyframeIndex>>,
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
    /// Paused by a frame step: to reach the next frame the player runs for a moment (the media
    /// engine slowly, mpv by unpausing itself), but that counts as paused until play / pause.
    step_paused: Cell<bool>,
    /// How seeking behaves on this file.
    seek_profile: SeekProfile,
    /// A step back was requested to here; checked when the seek completes.
    back_target: Option<f64>,
    /// Seeking with the arrow keys.
    key_seek: Option<KeySeek>,
    /// The time of the key frame on screen while the engine is still elsewhere
    /// ([`SeekProfile::Previewed`]).
    preview: Cell<Option<f64>>,
}

/// How seeking behaves on the current file.
#[derive(Clone, Copy, PartialEq)]
enum SeekProfile {
    /// Seeks land on the requested frame: indexed containers on the media engine, and mpv.
    Precise,
    /// The media engine snaps seeks to the next group of pictures (MPEG program streams, and
    /// files found out at run time), so stepping back is not offered.
    Snapping,
    /// Transport streams on the media engine. Their source seeks by a bitrate estimate: exact
    /// seeks take up to seconds, approximate ones land up to a minute off. So, as mpv does by
    /// default, arrow keys and the timeline go to key frames, shown decoded from the index while
    /// the engine stays put; it follows once they rest, always exactly.
    Previewed,
}

impl SeekProfile {
    fn of(player: &Player, path: &Path) -> Self {
        use mediares_core::probe::{has_extension, is_transport_stream};
        match player {
            Player::Mpv(_) => Self::Precise,
            Player::Mf(_) if is_transport_stream(path) => Self::Previewed,
            Player::Mf(_) if has_extension(path, &["mpg", "mpeg", "vob"]) => Self::Snapping,
            Player::Mf(_) => Self::Precise,
        }
    }
}

/// Arrow-key seeking, from the first press until the key is released. Playback pauses while the
/// key is held; only one seek is sent to the engine at a time: steps arriving meanwhile just move
/// the target, sent once the engine has shown the frame of the previous one (as VLC postpones
/// seeks until its decoders are fed). The bar shows the target meanwhile.
struct KeySeek {
    target: f64,
    /// How `target` is reached.
    landing: Landing,
    /// When the target last stepped.
    at: Instant,
    /// Last target handed to the engine (NaN: none yet).
    sent: f64,
    /// The frame on screen when `sent` went out.
    pts_at_send: Option<i64>,
    /// When the engine was first seen done with `sent`.
    seek_done: Option<Instant>,
    /// Playback resumes once the key is released (play / pause while seeking change it).
    was_playing: Cell<bool>,
}

/// How a [`KeySeek`] target is reached.
enum Landing {
    /// An exact seek: it decodes from the preceding key frame up to the target.
    Exact,
    /// A frame step back (`,` on the media engine): exact, and checked for accurate seeking.
    FrameBack,
    /// Wherever an approximate seek lands (held keys without a key frame index).
    Approximate,
    /// The key frame's picture, the engine left where it is ([`SeekProfile::Previewed`]).
    Preview(KeyFrame),
}

/// No step for this long: the key was released.
const KEY_SEEK_QUIET: Duration = Duration::from_millis(250);
/// A finished seek whose frame hasn't shown up by then doesn't hold the next one back any longer
/// (VLC's limit, too).
const SEEK_SETTLE: Duration = Duration::from_millis(125);

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
    /// Parsed once per change, rendered on every frame.
    template: crate::osd_template::Template,
    fields: HashMap<&'static str, String>,
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
        player.set_frame_rate(info.frame_rate);
        transport_bar::restore_audio_level(&*player);
        if !player.open(path) {
            return None;
        }

        SetTimer(Some(viewer), RENDER_TIMER_ID, RENDER_INTERVAL_MS, None);
        let resume_at = resume_point(path, &info, resume);
        let seek_profile = SeekProfile::of(&player, path);
        Some(Self {
            player,
            surface,
            viewer,
            path: path.to_path_buf(),
            keyframes: OnceCell::new(),
            info,
            error: None,
            osd: None,
            rate: 1.0,
            resume,
            resume_at,
            resumed_to: None,
            loaded: false,
            stepping: None,
            step_paused: Cell::new(false),
            seek_profile,
            back_target: None,
            key_seek: None,
            preview: Cell::new(None),
        })
    }

    /// Switches to another file, reusing the engine (and its speed).
    pub unsafe fn open(&mut self, path: &Path, resume: bool) -> bool {
        let Some(info) = video_meta(path) else {
            return false;
        };
        self.remember_position();
        self.finish_step();
        self.player.set_frame_rate(info.frame_rate);
        if !self.player.open(path) {
            return false;
        }
        self.seek_profile = SeekProfile::of(&self.player, path);
        self.preview.set(None);
        self.back_target = None;
        self.key_seek = None;
        self.resume = resume;
        self.resume_at = resume_point(path, &info, resume);
        self.resumed_to = None;
        self.loaded = false;
        self.info = info;
        self.path = path.to_path_buf();
        self.keyframes = OnceCell::new();
        self.step_paused.set(false);
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
        self.end_frame_step();
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
        let mf = matches!(self.player, Player::Mf(_));
        if mf && !forward && self.seek_profile == SeekProfile::Snapping {
            return false;
        }
        self.step_paused.set(true);
        if mf && !forward {
            self.step_back();
            return true;
        }
        // Steps end paused, so a held seek isn't resumed.
        self.settle_preview();
        self.key_seek = None;
        if let Player::Mpv(p) = &self.player {
            p.frame_step(forward);
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

    /// One frame back on the media engine: an exact seek, which decodes from the preceding key
    /// frame. Held down, it goes through [`KeySeek`] like the arrow keys: auto-repeat would
    /// otherwise restart the seek before it is done, and while seeking the engine still reports
    /// the old position, so the picture would stand still until the key is released.
    unsafe fn step_back(&mut self) {
        self.finish_step();
        let target = (self.key_seek_base() - frame_sec(self.info.frame_rate)).max(0.0);
        self.key_seek_to(target, Landing::FrameBack, false);
    }

    /// Render tick of a forward step: pause once enough new frames were shown.
    unsafe fn advance_step(&mut self) {
        let Some(step) = &mut self.stepping else {
            return;
        };
        if self.player.is_paused() || !self.step_paused.get() {
            // Paused or played by the user (or the end reached) mid-step.
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
                let text = osd.template.render(|key| match key {
                    "time" => Some(position.clone()),
                    "duration" => Some(duration.clone()),
                    _ => osd.fields.get(key).cloned(),
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
        let Some(style) = style else {
            self.osd = None;
            return;
        };
        let template = crate::osd_template::Template::parse(&text.template);
        let fields = text.fields;
        self.osd = Some(match self.osd.take() {
            Some(osd) if osd.style == style => VideoOsd {
                template,
                fields,
                ..osd
            },
            _ => {
                let dpi = (crate::gdi::dpi_scale(self.viewer) * 96.0).round() as i32;
                let font = crate::image_view::create_osd_font(&style.face, style.size_pt, dpi);
                VideoOsd {
                    style,
                    font,
                    template,
                    fields,
                }
            }
        });
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

    /// Cuts a frame step short, paused as it would have ended, and forgets its step-back check:
    /// another kind of seek (or a speed change) takes over.
    unsafe fn end_frame_step(&mut self) {
        if self.step_paused.get() {
            self.player.pause();
        }
        self.finish_step();
        self.back_target = None;
    }

    /// The key frame index, opened on first use (once: a file without one isn't retried).
    fn keyframe_index(&self) -> Option<&KeyframeIndex> {
        self.keyframes
            .get_or_init(|| KeyframeIndex::open(&self.path))
            .as_ref()
    }

    /// Jumps to the next / previous key frame (exact seek, so the frame shows immediately).
    pub unsafe fn seek_keyframe(&mut self, forward: bool) {
        self.end_frame_step();
        self.cancel_key_seek();
        let now = self.player.position();
        let Some(index) = self.keyframe_index() else {
            return;
        };
        let key = if forward {
            index.next_after(now + 0.01)
        } else {
            index.previous_before(now - KEYFRAME_BACK_SLACK_SEC)
        };
        if let Some(key) = key {
            self.engine_seek(key.time, false);
        }
    }

    /// ±`step` s (see [`KeySeek`]). While the key is held, steps continue from the previous target,
    /// not from the position: that lags behind while seeking.
    pub unsafe fn seek_by(&mut self, forward: bool, step: f64) {
        let was_playing = Transport::is_playing(self);
        self.end_frame_step();
        let duration = self.player.duration();
        let target = transport_bar::step_target(self.key_seek_base(), duration, forward, step);
        // The first press seeks exactly; held-down steps go to key frames. Previewed, every step
        // does (see `SeekProfile::Previewed`).
        let previewed = self.seek_profile == SeekProfile::Previewed;
        let (target, landing) = if self.key_seek.is_none() && !previewed {
            (target, Landing::Exact)
        } else {
            match self.keyframe_toward(target, forward, duration) {
                Some(key) if previewed => (key.time, Landing::Preview(key)),
                Some(key) => (key.time, Landing::Exact),
                None => (target, Landing::Approximate),
            }
        };
        self.key_seek_to(target, landing, was_playing);
    }

    /// Where the next step starts: the previous target while a key is held (the position lags
    /// behind while seeking), else the position.
    fn key_seek_base(&self) -> f64 {
        match &self.key_seek {
            Some(s) => s.target,
            None => self.player.position(),
        }
    }

    /// Moves the held-down target, or starts [`KeySeek`] at it, and sends it if the engine is
    /// ready. Held down (and for frame steps and previews, which stand still) the player pauses,
    /// or it would play between the seeks.
    unsafe fn key_seek_to(&mut self, target: f64, landing: Landing, was_playing: bool) {
        let frame_back = matches!(landing, Landing::FrameBack);
        let stands_still = frame_back || matches!(landing, Landing::Preview(_));
        if (stands_still || self.key_seek.is_some()) && self.player.is_playing() {
            self.player.pause();
        }
        match &mut self.key_seek {
            Some(s) => {
                s.target = target;
                s.landing = landing;
                s.at = Instant::now();
                if frame_back {
                    s.was_playing.set(false);
                }
            }
            None => {
                self.key_seek = Some(KeySeek {
                    target,
                    landing,
                    at: Instant::now(),
                    sent: f64::NAN,
                    pts_at_send: None,
                    seek_done: None,
                    was_playing: Cell::new(was_playing),
                })
            }
        }
        self.send_key_seek();
    }

    /// The key frame at or beyond `target` in the direction of travel, for a held arrow key on
    /// the media engine. Its approximate seek lands on the key frame *before* the target, so
    /// with groups of pictures longer than the step the picture alternately stands and jumps;
    /// these targets advance evenly and decode one frame each. mpv picks well itself (`None`).
    fn keyframe_toward(&self, target: f64, forward: bool, duration: f64) -> Option<KeyFrame> {
        self.player.mf()?;
        let index = self.keyframe_index()?;
        if forward {
            // Not into the end margin (see `step_target`): that would end the file.
            index
                .next_after(target - 1e-3)
                .filter(|key| duration <= 0.0 || key.time <= duration - SEEK_END_MARGIN_SEC)
        } else {
            index.previous_before(target + 1e-3)
        }
    }

    /// Ends arrow-key seeking (released, or another kind of seek takes over); playback resumes
    /// if it was on.
    pub fn cancel_key_seek(&mut self) {
        self.settle_preview();
        if let Some(s) = self.key_seek.take() {
            if s.was_playing.get() && !self.player.is_playing() {
                self.player.play();
            }
        }
    }

    /// Hands the arrow-key target to the engine unless it is still busy with the previous seek,
    /// or done but its frame hasn't reached the screen yet: Media Foundation reports a seek done
    /// before the frame arrives, and a new seek would flush it, leaving the picture stuck while
    /// the key is held. A previewed key frame needs no engine and shows at once.
    unsafe fn send_key_seek(&mut self) {
        let Some(s) = &self.key_seek else { return };
        if s.sent == s.target {
            return;
        }
        if let Landing::Preview(key) = &s.landing {
            if self.show_key_frame(key) {
                if let Some(s) = &mut self.key_seek {
                    s.sent = s.target;
                }
                return;
            }
        }
        let Some(s) = &mut self.key_seek else { return };
        if self.player.is_seeking() {
            s.seek_done = None;
            return;
        }
        if !s.sent.is_nan() {
            let done = *s.seek_done.get_or_insert_with(Instant::now);
            if !self.player.shown_since(s.pts_at_send) && done.elapsed() < SEEK_SETTLE {
                return;
            }
        }
        s.pts_at_send = self.player.presented_pts();
        s.seek_done = None;
        s.sent = s.target;
        let (target, landing) = (s.target, &s.landing);
        let approximate = matches!(landing, Landing::Approximate);
        let checked = matches!(landing, Landing::FrameBack);
        self.engine_seek(target, approximate);
        if checked {
            self.back_target = Some(target);
        }
    }

    /// Render tick / seek completed: sends a target that waited for the engine, and ends arrow-key
    /// seeking once the key is released and the last seek is done (a previewed one is the
    /// engine's turn first).
    unsafe fn advance_key_seek(&mut self) {
        let Some(s) = &self.key_seek else { return };
        if s.sent != s.target {
            self.send_key_seek();
        } else if s.at.elapsed() >= KEY_SEEK_QUIET && !self.player.is_seeking() {
            if self.preview.get().is_some() {
                self.settle_preview();
            } else {
                self.cancel_key_seek();
            }
        }
    }

    /// Every engine seek goes through here: it supersedes a preview, and is exact where
    /// approximate ones land far off.
    fn engine_seek(&self, seconds: f64, approximate: bool) {
        if self.preview.take().is_some() {
            if let Some(player) = self.player.mf() {
                player.end_preview();
            }
        }
        let approximate = approximate && self.seek_profile != SeekProfile::Previewed;
        self.player.seek(seconds, approximate);
    }

    /// Shows `key` decoded, the engine left where it is; false if it can't be decoded.
    fn show_key_frame(&self, key: &KeyFrame) -> bool {
        let (Some(player), Some(index)) = (self.player.mf(), self.keyframe_index()) else {
            return false;
        };
        let Some(picture) = index.picture(key) else {
            return false;
        };
        player.show_picture(picture);
        self.preview.set(Some(key.time));
        true
    }

    /// Brings the engine to the key frame a preview left on screen: the arrow key was released,
    /// a timeline drag was cut short, or something else takes over.
    pub fn settle_preview(&self) {
        if let Some(time) = self.preview.get() {
            self.engine_seek(time, false);
        }
    }

    /// The frame on screen, at its native size.
    pub unsafe fn capture_frame(&self) -> Option<crate::image_cache::DecodedImage> {
        let (width, height, bgra) = self.player.capture_frame()?;
        Some(crate::image_cache::DecodedImage {
            width,
            height,
            full: (width, height),
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
            // Checked before the next step goes out; a stale event (another seek already under
            // way) doesn't tell where this one landed.
            if !self.player.is_seeking() {
                if let Some(target) = self.back_target.take() {
                    let off = (self.player.position() - target).abs();
                    if off > 1.5 * frame_sec(self.info.frame_rate) {
                        self.seek_profile = SeekProfile::Snapping;
                    }
                }
            }
            // A target that waited for this seek goes out now rather than on the next tick.
            self.advance_key_seek();
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
                        self.engine_seek(t, false);
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

/// The player, as seen through arrow-key seeking and frame steps: their pausing and playing
/// don't count and the bar shows the target.
impl Transport for VideoView {
    fn is_playing(&self) -> bool {
        match &self.key_seek {
            Some(s) => s.was_playing.get(),
            None => !self.step_paused.get() && self.player.is_playing(),
        }
    }

    fn play(&self) {
        if let Some(s) = &self.key_seek {
            s.was_playing.set(true);
        }
        self.step_paused.set(false);
        self.player.play()
    }

    fn pause(&self) {
        if let Some(s) = &self.key_seek {
            s.was_playing.set(false);
        }
        self.step_paused.set(false);
        self.player.pause()
    }

    fn position(&self) -> f64 {
        match &self.key_seek {
            Some(s) => s.target,
            None => self.preview.get().unwrap_or_else(|| self.player.position()),
        }
    }

    fn duration(&self) -> f64 {
        self.player.duration()
    }

    /// Approximate seeks come from the timeline being clicked or dragged. Previewed, they show the
    /// key frame at or before `seconds` (mpv's key frame scrubbing); the exact seek on release
    /// brings the engine there.
    fn seek(&self, seconds: f64, approximate: bool) {
        if approximate && self.seek_profile == SeekProfile::Previewed && self.key_seek.is_none() {
            let key = self
                .keyframe_index()
                .and_then(|index| index.previous_before(seconds + 1e-3));
            if key.is_some_and(|key| self.show_key_frame(&key)) {
                return;
            }
        }
        self.engine_seek(seconds, approximate)
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
        3 => tr("decoding error"),
        4 => tr("format or codec not supported"),
        5 => tr("file is encrypted"),
        _ => tr("playback error"),
    };
    format!(
        "{}: {} ({} {})",
        tr("Cannot play"),
        reason,
        tr("code"),
        code
    )
}
