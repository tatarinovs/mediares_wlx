//! Viewer configuration stored in `mediares.ini`.
//!
//! Location: next to the DLL if a `mediares.ini` already exists there (portable installs),
//! otherwise in the directory of TC's plugin INI passed via `ListSetDefaultParams` — the plugin
//! folder is often read-only (Program Files).

use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::core::{w, HSTRING, PCWSTR};

use crate::i18n::{tr, LangSetting};
use crate::osd_template;
use crate::playlist::{QueueOptions, Repeat};
use crate::snapshot::PictureFormat;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::WindowsProgramming::{
    GetPrivateProfileIntW, GetPrivateProfileStringW, WritePrivateProfileStringW,
};

const INI_NAME: &str = "mediares.ini";
const SECTION: PCWSTR = w!("Settings");

static TC_INI_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Remembers the directory of TC's default plugin INI (from `ListSetDefaultParams`).
pub fn set_tc_ini_path(ini: &Path) {
    if let Some(dir) = ini.parent() {
        *TC_INI_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.to_path_buf());
    }
}

fn ini_path() -> PathBuf {
    let portable = dll_dir().map(|d| d.join(INI_NAME));
    if let Some(p) = portable.as_ref().filter(|p| p.exists()) {
        return p.clone();
    }
    tc_ini_dir()
        .map(|d| d.join(INI_NAME))
        .or(portable)
        .unwrap_or_else(|| PathBuf::from(INI_NAME))
}

/// A data file kept next to `mediares.ini`.
pub fn data_file(name: &str) -> PathBuf {
    ini_path().with_file_name(name)
}

/// A cache folder tied to this machine (GPU, drivers), in `%LOCALAPPDATA%\mediares`.
pub fn cache_dir(name: &str) -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(dir) => PathBuf::from(dir).join("mediares").join(name),
        None => data_file(name),
    }
}

/// Player volume (0..=1) left by the previous session; full volume if none was saved.
pub fn load_volume() -> f64 {
    let ini = HSTRING::from(ini_path().as_os_str());
    let percent = unsafe { GetPrivateProfileIntW(SECTION, w!("Volume"), 100, &ini) };
    f64::from(percent.clamp(0, 100)) / 100.0
}

pub fn save_volume(volume: f64) {
    let ini = HSTRING::from(ini_path().as_os_str());
    let percent = (volume.clamp(0.0, 1.0) * 100.0).round() as i32;
    unsafe {
        let _ = WritePrivateProfileStringW(
            SECTION,
            w!("Volume"),
            &HSTRING::from(percent.to_string()),
            &ini,
        );
    }
}

/// Directory of TC's default plugin INI — normally the one holding `wincmd.ini` too.
pub fn tc_ini_dir() -> Option<PathBuf> {
    TC_INI_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Full path of this DLL.
pub fn dll_path() -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetModuleFileNameW(Some(crate::module().into()), &mut buf) } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..len])))
}

fn dll_dir() -> Option<PathBuf> {
    dll_path()?.parent().map(Path::to_path_buf)
}

/// Where the on-screen info line is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdMode {
    Off,
    Photo,
    Video,
    Both,
}

impl OsdMode {
    pub const ALL: [OsdMode; 4] = [OsdMode::Off, OsdMode::Photo, OsdMode::Video, OsdMode::Both];

    pub fn from_index(i: i32) -> Option<Self> {
        Self::ALL.get(usize::try_from(i).ok()?).copied()
    }

    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn label(self) -> &'static str {
        match self {
            OsdMode::Off => tr("Hidden"),
            OsdMode::Photo => tr("On photos"),
            OsdMode::Video => tr("On videos"),
            OsdMode::Both => tr("On photos and videos"),
        }
    }

    pub fn photo(self) -> bool {
        matches!(self, OsdMode::Photo | OsdMode::Both)
    }

    pub fn video(self) -> bool {
        matches!(self, OsdMode::Video | OsdMode::Both)
    }

    /// Mode with the photo / video part switched on or off.
    pub fn with(self, photo: bool, video: bool) -> Self {
        match (photo, video) {
            (false, false) => OsdMode::Off,
            (true, false) => OsdMode::Photo,
            (false, true) => OsdMode::Video,
            (true, true) => OsdMode::Both,
        }
    }
}

pub const LOUPE_SCALE_RANGE: (f32, f32) = (1.0, 5.0);
pub const FONT_SIZE_RANGE: (i32, i32) = (8, 72);
pub const SEEK_STEP_RANGE: (i32, i32) = (1, 600);

