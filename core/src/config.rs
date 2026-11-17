//! Shared INI location for standalone WDX and the combined viewer/content plugin.

use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::WindowsProgramming::GetPrivateProfileStringW;

static TC_INI_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Called by either ContentSetDefaultParams or ListSetDefaultParams.
pub fn set_tc_ini_path(ini: &Path) {
    if let Some(dir) = ini.parent() {
        *TC_INI_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.to_path_buf());
    }
}

pub fn tc_ini_dir() -> Option<PathBuf> {
    TC_INI_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Resolve this code's DLL, even when loaded only as a WDX without DllMain state.
pub fn dll_path() -> Option<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let mut module = HMODULE::default();
        unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(dll_path as *const () as *const u16),
                &mut module,
            )
            .ok()?;
        }
        let mut buf = [0u16; 32768];
        let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
        (len > 0 && len < buf.len())
            .then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..len])))
    })
    .clone()
}

/// An existing portable INI beside the DLL wins; otherwise use TC's plugin INI directory.
pub fn ini_path() -> PathBuf {
    let portable = dll_path().map(|p| p.with_file_name("mediares.ini"));
    if let Some(p) = portable.as_ref().filter(|p| p.exists()) {
        return p.clone();
    }
    tc_ini_dir()
        .map(|d| d.join("mediares.ini"))
        .or(portable)
        .unwrap_or_else(|| PathBuf::from("mediares.ini"))
}

const MAX_PREFETCH_THREADS: usize = 16;

pub(crate) fn default_prefetch_threads() -> usize {
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    available.saturating_sub(1).clamp(1, 8)
}

pub(crate) fn prefetch_threads() -> usize {
    read_prefetch_threads(&ini_path())
}

fn read_prefetch_threads(path: &Path) -> usize {
    let mut buf = [0u16; 32];
    let len = unsafe {
        GetPrivateProfileStringW(
            w!("Settings"),
            w!("PrefetchThreads"),
            w!(""),
            Some(&mut buf),
            &HSTRING::from(path.as_os_str()),
        )
    };
    String::from_utf16_lossy(&buf[..len as usize])
        .trim()
        .parse::<usize>()
        .unwrap_or_else(|_| default_prefetch_threads())
        .min(MAX_PREFETCH_THREADS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefetch_setting_accepts_zero_and_defaults_on_invalid_values() {
        let dir =
            std::env::temp_dir().join(format!("mediares_prefetch_ini_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let default = default_prefetch_threads();
        for (i, (value, expected)) in [
            ("", default),
            ("PrefetchThreads=0", 0),
            ("PrefetchThreads=7", 7),
            ("PrefetchThreads=99", MAX_PREFETCH_THREADS),
            ("PrefetchThreads=-1", default),
            ("PrefetchThreads=bad", default),
        ]
        .into_iter()
        .enumerate()
        {
            let path = dir.join(format!("{i}.ini"));
            std::fs::write(&path, format!("[Settings]\r\n{value}\r\n")).unwrap();
            assert_eq!(read_prefetch_threads(&path), expected, "{value}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
