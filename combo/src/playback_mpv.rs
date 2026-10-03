//! Playback via libmpv when `libmpv-2.dll` is found (next to the plugin or next to TC): video
//! instead of [`crate::playback_video`], audio the pure-Rust player can't decode, video
//! thumbnails, and photos WIC has no Store extension for (HEIC, AVIF, JPEG XL). Plays whatever
//! mpv's FFmpeg can, regardless of the system's Media Foundation codecs.
//!
//! mpv renders video into its own child window inside the surface (`wid`). Its events are read
//! on a thread of ours and posted to the viewer as [`WM_MEDIA_EVENT`] with the matching Media
//! Foundation event codes, so the viewer handles both backends alike.
//!
//! The GPU shader cache is essential: without it every new player compiles the renderer's
//! shaders again, about 250 ms before the first frame.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{s, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Media::MediaFoundation::{
    MF_MEDIA_ENGINE_EVENT, MF_MEDIA_ENGINE_EVENT_DURATIONCHANGE, MF_MEDIA_ENGINE_EVENT_ENDED,
    MF_MEDIA_ENGINE_EVENT_ERROR, MF_MEDIA_ENGINE_EVENT_FORMATCHANGE,
    MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA, MF_MEDIA_ENGINE_EVENT_PAUSE,
    MF_MEDIA_ENGINE_EVENT_SEEKED, MF_MEDIA_ENGINE_EVENT_TIMEUPDATE,
    MF_MEDIA_ENGINE_EVENT_VOLUMECHANGE,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use mediares_core::audio_tags::AudioTags;
use mediares_core::image::{DynamicImage, RgbaImage};
use mediares_core::mf_audio::AudioStreamMeta;
use mediares_core::video_frame::VideoMeta;

use crate::playback_video::{Osd, WM_MEDIA_EVENT};
use crate::transport_bar::Transport;

const DLL_NAME: &str = "libmpv-2.dll";

// mpv_format
const FORMAT_NONE: c_int = 0;
const FORMAT_STRING: c_int = 1;
const FORMAT_FLAG: c_int = 3;
const FORMAT_INT64: c_int = 4;
const FORMAT_DOUBLE: c_int = 5;
const FORMAT_NODE_MAP: c_int = 8;
const FORMAT_BYTE_ARRAY: c_int = 9;

// mpv_event_id
const EVENT_NONE: c_int = 0;
const EVENT_SHUTDOWN: c_int = 1;
const EVENT_END_FILE: c_int = 7;
const EVENT_FILE_LOADED: c_int = 8;
const EVENT_VIDEO_RECONFIG: c_int = 17;
const EVENT_PLAYBACK_RESTART: c_int = 21;
const EVENT_PROPERTY_CHANGE: c_int = 22;

/// `mpv_end_file_reason`
const END_FILE_ERROR: c_int = 4;