#[derive(Debug, Clone, PartialEq)]
pub struct ViewerConfig {
    pub language: LangSetting,
    pub start_fullscreen: bool,
    pub osd: OsdMode,
    pub osd_font_size: i32,
    /// COLORREF (0x00BBGGRR)
    pub osd_font_color: u32,
    pub osd_font_name: String,
    pub auto_rotate_exif: bool,
    pub loupe_scale: f32,
    pub queue: QueueOptions,
    /// Fullscreen: ◀ ▶ buttons floating over photos.
    pub overlay_photo: bool,
    /// Fullscreen: the transport bar floats over the video (otherwise it stays below it).
    pub overlay_video: bool,
    /// Fullscreen: those panels hide when idle and reappear when the mouse nears the bottom.
    pub overlay_autohide: bool,
    pub slideshow_seconds: u32,
    /// COLORREF around photos (and under their transparency).
    pub photo_background: u32,
    /// Fit mode shows images smaller than the window at 100% instead of enlarging them.
    pub no_upscale: bool,
    /// Enlarged photos and album art are smoothed (bicubic) instead of showing square pixels.
    pub smooth_zoom: bool,
    /// Ask before Del moves the file to the Recycle Bin.
    pub confirm_delete: bool,
    /// Long videos continue where they were left.
    pub resume_video: bool,
    /// Left / Right arrow step, seconds.
    pub seek_step_sec: u32,
    pub frame_format: PictureFormat,
    /// Programs for "Открыть в редакторе"; empty = the file type's Edit verb or default program.
    pub photo_editor: String,
    pub video_editor: String,
    pub audio_editor: String,
    /// OSD templates (see [`crate::osd_template`]).
    pub photo_osd: String,
    pub video_osd: String,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            language: LangSetting::Auto,
            start_fullscreen: false,
            osd: OsdMode::Photo,
            osd_font_size: 14,
            osd_font_color: 0x0000FF00,
            osd_font_name: "Segoe UI".to_string(),
            auto_rotate_exif: true,
            loupe_scale: 1.0,
            queue: QueueOptions::default(),
            overlay_photo: true,
            overlay_video: true,
            overlay_autohide: true,
            slideshow_seconds: 4,
            photo_background: crate::image_cache::BACKGROUND,
            no_upscale: false,
            smooth_zoom: true,
            confirm_delete: true,
            resume_video: true,
            seek_step_sec: 5,
            frame_format: PictureFormat::Png,
            photo_editor: String::new(),
            video_editor: String::new(),
            audio_editor: String::new(),
            photo_osd: osd_template::DEFAULT_PHOTO.to_string(),
            video_osd: osd_template::DEFAULT_VIDEO.to_string(),
        }
    }
}

/// An INI template; empty means the default.
fn template(value: String, default: &str) -> String {
    let value = osd_template::from_ini(&value);
    if value.trim().is_empty() {
        default.to_string()
    } else {
        value
    }
}

fn template_to_ini(value: &str, default: &str) -> String {
    if value == default {
        String::new()
    } else {
        osd_template::to_ini(value)
    }
}

