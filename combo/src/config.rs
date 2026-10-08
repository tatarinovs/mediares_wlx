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
    GetPrivateProfileIntW, GetPrivateProfileSectionW, WritePrivateProfileStringW,
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

/// A file or folder that only makes sense on this machine (shader cache, video positions,
/// wallpaper), in `%LOCALAPPDATA%\mediares` — a portable TC carries just `mediares.ini`.
/// The parent folder is created on demand.
pub fn local_path(name: &str) -> PathBuf {
    let path = match std::env::var_os("LOCALAPPDATA") {
        Some(dir) => PathBuf::from(dir).join("mediares").join(name),
        None => ini_path().with_file_name(name),
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    path
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
pub const RESUME_THRESHOLD_RANGE: (i32, i32) = (10, 3600);
pub const CONTACT_SHEET_GRID_RANGE: (i32, i32) = (1, 10);

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
    /// Zoomed in, the next photo shows the same place at the same zoom (comparing a series).
    pub keep_zoom: bool,
    /// Navigation passes over RAW files whose JPEG (or HEIC) of the same name is in the folder.
    pub skip_raw_twins: bool,
    /// Ask before Del moves the file to the Recycle Bin.
    pub confirm_delete: bool,
    /// Long videos continue where they were left.
    pub resume_video: bool,
    /// Audio plays at the loudness its ReplayGain tags ask for.
    pub replay_gain: bool,
    /// Left / Right arrow step, seconds.
    pub seek_step_sec: u32,
    pub frame_format: PictureFormat,
    /// Videos shorter than this (seconds) never offer to resume.
    pub resume_threshold_sec: u32,
    /// Grid of the contact sheet (Ctrl+Shift+S).
    pub contact_sheet_columns: u32,
    pub contact_sheet_rows: u32,
    /// The frame under the cursor on the timeline while scrubbing a video.
    pub seek_preview: bool,
    /// The wheel over a photo zooms it; Ctrl+wheel then pages through the files instead of the
    /// other way round.
    pub wheel_zoom: bool,
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
            keep_zoom: false,
            skip_raw_twins: false,
            confirm_delete: true,
            resume_video: true,
            replay_gain: false,
            seek_step_sec: 5,
            frame_format: PictureFormat::Png,
            resume_threshold_sec: crate::resume::DEFAULT_MIN_DURATION_SEC as u32,
            contact_sheet_columns: 4,
            contact_sheet_rows: 4,
            seek_preview: true,
            wheel_zoom: false,
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

/// The `[Settings]` section, read from the INI at once: every `GetPrivateProfile*` call opens and
/// parses the whole file again, which adds up over the ~35 keys on a slow flash drive. Values are
/// read as those calls read them.
struct Section(Vec<(String, String)>);

impl Section {
    fn read(ini: &HSTRING) -> Self {
        // `key=value` entries, each NUL-terminated; the length is the buffer's minus 2 when it
        // was too small.
        let mut buf = vec![0u16; 16 * 1024];
        let len = loop {
            let len = unsafe { GetPrivateProfileSectionW(SECTION, Some(&mut buf), ini) } as usize;
            if len + 2 < buf.len() || buf.len() >= 1 << 20 {
                break len;
            }
            buf = vec![0u16; buf.len() * 4];
        };
        Self::parse(&String::from_utf16_lossy(&buf[..len]))
    }

    /// Entries as the section read returns them: keys and values trimmed, comments left out.
    fn parse(entries: &str) -> Self {
        let pairs = entries
            .split('\0')
            .filter_map(|entry| entry.split_once('='))
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            .collect();
        Self(pairs)
    }

    /// The first value of `key` (any case), without the quotes that enclose it as a whole.
    fn get(&self, key: &str) -> Option<&str> {
        let value = &self.0.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))?.1;
        let quoted = value.len() >= 2
            && (value.starts_with('"') && value.ends_with('"')
                || value.starts_with('\'') && value.ends_with('\''));
        Some(if quoted {
            &value[1..value.len() - 1]
        } else {
            value
        })
    }

    /// As `GetPrivateProfileStringW`: `default` when the key is missing.
    fn string(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_string()
    }

    /// As `GetPrivateProfileIntW`: `default` when the key is missing or empty, the leading number
    /// otherwise (decimal or `0x` hex, 0 if there is none), wrapping like its 32-bit result.
    fn int(&self, key: &str, default: i32) -> i32 {
        let Some(value) = self.get(key).filter(|v| !v.is_empty()) else {
            return default;
        };
        let (negative, digits) = match value.as_bytes()[0] {
            b'-' => (true, &value[1..]),
            b'+' => (false, &value[1..]),
            _ => (false, value),
        };
        let (radix, digits) = match digits.get(..2) {
            Some("0x" | "0X") => (16, &digits[2..]),
            _ => (10, digits),
        };
        let magnitude = digits
            .chars()
            .map_while(|c| c.to_digit(radix))
            .fold(0u32, |n, d| n.wrapping_mul(radix).wrapping_add(d));
        let n = if negative {
            magnitude.wrapping_neg()
        } else {
            magnitude
        };
        n as i32
    }
}