#[repr(C)]
struct Event {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct EventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct EventEndFile {
    reason: c_int,
    error: c_int,
}

#[repr(C)]
struct Node {
    u: NodeValue,
    format: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
union NodeValue {
    string: *mut c_char,
    int64: i64,
    list: *mut NodeList,
    ba: *mut ByteArray,
}

#[repr(C)]
struct NodeList {
    num: c_int,
    values: *mut Node,
    keys: *mut *mut c_char,
}

#[repr(C)]
struct ByteArray {
    data: *mut c_void,
    size: usize,
}

type Handle = *mut c_void;

/// libmpv entry points (the DLL stays loaded for the life of the process).
struct Api {
    create: unsafe extern "C" fn() -> Handle,
    initialize: unsafe extern "C" fn(Handle) -> c_int,
    terminate_destroy: unsafe extern "C" fn(Handle),
    set_option_string: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int,
    set_property_string: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int,
    get_property: unsafe extern "C" fn(Handle, *const c_char, c_int, *mut c_void) -> c_int,
    command: unsafe extern "C" fn(Handle, *const *const c_char) -> c_int,
    command_ret: unsafe extern "C" fn(Handle, *const *const c_char, *mut Node) -> c_int,
    free_node_contents: unsafe extern "C" fn(*mut Node),
    observe_property: unsafe extern "C" fn(Handle, u64, *const c_char, c_int) -> c_int,
    wait_event: unsafe extern "C" fn(Handle, f64) -> *mut Event,
    wakeup: unsafe extern "C" fn(Handle),
    free: unsafe extern "C" fn(*mut c_void),
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe { load_api() }).as_ref()
}

/// libmpv can be loaded: video plays through it.
pub fn available() -> bool {
    api().is_some()
}

/// `libmpv-2.dll` is installed; checked without loading it (it's large).
pub fn dll_found() -> bool {
    dll_candidates().next().is_some()
}

/// Next to the plugin, then next to TC. Never by bare name: the DLL search path includes the
/// current directory, which in TC is whatever folder the panel shows.
fn dll_candidates() -> impl Iterator<Item = PathBuf> {
    let exe = std::env::current_exe().ok();
    [crate::config::dll_path(), exe]
        .into_iter()
        .flatten()
        .filter_map(|p| Some(p.parent()?.join(DLL_NAME)))
        .filter(|p| p.is_file())
}

unsafe fn load_api() -> Option<Api> {
    let module = dll_candidates().find_map(|p| {
        // Altered search path: the DLL's own dependencies resolve from its folder.
        LoadLibraryExW(
            &HSTRING::from(p.as_path()),
            None,
            LOAD_WITH_ALTERED_SEARCH_PATH,
        )
        .ok()
    })?;
    macro_rules! entry {
        ($name:literal) => {{
            let f = GetProcAddress(module, s!($name))?;
            // Every field is a function pointer, the size of `f`.
            std::mem::transmute_copy(&f)
        }};
    }
    Some(Api {
        create: entry!("mpv_create"),
        initialize: entry!("mpv_initialize"),
        terminate_destroy: entry!("mpv_terminate_destroy"),
        set_option_string: entry!("mpv_set_option_string"),
        set_property_string: entry!("mpv_set_property_string"),
        get_property: entry!("mpv_get_property"),
        command: entry!("mpv_command"),
        command_ret: entry!("mpv_command_ret"),
        free_node_contents: entry!("mpv_free_node_contents"),
        observe_property: entry!("mpv_observe_property"),
        wait_event: entry!("mpv_wait_event"),
        wakeup: entry!("mpv_wakeup"),
        free: entry!("mpv_free"),
    })
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Every handle: the user's mpv.conf, scripts and key bindings stay out of the viewer.
const BASE_OPTIONS: &[(&str, &str)] = &[
    ("config", "no"),
    ("terminal", "no"),
    ("load-scripts", "no"),
    ("ytdl", "no"),
    ("osc", "no"),
    ("input-default-bindings", "no"),
    ("input-vo-keyboard", "no"),
    ("input-media-keys", "no"),
];

/// An initialized handle with [`BASE_OPTIONS`] and `options`.
unsafe fn create_handle(api: &Api, options: &[(&str, &str)]) -> Option<Handle> {
    let handle = (api.create)();
    if handle.is_null() {
        return None;
    }
    for &(name, value) in BASE_OPTIONS.iter().chain(options) {
        (api.set_option_string)(handle, cstr(name).as_ptr(), cstr(value).as_ptr());
    }
    if (api.initialize)(handle) < 0 {
        (api.terminate_destroy)(handle);
        return None;
    }
    Some(handle)
}

unsafe fn command(api: &Api, handle: Handle, args: &[&str]) -> bool {
    let owned: Vec<CString> = args.iter().map(|a| cstr(a)).collect();
    let mut argv: Vec<*const c_char> = owned.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    (api.command)(handle, argv.as_ptr()) >= 0
}

/// The current video frame at its native size, as top-down BGRA rows.
unsafe fn screenshot(api: &Api, handle: Handle) -> Option<(u32, u32, Vec<u8>)> {
    let args = [cstr("screenshot-raw"), cstr("video")];
    let argv = [args[0].as_ptr(), args[1].as_ptr(), std::ptr::null()];
    let mut node = Node {
        u: NodeValue { int64: 0 },
        format: FORMAT_NONE,
    };
    if (api.command_ret)(handle, argv.as_ptr(), &mut node) < 0 {
        return None;
    }
    let frame = read_screenshot(&node);
    (api.free_node_contents)(&mut node);
    frame
}

/// How long a video thumbnail may take to decode.
const VIDEO_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// A photo: a 24 MP HEIC takes about 0.6 s.
const PICTURE_TIMEOUT: Duration = Duration::from_secs(3);

/// Photo formats mpv's FFmpeg reads, for when WIC lacks the Store extension (HEIF / AV1 / JPEG
/// XL image extensions).
pub const PICTURE_EXTS: &[&str] = &["heic", "heif", "hif", "avif", "jxl"];

/// The frame at `fraction` (0..1) of the duration (the key frame before it), for thumbnails.
pub fn video_frame(path: &Path, fraction: f64) -> Option<RgbaImage> {
    let start = format!("{:.3}%", fraction.clamp(0.0, 1.0) * 100.0);
    grab_frame(path, &start, VIDEO_FRAME_TIMEOUT)
}

/// A photo WIC can't decode, upright (mpv applies the HEIF `irot` / `imir` transforms like WIC).
pub fn picture(path: &Path) -> Option<DynamicImage> {
    grab_frame(path, "0", PICTURE_TIMEOUT).map(DynamicImage::ImageRgba8)
}

/// The first frame from `start`, decoded without a window or sound.
fn grab_frame(path: &Path, start: &str, timeout: Duration) -> Option<RgbaImage> {
    let api = api()?;
    let (w, h, mut pixels) = unsafe {
        let handle = create_handle(
            api,
            &[
                ("vo", "null"),
                ("ao", "null"),
                ("aid", "no"),
                ("sid", "no"),
                ("pause", "yes"),
                ("hwdec", "no"),
                ("hr-seek", "no"),
                ("start", start),
                ("idle", "yes"),
                ("keep-open", "yes"),
            ],
        )?;
        let frame = first_frame(api, handle, path, timeout);
        (api.terminate_destroy)(handle);
        frame?
    };
    pixels.chunks_exact_mut(4).for_each(|px| px.swap(0, 2));
    RgbaImage::from_raw(w, h, pixels)
}

unsafe fn first_frame(
    api: &Api,
    handle: Handle,
    path: &Path,
    timeout: Duration,
) -> Option<(u32, u32, Vec<u8>)> {
    if !command(api, handle, &["loadfile", &path.to_string_lossy()]) {
        return None;
    }
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match (*(api.wait_event)(handle, left.as_secs_f64())).event_id {
            // Timed out, or the file failed.
            EVENT_NONE | EVENT_END_FILE | EVENT_SHUTDOWN => return None,
            // Paused: the frame at the start position is on the (null) video output.
            EVENT_PLAYBACK_RESTART => return screenshot(api, handle),
            _ => {}
        }
    }
}

unsafe fn get_raw<T: Default>(api: &Api, handle: Handle, name: &str, format: c_int) -> Option<T> {
    let mut value = T::default();
    let r = (api.get_property)(
        handle,
        cstr(name).as_ptr(),
        format,
        &mut value as *mut T as *mut c_void,
    );
    (r >= 0).then_some(value)
}

unsafe fn get_f64(api: &Api, handle: Handle, name: &str) -> Option<f64> {
    get_raw::<f64>(api, handle, name, FORMAT_DOUBLE).filter(|v| v.is_finite())
}

unsafe fn get_i64(api: &Api, handle: Handle, name: &str) -> Option<i64> {
    get_raw(api, handle, name, FORMAT_INT64)
}

unsafe fn get_flag(api: &Api, handle: Handle, name: &str) -> bool {
    get_raw::<c_int>(api, handle, name, FORMAT_FLAG).is_some_and(|v| v != 0)
}

unsafe fn get_string(api: &Api, handle: Handle, name: &str) -> Option<String> {
    let value: *mut c_char = get_raw::<usize>(api, handle, name, FORMAT_STRING)? as *mut c_char;
    if value.is_null() {
        return None;
    }
    let text = CStr::from_ptr(value).to_string_lossy().trim().to_string();
    (api.free)(value as *mut c_void);
    Some(text).filter(|t| !t.is_empty())
}

/// How long reading a file's streams may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A file opened by mpv's demuxer, nothing decoded: its streams, for files Media Foundation can't
/// open.
struct Probe {
    api: &'static Api,
    handle: Handle,
}

impl Probe {
    fn open(path: &Path) -> Option<Self> {
        let api = api()?;
        unsafe {
            let handle = create_handle(
                api,
                &[
                    ("vo", "null"),
                    ("ao", "null"),
                    ("pause", "yes"),
                    ("idle", "yes"),
                ],
            )?;
            let probe = Self { api, handle };
            if !command(api, handle, &["loadfile", &path.to_string_lossy()]) {
                return None;
            }
            let deadline = Instant::now() + PROBE_TIMEOUT;
            loop {
                let left = deadline.checked_duration_since(Instant::now())?;
                match (*(api.wait_event)(handle, left.as_secs_f64())).event_id {
                    EVENT_NONE | EVENT_END_FILE | EVENT_SHUTDOWN => return None,
                    EVENT_FILE_LOADED => return Some(probe),
                    _ => {}
                }
            }
        }
    }