impl ViewerConfig {
    pub fn load() -> Self {
        let ini = HSTRING::from(ini_path().as_os_str());
        let d = Self::default();
        let int = |key: PCWSTR, default: i32| unsafe {
            GetPrivateProfileIntW(SECTION, key, default, &ini)
        };
        let string = |key: PCWSTR, default: &str| {
            let mut buf = [0u16; 4096];
            let len = unsafe {
                GetPrivateProfileStringW(
                    SECTION,
                    key,
                    &HSTRING::from(default),
                    Some(&mut buf),
                    &ini,
                )
            };
            String::from_utf16_lossy(&buf[..len as usize])
        };

        let font_name = string(w!("OSDFontName"), &d.osd_font_name);
        let loupe = string(w!("LoupeScale"), "1.0")
            .trim()
            .replace(',', ".")
            .parse::<f32>()
            .ok();

        Self {
            language: LangSetting::from_ini(&string(w!("Language"), "")),
            start_fullscreen: int(w!("StartFullscreen"), d.start_fullscreen as i32) != 0,
            // Older configs only had ShowOSD (photos).
            osd: OsdMode::from_index(int(w!("OSDMode"), -1)).unwrap_or(
                if int(w!("ShowOSD"), 1) != 0 {
                    OsdMode::Photo
                } else {
                    OsdMode::Off
                },
            ),
            osd_font_size: int(w!("OSDFontSize"), d.osd_font_size)
                .clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1),
            osd_font_color: int(w!("OSDFontColor"), d.osd_font_color as i32) as u32 & 0x00FF_FFFF,
            osd_font_name: if font_name.trim().is_empty() {
                d.osd_font_name
            } else {
                font_name
            },
            auto_rotate_exif: int(w!("AutoRotateExif"), d.auto_rotate_exif as i32) != 0,
            loupe_scale: loupe.map_or(d.loupe_scale, |v| {
                v.clamp(LOUPE_SCALE_RANGE.0, LOUPE_SCALE_RANGE.1)
            }),
            queue: QueueOptions {
                auto_advance: int(w!("AutoAdvance"), d.queue.auto_advance as i32) != 0,
                repeat: Repeat::from_index(int(w!("Repeat"), d.queue.repeat.index())),
                shuffle: int(w!("Shuffle"), d.queue.shuffle as i32) != 0,
            },
            overlay_photo: int(w!("OverlayPhoto"), d.overlay_photo as i32) != 0,
            overlay_video: int(w!("OverlayVideo"), d.overlay_video as i32) != 0,
            overlay_autohide: int(w!("OverlayAutoHide"), d.overlay_autohide as i32) != 0,
            slideshow_seconds: int(w!("SlideshowSeconds"), d.slideshow_seconds as i32)
                .clamp(1, 3600) as u32,
            photo_background: int(w!("PhotoBackground"), d.photo_background as i32) as u32
                & 0x00FF_FFFF,
            no_upscale: int(w!("NoUpscale"), d.no_upscale as i32) != 0,
            smooth_zoom: int(w!("SmoothZoom"), d.smooth_zoom as i32) != 0,
            confirm_delete: int(w!("ConfirmDelete"), d.confirm_delete as i32) != 0,
            resume_video: int(w!("ResumeVideo"), d.resume_video as i32) != 0,
            seek_step_sec: int(w!("SeekStep"), d.seek_step_sec as i32)
                .clamp(SEEK_STEP_RANGE.0, SEEK_STEP_RANGE.1) as u32,
            frame_format: PictureFormat::from_ini(&string(w!("FrameFormat"), "")),
            photo_editor: string(w!("PhotoEditor"), "").trim().to_string(),
            video_editor: string(w!("VideoEditor"), "").trim().to_string(),
            audio_editor: string(w!("AudioEditor"), "").trim().to_string(),
            photo_osd: template(
                string(w!("PhotoOSDTemplate"), ""),
                osd_template::DEFAULT_PHOTO,
            ),
            video_osd: template(
                string(w!("VideoOSDTemplate"), ""),
                osd_template::DEFAULT_VIDEO,
            ),
        }
    }

    /// The settings as INI key / value pairs.
    fn entries(&self) -> [(PCWSTR, String); 27] {
        let flag = |b: bool| if b { "1" } else { "0" }.to_string();
        [
            (w!("Language"), self.language.to_ini().to_string()),
            (w!("StartFullscreen"), flag(self.start_fullscreen)),
            (w!("OSDMode"), self.osd.index().to_string()),
            (w!("OSDFontSize"), self.osd_font_size.to_string()),
            (w!("OSDFontColor"), self.osd_font_color.to_string()),
            (w!("OSDFontName"), self.osd_font_name.clone()),
            (w!("AutoRotateExif"), flag(self.auto_rotate_exif)),
            (w!("LoupeScale"), format!("{:.1}", self.loupe_scale)),
            (w!("AutoAdvance"), flag(self.queue.auto_advance)),
            (w!("Repeat"), self.queue.repeat.index().to_string()),
            (w!("Shuffle"), flag(self.queue.shuffle)),
            (w!("OverlayPhoto"), flag(self.overlay_photo)),
            (w!("OverlayVideo"), flag(self.overlay_video)),
            (w!("OverlayAutoHide"), flag(self.overlay_autohide)),
            (w!("SlideshowSeconds"), self.slideshow_seconds.to_string()),
            (w!("PhotoBackground"), self.photo_background.to_string()),
            (w!("NoUpscale"), flag(self.no_upscale)),
            (w!("SmoothZoom"), flag(self.smooth_zoom)),
            (w!("ConfirmDelete"), flag(self.confirm_delete)),
            (w!("ResumeVideo"), flag(self.resume_video)),
            (w!("SeekStep"), self.seek_step_sec.to_string()),
            (w!("FrameFormat"), self.frame_format.extension().to_string()),
            (w!("PhotoEditor"), self.photo_editor.clone()),
            (w!("VideoEditor"), self.video_editor.clone()),
            (w!("AudioEditor"), self.audio_editor.clone()),
            // The default is stored as empty, so a changed default reaches users who never edited it.
            (
                w!("PhotoOSDTemplate"),
                template_to_ini(&self.photo_osd, osd_template::DEFAULT_PHOTO),
            ),
            (
                w!("VideoOSDTemplate"),
                template_to_ini(&self.video_osd, osd_template::DEFAULT_VIDEO),
            ),
        ]
    }

    /// Writes the settings that differ from `before` (what was loaded or last saved), so keys
    /// another viewer window or the user changed in the meantime are kept. Returns false if any
    /// write failed (e.g. read-only location).
    pub fn save(&self, before: &ViewerConfig) -> bool {
        let ini = HSTRING::from(ini_path().as_os_str());
        self.entries()
            .into_iter()
            .zip(before.entries())
            .filter(|((_, value), (_, old))| value != old)
            // Every write is attempted, so one failure doesn't drop the rest.
            .filter(|((key, value), _)| unsafe {
                WritePrivateProfileStringW(SECTION, *key, &HSTRING::from(value), &ini).is_err()
            })
            .count()
            == 0
    }
}