impl ViewerConfig {
    pub fn load() -> Self {
        let ini = HSTRING::from(ini_path().as_os_str());
        let d = Self::default();
        let section = Section::read(&ini);
        let int = |key: &str, default: i32| section.int(key, default);
        let string = |key: &str, default: &str| section.string(key, default);

        let font_name = string("OSDFontName", &d.osd_font_name);
        let loupe = string("LoupeScale", "1.0")
            .trim()
            .replace(',', ".")
            .parse::<f32>()
            .ok();

        Self {
            language: LangSetting::from_ini(&string("Language", "")),
            start_fullscreen: int("StartFullscreen", d.start_fullscreen as i32) != 0,
            // Older configs only had ShowOSD (photos).
            osd: OsdMode::from_index(int("OSDMode", -1)).unwrap_or(if int("ShowOSD", 1) != 0 {
                OsdMode::Photo
            } else {
                OsdMode::Off
            }),
            osd_font_size: int("OSDFontSize", d.osd_font_size)
                .clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1),
            osd_font_color: int("OSDFontColor", d.osd_font_color as i32) as u32 & 0x00FF_FFFF,
            osd_font_name: if font_name.trim().is_empty() {
                d.osd_font_name
            } else {
                font_name
            },
            auto_rotate_exif: int("AutoRotateExif", d.auto_rotate_exif as i32) != 0,
            loupe_scale: loupe.map_or(d.loupe_scale, |v| {
                v.clamp(LOUPE_SCALE_RANGE.0, LOUPE_SCALE_RANGE.1)
            }),
            queue: QueueOptions {
                auto_advance: int("AutoAdvance", d.queue.auto_advance as i32) != 0,
                repeat: Repeat::from_index(int("Repeat", d.queue.repeat.index())),
                shuffle: int("Shuffle", d.queue.shuffle as i32) != 0,
            },
            overlay_photo: int("OverlayPhoto", d.overlay_photo as i32) != 0,
            overlay_video: int("OverlayVideo", d.overlay_video as i32) != 0,
            overlay_autohide: int("OverlayAutoHide", d.overlay_autohide as i32) != 0,
            slideshow_seconds: int("SlideshowSeconds", d.slideshow_seconds as i32).clamp(1, 3600)
                as u32,
            photo_background: int("PhotoBackground", d.photo_background as i32) as u32
                & 0x00FF_FFFF,
            no_upscale: int("NoUpscale", d.no_upscale as i32) != 0,
            smooth_zoom: int("SmoothZoom", d.smooth_zoom as i32) != 0,
            keep_zoom: int("KeepZoom", d.keep_zoom as i32) != 0,
            skip_raw_twins: int("SkipRawTwins", d.skip_raw_twins as i32) != 0,
            confirm_delete: int("ConfirmDelete", d.confirm_delete as i32) != 0,
            resume_video: int("ResumeVideo", d.resume_video as i32) != 0,
            replay_gain: int("ReplayGain", d.replay_gain as i32) != 0,
            seek_step_sec: int("SeekStep", d.seek_step_sec as i32)
                .clamp(SEEK_STEP_RANGE.0, SEEK_STEP_RANGE.1) as u32,
            frame_format: PictureFormat::from_ini(&string("FrameFormat", "")),
            resume_threshold_sec: int("ResumeThreshold", d.resume_threshold_sec as i32)
                .clamp(RESUME_THRESHOLD_RANGE.0, RESUME_THRESHOLD_RANGE.1)
                as u32,
            contact_sheet_columns: int("ContactSheetColumns", d.contact_sheet_columns as i32)
                .clamp(CONTACT_SHEET_GRID_RANGE.0, CONTACT_SHEET_GRID_RANGE.1)
                as u32,
            contact_sheet_rows: int("ContactSheetRows", d.contact_sheet_rows as i32)
                .clamp(CONTACT_SHEET_GRID_RANGE.0, CONTACT_SHEET_GRID_RANGE.1)
                as u32,
            seek_preview: int("SeekPreview", d.seek_preview as i32) != 0,
            wheel_zoom: int("WheelZoom", d.wheel_zoom as i32) != 0,
            photo_editor: string("PhotoEditor", "").trim().to_string(),
            video_editor: string("VideoEditor", "").trim().to_string(),
            audio_editor: string("AudioEditor", "").trim().to_string(),
            photo_osd: template(string("PhotoOSDTemplate", ""), osd_template::DEFAULT_PHOTO),
            video_osd: template(string("VideoOSDTemplate", ""), osd_template::DEFAULT_VIDEO),
        }
    }

    /// The settings as INI key / value pairs.
    fn entries(&self) -> [(PCWSTR, String); 35] {
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
            (w!("KeepZoom"), flag(self.keep_zoom)),
            (w!("SkipRawTwins"), flag(self.skip_raw_twins)),
            (w!("ConfirmDelete"), flag(self.confirm_delete)),
            (w!("ResumeVideo"), flag(self.resume_video)),
            (w!("ReplayGain"), flag(self.replay_gain)),
            (w!("SeekStep"), self.seek_step_sec.to_string()),
            (w!("FrameFormat"), self.frame_format.extension().to_string()),
            (w!("ResumeThreshold"), self.resume_threshold_sec.to_string()),
            (
                w!("ContactSheetColumns"),
                self.contact_sheet_columns.to_string(),
            ),
            (w!("ContactSheetRows"), self.contact_sheet_rows.to_string()),
            (w!("SeekPreview"), flag(self.seek_preview)),
            (w!("WheelZoom"), flag(self.wheel_zoom)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::WindowsProgramming::GetPrivateProfileStringW;

    /// The section read gives what the per-key calls it replaced gave, on the same file.
    #[test]
    fn section_reads_like_the_profile_api() {
        let text = [
            "[Other]",
            "Dup=other section",
            "[settings]",
            "; Comment=1",
            "  Spaced  =   value with spaces   ",
            r#"Quoted="C:\a b\x.exe""#,
            r#"HalfQuoted="C:\a b\x.exe" -n %1"#,
            "Single='abc'",
            "Dup=first",
            "DUP=second",
            "Num1= 12abc",
            "Num2=-5",
            "Num3=0x10",
            "Num4=",
            "Num5=abc",
            "Num6=+7",
            "Num7=4294967295",
            "Num8=16777215",
            "NoEquals",
            "Empty=",
            "Тема=Значение",
        ]
        .join("\r\n");
        let path = std::env::temp_dir().join(format!("mediares_ini_{}.ini", std::process::id()));
        // UTF-16 with a BOM, as the profile API reads non-ASCII text.
        let wide: Vec<u8> = [0xFEFFu16]
            .into_iter()
            .chain(text.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        std::fs::write(&path, wide).unwrap();
        let ini = HSTRING::from(path.as_os_str());
        let section = Section::read(&ini);

        let api_string = |key: &str| {
            let mut buf = [0u16; 512];
            let len = unsafe {
                GetPrivateProfileStringW(
                    SECTION,
                    &HSTRING::from(key),
                    w!("<default>"),
                    Some(&mut buf),
                    &ini,
                )
            };
            String::from_utf16_lossy(&buf[..len as usize])
        };
        let api_int =
            |key: &str| unsafe { GetPrivateProfileIntW(SECTION, &HSTRING::from(key), -99, &ini) };
        let strings = [
            "Spaced",
            "Quoted",
            "HalfQuoted",
            "Single",
            "Dup",
            "dup",
            "Comment",
            "; Comment",
            "NoEquals",
            "Empty",
            "Missing",
            "Тема",
        ];
        let ints = [
            "Num1", "Num2", "Num3", "Num4", "Num5", "Num6", "Num7", "Num8", "Missing", "Spaced",
        ];
        let results: Vec<_> = strings
            .iter()
            .map(|k| (api_string(k), section.string(k, "<default>")))
            .collect();
        let numbers: Vec<_> = ints
            .iter()
            .map(|k| (api_int(k), section.int(k, -99)))
            .collect();
        let _ = std::fs::remove_file(&path);

        for (key, (api, ours)) in strings.iter().zip(results) {
            assert_eq!(api, ours, "string {key}");
        }
        for (key, (api, ours)) in ints.iter().zip(numbers) {
            assert_eq!(api, ours, "int {key}");
        }
    }
}