    fn f64(&self, name: &str) -> Option<f64> {
        unsafe { get_f64(self.api, self.handle, name) }
    }

    fn u32(&self, name: &str) -> Option<u32> {
        unsafe { get_i64(self.api, self.handle, name) }
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&v| v > 0)
    }

    fn string(&self, name: &str) -> Option<String> {
        unsafe { get_string(self.api, self.handle, name) }
    }

    /// `track-list/N/` of the first track of `kind` ("video" / "audio"); cover art isn't video.
    fn track(&self, kind: &str) -> Option<String> {
        let count = unsafe { get_i64(self.api, self.handle, "track-list/count") }?;
        (0..count).map(|i| format!("track-list/{i}/")).find(|t| {
            self.string(&format!("{t}type")).as_deref() == Some(kind)
                && !unsafe { get_flag(self.api, self.handle, &format!("{t}albumart")) }
        })
    }

    fn duration(&self) -> f64 {
        self.f64("duration").unwrap_or(0.0).max(0.0)
    }

    /// Whole file (size / duration), kbit/s.
    fn bitrate_kbps(&self) -> Option<u32> {
        let duration = self.duration();
        let size = unsafe { get_i64(self.api, self.handle, "file-size") }?;
        (duration >= 1.0 && size > 0)
            .then(|| (size as f64 * 8.0 / 1000.0 / duration).round() as u32)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        unsafe { (self.api.terminate_destroy)(self.handle) };
    }
}

