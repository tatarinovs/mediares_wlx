//! Video content: a child surface the media engine renders into, letterboxed into the area above
//! the transport bar (the bar itself belongs to [`crate::media_view`]).
//!
//! The surface is transparent to mouse input (`HTTRANSPARENT`), so the viewer window receives
//! every click and decides between the video area and the bar.

use std::path::{Path, PathBuf};
use std::sync::Once;

use mediares_core::video_frame::{probe_video, KeyframeIndex, VideoInfo};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
use windows::Win32::Media::MediaFoundation::{
    MF_MEDIA_ENGINE_EVENT_DURATIONCHANGE, MF_MEDIA_ENGINE_EVENT_ENDED, MF_MEDIA_ENGINE_EVENT_ERROR,
    MF_MEDIA_ENGINE_EVENT_FORMATCHANGE, MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA,
    MF_MEDIA_ENGINE_EVENT_PAUSE, MF_MEDIA_ENGINE_EVENT_PLAY, MF_MEDIA_ENGINE_EVENT_PLAYING,
    MF_MEDIA_ENGINE_EVENT_SEEKED, MF_MEDIA_ENGINE_EVENT_TIMEUPDATE, MF_MEDIA_ENGINE_EVENT_VOLUMECHANGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, IsWindow, KillTimer, MoveWindow, RegisterClassExW,
    SetTimer, HTTRANSPARENT, WINDOW_EX_STYLE, WINDOW_STYLE, WM_NCHITTEST, WNDCLASSEXW, WS_CHILD,
    WS_CLIPSIBLINGS, WS_VISIBLE,
};

use crate::media_view::EventEffect;
use crate::module;
use crate::playback_video::VideoPlayer;
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
        CreateWindowExW(WINDOW_EX_STYLE(0), SURFACE_CLASS, None, style, 0, 0, 1, 1, Some(viewer), None, Some(module()), None)
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

unsafe extern "system" fn surface_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn register_surface_class() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(surface_proc),
            hInstance: module(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: SURFACE_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
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
    pub info: VideoInfo,
    error: Option<String>,
}

impl VideoView {
    /// Creates the surface inside `viewer` and starts playing `path`. `None` if Media Foundation
    /// cannot open the file (so TC can fall back to another plugin).
    pub unsafe fn new(viewer: HWND, path: &Path) -> Option<Self> {
        let info = probe_video(path)?;
        let surface = Surface::new(viewer, true)?;
        let player = VideoPlayer::new(surface.0, viewer).ok()?;
        transport_bar::restore_audio_level(&player);
        player.open(path).ok()?;

        SetTimer(Some(viewer), RENDER_TIMER_ID, RENDER_INTERVAL_MS, None);
        Some(Self { player, surface, viewer, path: path.to_path_buf(), keyframes: None, info, error: None })
    }

    /// Switches to another file, reusing the engine.
    pub unsafe fn open(&mut self, path: &Path) -> bool {
        let Some(info) = probe_video(path) else { return false };
        if self.player.open(path).is_err() {
            return false;
        }
        self.info = info;
        self.path = path.to_path_buf();
        self.keyframes = None;
        self.error = None;
        true
    }

    pub fn transport(&self) -> &dyn Transport {
        &self.player
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// `WM_TIMER` with [`RENDER_TIMER_ID`].
    pub unsafe fn render(&self) {
        self.player.render();
    }

    /// Letterboxes the surface into `area` (viewer client coordinates).
    pub unsafe fn layout(&self, area: RECT) {
        let (area_w, area_h) = ((area.right - area.left).max(1), (area.bottom - area.top).max(1));
        let (vw, vh) = self.player.native_size().unwrap_or((self.info.width, self.info.height));
        let (w, h) = if vw > 0 && vh > 0 {
            let scale = (area_w as f64 / vw as f64).min(area_h as f64 / vh as f64);
            (((vw as f64 * scale).round() as i32).max(1), ((vh as f64 * scale).round() as i32).max(1))
        } else {
            (area_w, area_h)
        };
        let _ = MoveWindow(self.surface.0, area.left + (area_w - w) / 2, area.top + (area_h - h) / 2, w, h, true);
        self.player.resize(w, h);
    }

    /// Jumps to the next / previous key frame (exact seek, so the frame shows immediately).
    pub unsafe fn seek_keyframe(&mut self, forward: bool) {
        if self.keyframes.is_none() {
            self.keyframes = KeyframeIndex::open(&self.path);
        }
        let Some(index) = &self.keyframes else { return };
        let now = self.player.position();
        let target = if forward { index.next_after(now + 0.01) } else { index.previous_before(now - KEYFRAME_BACK_SLACK_SEC) };
        if let Some(t) = target {
            self.player.seek(t, false);
        }
    }

    /// "1920x1080, 1:23:45"
    pub fn title_info(&self) -> String {
        format!("{}x{}, {}", self.info.width, self.info.height, format_time(self.info.duration_sec))
    }

    pub unsafe fn on_event(&mut self, event: i32, param1: isize) -> EventEffect {
        match event {
            e if e == MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA.0 || e == MF_MEDIA_ENGINE_EVENT_FORMATCHANGE.0 => {
                if let Some((w, h)) = self.player.native_size() {
                    self.info.width = w;
                    self.info.height = h;
                }
                EventEffect::Relayout
            }
            e if e == MF_MEDIA_ENGINE_EVENT_ENDED.0 => EventEffect::Ended,
            e if e == MF_MEDIA_ENGINE_EVENT_ERROR.0 => {
                self.error = Some(engine_error_text(self.player.error_code().unwrap_or(param1 as u16)));
                EventEffect::RepaintBar
            }
            e if is_progress_event(e) => EventEffect::RepaintBar,
            _ => EventEffect::None,
        }
    }
}

impl Drop for VideoView {
    fn drop(&mut self) {
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
        3 => "ошибка декодирования",
        4 => "формат или кодек не поддерживается",
        5 => "файл зашифрован",
        _ => "ошибка воспроизведения",
    };
    format!("Не удалось воспроизвести: {} (код {})", reason, code)
}
