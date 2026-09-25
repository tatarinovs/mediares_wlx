//! Viewer configuration stored in `mediares.ini`.
//!
//! Location: next to the DLL if a `mediares.ini` already exists there (portable installs),
//! otherwise in the directory of TC's plugin INI passed via `ListSetDefaultParams` — the plugin
//! folder is often read-only (Program Files).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::core::{w, HSTRING, PCWSTR};

use crate::playlist::{QueueOptions, Repeat};
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
    let tc_dir = TC_INI_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    tc_dir.map(|d| d.join(INI_NAME)).or(portable).unwrap_or_else(|| PathBuf::from(INI_NAME))
}

fn dll_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetModuleFileNameW(Some(crate::module().into()), &mut buf) } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..len])).parent().map(Path::to_path_buf)
}

pub const LOUPE_SCALE_RANGE: (f32, f32) = (1.0, 5.0);
pub const FONT_SIZE_RANGE: (i32, i32) = (8, 72);

#[derive(Debug, Clone, PartialEq)]
pub struct ViewerConfig {
    pub start_fullscreen: bool,
    pub show_osd: bool,
    pub osd_font_size: i32,
    /// COLORREF (0x00BBGGRR)
    pub osd_font_color: u32,
    pub osd_font_name: String,
    pub auto_rotate_exif: bool,
    pub loupe_scale: f32,
    pub queue: QueueOptions,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            start_fullscreen: false,
            show_osd: true,
            osd_font_size: 14,
            osd_font_color: 0x0000FF00,
            osd_font_name: "Segoe UI".to_string(),
            auto_rotate_exif: true,
            loupe_scale: 1.0,
            queue: QueueOptions::default(),
        }
    }
}

impl ViewerConfig {
    pub fn load() -> Self {
        let ini = HSTRING::from(ini_path().as_os_str());
        let d = Self::default();
        let int = |key: PCWSTR, default: i32| unsafe { GetPrivateProfileIntW(SECTION, key, default, &ini) };
        let string = |key: PCWSTR, default: &str| {
            let mut buf = [0u16; 256];
            let len = unsafe { GetPrivateProfileStringW(SECTION, key, &HSTRING::from(default), Some(&mut buf), &ini) };
            String::from_utf16_lossy(&buf[..len as usize])
        };

        let font_name = string(w!("OSDFontName"), &d.osd_font_name);
        let loupe = string(w!("LoupeScale"), "1.0").trim().replace(',', ".").parse::<f32>().ok();

        Self {
            start_fullscreen: int(w!("StartFullscreen"), d.start_fullscreen as i32) != 0,
            show_osd: int(w!("ShowOSD"), d.show_osd as i32) != 0,
            osd_font_size: int(w!("OSDFontSize"), d.osd_font_size).clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1),
            osd_font_color: int(w!("OSDFontColor"), d.osd_font_color as i32) as u32 & 0x00FF_FFFF,
            osd_font_name: if font_name.trim().is_empty() { d.osd_font_name } else { font_name },
            auto_rotate_exif: int(w!("AutoRotateExif"), d.auto_rotate_exif as i32) != 0,
            loupe_scale: loupe.map_or(d.loupe_scale, |v| v.clamp(LOUPE_SCALE_RANGE.0, LOUPE_SCALE_RANGE.1)),
            queue: QueueOptions {
                auto_advance: int(w!("AutoAdvance"), d.queue.auto_advance as i32) != 0,
                repeat: Repeat::from_index(int(w!("Repeat"), d.queue.repeat.index())),
                shuffle: int(w!("Shuffle"), d.queue.shuffle as i32) != 0,
            },
        }
    }

    /// Writes all settings; returns false if any write failed (e.g. read-only location).
    pub fn save(&self) -> bool {
        let ini = HSTRING::from(ini_path().as_os_str());
        let write = |key: PCWSTR, value: String| unsafe {
            WritePrivateProfileStringW(SECTION, key, &HSTRING::from(value), &ini).is_ok()
        };
        let flag = |b: bool| if b { "1" } else { "0" }.to_string();

        [
            write(w!("StartFullscreen"), flag(self.start_fullscreen)),
            write(w!("ShowOSD"), flag(self.show_osd)),
            write(w!("OSDFontSize"), self.osd_font_size.to_string()),
            write(w!("OSDFontColor"), self.osd_font_color.to_string()),
            write(w!("OSDFontName"), self.osd_font_name.clone()),
            write(w!("AutoRotateExif"), flag(self.auto_rotate_exif)),
            write(w!("LoupeScale"), format!("{:.1}", self.loupe_scale)),
            write(w!("AutoAdvance"), flag(self.queue.auto_advance)),
            write(w!("Repeat"), self.queue.repeat.index().to_string()),
            write(w!("Shuffle"), flag(self.queue.shuffle)),
        ]
        .iter()
        .all(|&ok| ok)
    }
}