/// FFmpeg codec names as Media Foundation's are shown ("h264" → "H.264").
fn codec_name(codec: &str) -> String {
    let name = match codec {
        "h264" => "H.264",
        "hevc" => "HEVC",
        "av1" => "AV1",
        "vp8" => "VP8",
        "vp9" => "VP9",
        "mpeg1video" => "MPEG-1",
        "mpeg2video" => "MPEG-2",
        "mpeg4" => "MPEG-4",
        "msmpeg4v3" => "DivX 3",
        "rv10" | "rv20" | "rv30" | "rv40" => "RealVideo",
        "theora" => "Theora",
        "dvvideo" => "DV",
        "mjpeg" => "MJPEG",
        "prores" => "ProRes",
        "aac" => "AAC",
        "mp1" => "MP1",
        "mp2" => "MP2",
        "mp3" => "MP3",
        "ac3" => "AC-3",
        "eac3" => "E-AC-3",
        "dts" | "dca" => "DTS",
        "truehd" => "TrueHD",
        "mlp" => "MLP",
        "flac" => "FLAC",
        "alac" => "ALAC",
        "opus" => "Opus",
        "vorbis" => "Vorbis",
        "ape" => "APE",
        "wavpack" => "WavPack",
        "tta" => "TTA",
        "tak" => "TAK",
        "mpc7" | "mpc8" => "Musepack",
        "cook" | "ralf" | "sipr" | "atrac3" => "RealAudio",
        "speex" => "Speex",
        "shorten" => "Shorten",
        "amr_nb" | "amrnb" => "AMR",
        "amr_wb" | "amrwb" => "AMR-WB",
        c if c.starts_with("dsd_") => "DSD",
        c if c.starts_with("pcm_") => "PCM",
        c => return c.to_ascii_uppercase(),
    };
    name.to_string()
}

fn is_lossless(codec: &str) -> bool {
    matches!(
        codec,
        "flac" | "alac" | "ape" | "wavpack" | "tta" | "tak" | "truehd" | "mlp" | "shorten" | "ralf"
    ) || codec.starts_with("pcm_")
        || codec.starts_with("dsd_")
}

/// Stream properties of a video Media Foundation can't open.
pub fn video_meta(path: &Path) -> Option<VideoMeta> {
    let probe = Probe::open(path)?;
    // RealMedia and the like are often audio only: still worth the duration and codec.
    let video = probe.track("video");
    let audio = probe.track("audio");
    if video.is_none() && audio.is_none() {
        return None;
    }
    let video_prop = |name: &str| video.as_ref().map(|v| format!("{v}{name}"));
    let audio_prop = |name: &str| {
        audio
            .as_ref()
            .and_then(|a| probe.u32(&format!("{a}{name}")))
    };
    Some(VideoMeta {
        width: video_prop("demux-w")
            .and_then(|p| probe.u32(&p))
            .unwrap_or(0),
        height: video_prop("demux-h")
            .and_then(|p| probe.u32(&p))
            .unwrap_or(0),
        duration_sec: probe.duration(),
        frame_rate: video_prop("demux-fps")
            .and_then(|p| probe.f64(&p))
            .unwrap_or(0.0)
            .max(0.0),
        codec: video_prop("codec")
            .and_then(|p| probe.string(&p))
            .map(|c| codec_name(&c)),
        bitrate_kbps: probe.bitrate_kbps(),
        audio_codec: audio
            .as_ref()
            .and_then(|a| probe.string(&format!("{a}codec")))
            .map(|c| codec_name(&c)),
        audio_channels: audio_prop("demux-channel-count"),
        audio_sample_rate: audio_prop("demux-samplerate"),
    })
}

/// Stream properties of audio neither symphonia nor Media Foundation can open.
pub fn audio_meta(path: &Path) -> Option<AudioStreamMeta> {
    let probe = Probe::open(path)?;
    let audio = probe.track("audio")?;
    let codec = probe.string(&format!("{audio}codec"));
    Some(AudioStreamMeta {
        lossless: codec.as_deref().map(is_lossless),
        codec: codec.as_deref().map(codec_name),
        duration_sec: probe.duration(),
        bitrate_kbps: probe.bitrate_kbps(),
        sample_rate: probe.u32(&format!("{audio}demux-samplerate")),
        channels: probe.u32(&format!("{audio}demux-channel-count")),
        bit_depth: None,
    })
}

/// Observed properties, told apart by their reply id.
const OBSERVED: &[(u64, &str, c_int)] = &[
    (1, "time-pos", FORMAT_NONE),
    (2, "pause", FORMAT_NONE),
    (3, "duration", FORMAT_NONE),
    (4, "volume", FORMAT_NONE),
    (5, "mute", FORMAT_NONE),
    (6, "eof-reached", FORMAT_FLAG),
];

/// A frame step that shows nothing (the end of the file) stops holding back the next one.
const STEP_TIMEOUT: Duration = Duration::from_millis(500);

/// Position updates are posted at most this often (mpv reports every frame).
const TIME_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// What the viewer thread and the event thread share.
struct Shared {
    stop: AtomicBool,
    /// A seek was sent and mpv hasn't finished it yet.
    seeking: AtomicBool,
    /// Error code (`MF_MEDIA_ENGINE_ERR`) of the last failed file, 0 if none.
    error: AtomicU16,
    /// A frame step was sent and its frame isn't on screen yet.
    stepping: AtomicBool,
}

/// `Handle` is thread-safe by libmpv's contract.
struct SendHandle(Handle);
unsafe impl Send for SendHandle {}

/// Field order doesn't matter: `Drop` stops the event thread before destroying the handle.
pub struct MpvPlayer {
    api: &'static Api,
    handle: Handle,
    shared: Arc<Shared>,
    events: Option<JoinHandle<()>>,
    /// Surface size, the OSD's coordinate space.
    size: std::cell::Cell<(i32, i32)>,
    /// OSD text on screen, as sent to mpv.
    osd: std::cell::RefCell<Option<String>>,
    /// When the pending frame step was sent.
    step_sent: std::cell::Cell<Option<Instant>>,
}

/// Both kinds of players.
const PLAYER_OPTIONS: &[(&str, &str)] = &[
    ("idle", "yes"),
    // Stay on the last frame at the end, like the media engine.
    ("keep-open", "yes"),
    ("volume-max", "100"),
];

impl MpvPlayer {
    /// A video player rendering into `surface` and notifying `events_to`.
    pub unsafe fn new(surface: HWND, events_to: HWND) -> Option<Self> {
        let api = api()?;
        let wid = (surface.0 as isize).to_string();
        let cache = crate::config::cache_dir("mpv_shader_cache");
        let _ = std::fs::create_dir_all(&cache);
        let cache = cache.to_string_lossy();
        let video = [
            ("wid", wid.as_str()),
            ("gpu-shader-cache-dir", &cache),
            // Clicks and the cursor belong to the viewer (the surface is transparent to them).
            ("input-cursor", "no"),
            ("input-cursor-passthrough", "yes"),
            ("cursor-autohide", "no"),
            ("osd-level", "0"),
            ("osd-bar", "no"),
            ("vo", "gpu-next"),
            ("hwdec", "auto-safe"),
            ("gpu-shader-cache", "yes"),
            ("screenshot-sw", "no"),
        ];
        let handle = create_handle(api, &[PLAYER_OPTIONS, &video].concat())?;
        Self::start(api, handle, events_to)
    }

    /// A sound-only player (no window, even for a file's cover art) notifying `events_to`.
    pub unsafe fn new_audio(events_to: HWND) -> Option<Self> {
        let api = api()?;
        let audio = [("vid", "no"), ("audio-display", "no")];
        let handle = create_handle(api, &[PLAYER_OPTIONS, &audio].concat())?;
        Self::start(api, handle, events_to)
    }

    unsafe fn start(api: &'static Api, handle: Handle, events_to: HWND) -> Option<Self> {
        for &(id, name, format) in OBSERVED {
            (api.observe_property)(handle, id, cstr(name).as_ptr(), format);
        }
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            seeking: AtomicBool::new(false),
            error: AtomicU16::new(0),
            stepping: AtomicBool::new(false),
        });
        let events = {
            let shared = shared.clone();
            let h = SendHandle(handle);
            let target = events_to.0 as isize;
            std::thread::Builder::new()
                .name("mpv events".into())
                .spawn(move || {
                    let h = h;
                    event_loop(api, h.0, &shared, target)
                })
        };
        let Ok(events) = events else {
            (api.terminate_destroy)(handle);
            return None;
        };
        Some(Self {
            api,
            handle,
            shared,
            events: Some(events),
            size: std::cell::Cell::new((1, 1)),
            osd: std::cell::RefCell::new(None),
            step_sent: std::cell::Cell::new(None),
        })
    }

    fn command(&self, args: &[&str]) -> bool {
        unsafe { command(self.api, self.handle, args) }
    }

    fn get_string(&self, name: &str) -> Option<String> {
        let mut value: *mut c_char = std::ptr::null_mut();
        unsafe {
            let r = (self.api.get_property)(
                self.handle,
                cstr(name).as_ptr(),
                FORMAT_STRING,
                &mut value as *mut *mut c_char as *mut c_void,
            );
            if r < 0 || value.is_null() {
                return None;
            }
            let text = CStr::from_ptr(value).to_string_lossy().trim().to_string();
            (self.api.free)(value as *mut c_void);
            Some(text).filter(|t| !t.is_empty())
        }
    }

    fn set(&self, name: &str, value: &str) {
        unsafe {
            (self.api.set_property_string)(self.handle, cstr(name).as_ptr(), cstr(value).as_ptr());
        }
    }

    fn get_f64(&self, name: &str) -> Option<f64> {
        let mut v = 0f64;
        let r = unsafe {
            (self.api.get_property)(
                self.handle,
                cstr(name).as_ptr(),
                FORMAT_DOUBLE,
                &mut v as *mut f64 as *mut c_void,
            )
        };
        (r >= 0 && v.is_finite()).then_some(v)
    }

    fn get_i64(&self, name: &str) -> Option<i64> {
        let mut v = 0i64;
        let r = unsafe {
            (self.api.get_property)(
                self.handle,
                cstr(name).as_ptr(),
                FORMAT_INT64,
                &mut v as *mut i64 as *mut c_void,
            )
        };
        (r >= 0).then_some(v)
    }

    fn get_flag(&self, name: &str) -> bool {
        let mut v: c_int = 0;
        let r = unsafe {
            (self.api.get_property)(
                self.handle,
                cstr(name).as_ptr(),
                FORMAT_FLAG,
                &mut v as *mut c_int as *mut c_void,
            )
        };
        r >= 0 && v != 0
    }

    /// Opens `path` and starts playback (asynchronous; errors arrive as an ERROR event).
    pub fn open(&self, path: &Path) -> bool {
        self.shared.error.store(0, Ordering::SeqCst);
        self.shared.seeking.store(false, Ordering::SeqCst);
        self.set("pause", "no");
        self.command(&["loadfile", &path.to_string_lossy()])
    }

    /// mpv presents frames itself; this only keeps the OSD text up to date.
    pub fn render(&self, osd: Option<Osd<'_>>) {
        let text = osd.map(|o| self.ass_text(&o));
        let mut shown = self.osd.borrow_mut();
        if *shown == text {
            return;
        }
        let (w, h) = self.size.get();
        match &text {
            Some(t) => {
                let (w, h) = (w.to_string(), h.to_string());
                self.command(&["osd-overlay", "1", "ass-events", t, &w, &h]);
            }
            None => {
                self.command(&["osd-overlay", "1", "none", ""]);
            }
        }
        *shown = text;
    }

    /// The OSD line as an ASS event, looking like the GDI one: top left, 1 px black shadow.
    fn ass_text(&self, osd: &Osd<'_>) -> String {
        use windows::Win32::Graphics::Gdi::{
            GetDC, GetObjectW, GetTextMetricsW, ReleaseDC, SelectObject, FW_BOLD, LOGFONTW,
            TEXTMETRICW,
        };
        let mut lf = LOGFONTW::default();
        let mut tm = TEXTMETRICW::default();
        unsafe {
            GetObjectW(
                osd.font.into(),
                std::mem::size_of::<LOGFONTW>() as i32,
                Some(&mut lf as *mut _ as *mut c_void),
            );
            let dc = GetDC(None);
            let old = SelectObject(dc, osd.font.into());
            let _ = GetTextMetricsW(dc, &mut tm);
            SelectObject(dc, old);
            ReleaseDC(None, dc);
        }
        let face_len = lf.lfFaceName.iter().position(|&c| c == 0).unwrap_or(0);
        let face = String::from_utf16_lossy(&lf.lfFaceName[..face_len]);
        // libass `\fs` is the line height (win ascent + descent), not the em size GDI fonts are
        // created with: that is the GDI cell height.
        let size = if tm.tmHeight > 0 {
            tm.tmHeight
        } else {
            lf.lfHeight.abs()
        }
        .max(1);
        let bold = (lf.lfWeight >= FW_BOLD.0 as i32) as u8;
        let mut escaped = String::with_capacity(osd.text.len());
        for c in osd.text.chars() {
            match c {
                '\n' => escaped.push_str("\\N"),
                '\r' => {}
                '{' => escaped.push_str("\\{"),
                '}' => escaped.push_str("\\}"),
                // Keeps "\N", "\h"... in the text literal.
                '\\' => escaped.push_str("\\\u{2060}"),
                c => escaped.push(c),
            }
        }
        format!(
            "{{\\an7\\pos(10,10)\\fn{}\\fs{}\\b{}\\c&H{:06X}&\\bord0\\shad1\\4c&H000000&\\4a&H00&}}{}",
            face,
            size,
            bold,
            osd.color & 0xFF_FFFF,
            escaped
        )
    }

    /// mpv follows the surface's size on its own; the size is kept for the OSD.
    pub fn resize(&self, width: i32, height: i32) {
        if self.size.replace((width.max(1), height.max(1))) != (width, height) {
            // Re-send the OSD in the new coordinate space.
            self.osd.borrow_mut().take();
        }
    }

    /// Display size (aspect ratio and rotation applied) once the video is decoded.
    pub fn native_size(&self) -> Option<(u32, u32)> {
        let w = self.get_i64("dwidth")?;
        let h = self.get_i64("dheight")?;
        (w > 0 && h > 0).then_some((w as u32, h as u32))
    }

    /// The current frame at its native size, as top-down BGRA rows.
    pub fn capture_frame(&self) -> Option<(u32, u32, Vec<u8>)> {
        unsafe { screenshot(self.api, self.handle) }
    }

    /// Fills in what `tags` lacks from the loaded file (after `LOADEDMETADATA`): mpv reads the
    /// tags and stream parameters of formats the tag reader doesn't know (APE, WavPack, DSD...).
    pub fn fill_tags(&self, tags: &mut AudioTags) {
        let meta = |key: &str| self.get_string(&format!("metadata/by-key/{key}"));
        let first = |keys: &[&str]| keys.iter().find_map(|k| meta(k));
        // "3/12" → 3; "2006-05-01" → 2006.
        let leading_number = |v: String| -> Option<u32> {
            let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok().filter(|&n| n > 0)
        };
        let fill = |field: &mut Option<String>, keys: &[&str]| {
            if field.is_none() {
                *field = first(keys);
            }
        };
        fill(&mut tags.title, &["title"]);
        // RealMedia calls the artist "author".
        fill(&mut tags.artist, &["artist", "author"]);
        fill(&mut tags.album, &["album"]);
        fill(&mut tags.album_artist, &["album_artist", "album artist"]);
        fill(&mut tags.genre, &["genre"]);
        tags.year = tags
            .year
            .or_else(|| first(&["date", "year"]).and_then(leading_number));
        tags.track = tags
            .track
            .or_else(|| first(&["track"]).and_then(leading_number));
        let positive = |name: &str| self.get_i64(name).filter(|&v| v > 0);
        tags.sample_rate = tags
            .sample_rate
            .or_else(|| positive("audio-params/samplerate").and_then(|v| u32::try_from(v).ok()));
        tags.channels = tags
            .channels
            .or_else(|| positive("audio-params/channel-count").and_then(|v| u8::try_from(v).ok()));
        let duration = self.duration();
        if tags.duration_sec <= 0.0 {
            tags.duration_sec = duration;
        }
        if tags.bitrate_kbps.is_none() && duration > 0.0 {
            tags.bitrate_kbps = positive("file-size")
                .map(|bytes| (bytes as f64 * 8.0 / duration / 1000.0).round() as u32);
        }
    }

    pub fn set_rate(&self, rate: f64) {
        self.set("speed", &rate.to_string());
    }

    pub fn is_paused(&self) -> bool {
        self.get_flag("pause") || self.get_flag("eof-reached")
    }

    pub fn is_seeking(&self) -> bool {
        self.shared.seeking.load(Ordering::SeqCst) || self.get_flag("seeking")
    }

    /// One frame forward / back, ending paused. mpv adds up steps sent before the last one is
    /// shown, and key auto-repeat outruns a slow decoder: those are dropped, so the picture
    /// stops as soon as the key is released (at the end of the file no frame comes, hence the
    /// time limit).
    pub fn frame_step(&self, forward: bool) {
        let now = Instant::now();
        if self.shared.stepping.load(Ordering::SeqCst)
            && self.step_sent.get().is_some_and(|t| now - t < STEP_TIMEOUT)
        {
            return;
        }
        self.shared.stepping.store(true, Ordering::SeqCst);
        self.step_sent.set(Some(now));
        self.command(&[if forward {
            "frame-step"
        } else {
            "frame-back-step"
        }]);
    }

    /// Error code (`MF_MEDIA_ENGINE_ERR`), if the file failed.
    pub fn error_code(&self) -> Option<u16> {
        Some(self.shared.error.load(Ordering::SeqCst)).filter(|&c| c != 0)
    }
}

unsafe fn read_screenshot(node: &Node) -> Option<(u32, u32, Vec<u8>)> {
    if node.format != FORMAT_NODE_MAP {
        return None;
    }
    let list = &*node.u.list;
    let (mut w, mut h, mut stride, mut data, mut format) = (0i64, 0i64, 0i64, None, None);
    for i in 0..list.num.max(0) as usize {
        let key = CStr::from_ptr(*list.keys.add(i)).to_bytes();
        let value = &*list.values.add(i);
        match (key, value.format) {
            (b"w", FORMAT_INT64) => w = value.u.int64,
            (b"h", FORMAT_INT64) => h = value.u.int64,
            (b"stride", FORMAT_INT64) => stride = value.u.int64,
            (b"format", FORMAT_STRING) => {
                format = Some(CStr::from_ptr(value.u.string).to_bytes().to_vec())
            }
            (b"data", FORMAT_BYTE_ARRAY) => data = Some(&*value.u.ba),
            _ => {}
        }
    }
    // "bgr0" / "bgra": already the BGRA byte order.
    let format = format?;
    if w <= 0 || h <= 0 || stride < w * 4 || !(format == b"bgr0" || format == b"bgra") {
        return None;
    }
    let data = data?;
    let (w, h, stride) = (w as usize, h as usize, stride as usize);
    if data.size < stride * (h - 1) + w * 4 {
        return None;
    }
    let bytes = std::slice::from_raw_parts(data.data as *const u8, data.size);
    let mut pixels = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        pixels.extend_from_slice(&bytes[y * stride..y * stride + w * 4]);
    }
    pixels.chunks_exact_mut(4).for_each(|px| px[3] = 255);
    Some((w as u32, h as u32, pixels))
}

/// Reads mpv's events until the player is dropped, posting them to the viewer as media engine
/// events.
fn event_loop(api: &'static Api, handle: Handle, shared: &Shared, target: isize) {
    let post = |event: MF_MEDIA_ENGINE_EVENT| unsafe {
        let _ = PostMessageW(
            Some(HWND(target as *mut _)),
            WM_MEDIA_EVENT,
            WPARAM(event.0 as usize),
            LPARAM(0),
        );
    };
    let mut last_time_update: Option<Instant> = None;
    while !shared.stop.load(Ordering::SeqCst) {
        let event = unsafe { &*(api.wait_event)(handle, -1.0) };
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        match event.event_id {
            EVENT_NONE => {}
            EVENT_SHUTDOWN => break,
            EVENT_FILE_LOADED => post(MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA),
            EVENT_VIDEO_RECONFIG => post(MF_MEDIA_ENGINE_EVENT_FORMATCHANGE),
            EVENT_PLAYBACK_RESTART => {
                shared.seeking.store(false, Ordering::SeqCst);
                shared.stepping.store(false, Ordering::SeqCst);
                post(MF_MEDIA_ENGINE_EVENT_SEEKED);
            }
            EVENT_END_FILE => {
                let end = unsafe { &*(event.data as *const EventEndFile) };
                if end.reason == END_FILE_ERROR {
                    // Everything mpv can't open reads as "format or codec not supported"; other
                    // failures as decoding errors.
                    let code = match end.error {
                        -13 | -16 | -17 => 4,
                        _ => 3,
                    };
                    shared.error.store(code, Ordering::SeqCst);
                    post(MF_MEDIA_ENGINE_EVENT_ERROR);
                }
            }
            EVENT_PROPERTY_CHANGE => match event.reply_userdata {
                1 => {
                    shared.stepping.store(false, Ordering::SeqCst);
                    if last_time_update.is_none_or(|t| t.elapsed() >= TIME_UPDATE_INTERVAL) {
                        last_time_update = Some(Instant::now());
                        post(MF_MEDIA_ENGINE_EVENT_TIMEUPDATE);
                    }
                }
                2 => post(MF_MEDIA_ENGINE_EVENT_PAUSE),
                3 => post(MF_MEDIA_ENGINE_EVENT_DURATIONCHANGE),
                4 | 5 => post(MF_MEDIA_ENGINE_EVENT_VOLUMECHANGE),
                6 => {
                    let p = unsafe { &*(event.data as *const EventProperty) };
                    if p.format == FORMAT_FLAG && unsafe { *(p.data as *const c_int) } != 0 {
                        post(MF_MEDIA_ENGINE_EVENT_ENDED);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        unsafe { (self.api.wakeup)(self.handle) };
        if let Some(events) = self.events.take() {
            let _ = events.join();
        }
        unsafe { (self.api.terminate_destroy)(self.handle) };
    }
}

/// mpv's volume is perceptual (cubed into the gain), the media engine's linear: the viewer's
/// volume stays the engine's, so the saved level sounds the same with either.
fn mpv_volume(linear: f64) -> f64 {
    linear.clamp(0.0, 1.0).cbrt() * 100.0
}

impl Transport for MpvPlayer {
    fn is_playing(&self) -> bool {
        !self.is_paused()
    }

    fn play(&self) {
        if self.get_flag("eof-reached") {
            self.seek(0.0, false);
        }
        self.set("pause", "no");
    }

    fn pause(&self) {
        self.set("pause", "yes");
    }

    fn position(&self) -> f64 {
        self.get_f64("time-pos").unwrap_or(0.0).max(0.0)
    }

    fn duration(&self) -> f64 {
        self.get_f64("duration").unwrap_or(0.0).max(0.0)
    }

    fn seek(&self, seconds: f64, approximate: bool) {
        let t = seconds.clamp(0.0, self.duration().max(0.0));
        let flags = if approximate {
            "absolute+keyframes"
        } else {
            "absolute+exact"
        };
        if self.command(&["seek", &format!("{t:.6}"), flags]) {
            self.shared.seeking.store(true, Ordering::SeqCst);
        }
    }

    fn volume(&self) -> f64 {
        (self.get_f64("volume").unwrap_or(100.0) / 100.0)
            .clamp(0.0, 1.0)
            .powi(3)
    }

    fn set_volume(&self, volume: f64) {
        self.set("volume", &format!("{:.3}", mpv_volume(volume)));
    }

    fn is_muted(&self) -> bool {
        self.get_flag("mute")
    }

    fn set_muted(&self, muted: bool) {
        self.set("mute", if muted { "yes" } else { "no" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_round_trips() {
        for v in [0.0, 0.125, 0.5, 1.0] {
            let back = (mpv_volume(v) / 100.0).powi(3);
            assert!((back - v).abs() < 1e-9, "{v} -> {back}");
        }
        assert!((mpv_volume(0.125) - 50.0).abs() < 1e-9);
    }
}
