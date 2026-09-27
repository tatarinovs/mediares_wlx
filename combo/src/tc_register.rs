//! Registers this DLL as a content plugin (WDX) in TC's `wincmd.ini`.
//!
//! `pluginst.inf` installs the combo build as a Lister plugin only; its content functions are the
//! same file listed in `[ContentPlugins]`. TC reads that list at startup, so a new entry takes
//! effect after a restart.

use std::path::{Path, PathBuf};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::System::WindowsProgramming::{
    GetPrivateProfileStringW, WritePrivateProfileStringW,
};

use crate::i18n::tr;

const SECTION: PCWSTR = w!("ContentPlugins");

/// Where our entry would go, and whether it is already there.
pub struct Registration {
    /// The INI holding `[ContentPlugins]` (`wincmd.ini`, or the file its `RedirectSection` names).
    ini: PathBuf,
    dll: PathBuf,
    pub registered: bool,
}

impl Registration {
    /// Looks up `wincmd.ini`; `None` if it can't be found (e.g. not running inside TC).
    pub fn find() -> Option<Self> {
        let ini = content_plugins_ini(&wincmd_ini()?);
        let dll = crate::config::dll_path()?;
        let registered = read_section(&ini).iter().any(|(key, value)| {
            plugin_index(key).is_some() && is_ours(value, &dll, |name| std::env::var(name).ok())
        });
        Some(Self {
            ini,
            dll,
            registered,
        })
    }

    /// Adds our entry under the first free number. Err: what to tell the user to add by hand.
    pub fn register(&mut self) -> Result<(), String> {
        let entries = read_section(&self.ini);
        let index = first_free_index(entries.iter().map(|(key, _)| key.as_str()));
        let value = ini_value(&self.dll, std::env::var("COMMANDER_PATH").ok().as_deref());

        let file = HSTRING::from(self.ini.as_os_str());
        // Leftovers of a removed plugin under this number (its cached detect string etc.).
        let stale = format!("{}_", index);
        for (key, _) in entries.iter().filter(|(key, _)| key.starts_with(&stale)) {
            unsafe {
                let _ = WritePrivateProfileStringW(
                    SECTION,
                    &HSTRING::from(key.as_str()),
                    PCWSTR::null(),
                    &file,
                );
            }
        }
        let key = HSTRING::from(index.to_string());
        if unsafe {
            WritePrivateProfileStringW(SECTION, &key, &HSTRING::from(value.as_str()), &file)
        }
        .is_err()
        {
            return Err(format!(
                "{} {}.\n\n{} [ContentPlugins]:\n{}={}",
                tr("Не удалось записать в", "Could not write to"),
                self.ini.display(),
                tr(
                    "Добавьте вручную в секцию",
                    "Add it manually to the section"
                ),
                index,
                value
            ));
        }
        self.registered = true;
        Ok(())
    }
}

/// TC puts its INI path into its own environment; failing that, it sits next to the plugin INI.
pub fn wincmd_ini() -> Option<PathBuf> {
    if let Some(ini) = std::env::var_os("COMMANDER_INI")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
    {
        return Some(ini);
    }
    crate::config::tc_ini_dir()
        .map(|d| d.join("wincmd.ini"))
        .filter(|p| p.is_file())
}

/// Follows `RedirectSection=` (a shared install may keep the plugin list in another file).
fn content_plugins_ini(wincmd: &Path) -> PathBuf {
    let redirect = read_string(wincmd, "RedirectSection");
    if redirect.trim().is_empty() {
        return wincmd.to_path_buf();
    }
    let target = PathBuf::from(expand_vars(redirect.trim(), |name| {
        std::env::var(name).ok()
    }));
    match wincmd.parent() {
        Some(dir) if target.is_relative() => dir.join(target),
        _ => target,
    }
}

fn read_string(ini: &Path, key: &str) -> String {
    let mut buf = [0u16; 2048];
    let (key, file) = (HSTRING::from(key), HSTRING::from(ini.as_os_str()));
    let len = unsafe { GetPrivateProfileStringW(SECTION, &key, w!(""), Some(&mut buf), &file) };
    String::from_utf16_lossy(&buf[..len as usize])
}

/// All `(key, value)` pairs of `[ContentPlugins]`.
fn read_section(ini: &Path) -> Vec<(String, String)> {
    let file = HSTRING::from(ini.as_os_str());
    // With no key name the API returns all key names, each NUL-terminated; retry until they fit.
    let mut buf = vec![0u16; 8192];
    let len = loop {
        let len = unsafe {
            GetPrivateProfileStringW(SECTION, PCWSTR::null(), w!(""), Some(&mut buf), &file)
        } as usize;
        if len + 2 < buf.len() || buf.len() >= 1 << 20 {
            break len;
        }
        buf = vec![0u16; buf.len() * 4];
    };
    buf[..len]
        .split(|&c| c == 0)
        .filter(|key| !key.is_empty())
        .map(|key| {
            let key = String::from_utf16_lossy(key);
            let value = read_string(ini, &key);
            (key, value)
        })
        .collect()
}

/// `N` for a plugin entry `N=path`; other keys (`N_detect`, `RedirectSection`...) are `None`.
fn plugin_index(key: &str) -> Option<u32> {
    key.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| key.parse().ok())
        .flatten()
}

/// The lowest number without a plugin entry (fills gaps left by hand edits).
fn first_free_index<'a>(keys: impl Iterator<Item = &'a str>) -> u32 {
    let mut used: Vec<u32> = keys.filter_map(plugin_index).collect();
    used.sort_unstable();
    let mut free = 0;
    for n in used {
        if n == free {
            free += 1;
        } else if n > free {
            break;
        }
    }
    free
}

/// Replaces `%NAME%` with `lookup(NAME)`; unknown variables stay as written.
fn expand_vars(s: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match lookup(name) {
                    Some(value) if !name.is_empty() => out.push_str(&value),
                    _ => out.push_str(&rest[start..start + end + 2]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn normalize(path: &str) -> String {
    path.trim()
        .trim_matches('"')
        .replace('/', "\\")
        .to_lowercase()
}

/// Whether the entry `value` points at `dll`. A standalone `mediares.wdx64` counts too: it has the
/// same fields, and a second copy would only duplicate them.
fn is_ours(value: &str, dll: &Path, lookup: impl Fn(&str) -> Option<String>) -> bool {
    let entry = normalize(&expand_vars(value, lookup));
    if entry == normalize(&dll.to_string_lossy()) {
        return true;
    }
    let name = entry.rsplit('\\').next().unwrap_or("");
    let own_name = dll
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name == own_name || name == "mediares.wdx64"
}

/// The path as TC itself writes it: relative to `%COMMANDER_PATH%` when inside it (portable installs).
fn ini_value(dll: &Path, commander_path: Option<&str>) -> String {
    let full = dll.to_string_lossy().into_owned();
    if let Some(root) = commander_path
        .map(|r| r.trim_end_matches('\\'))
        .filter(|r| !r.is_empty())
    {
        let prefix = format!("{}\\", root.to_lowercase());
        if full.to_lowercase().starts_with(&prefix) {
            return format!("%COMMANDER_PATH%\\{}", &full[prefix.len()..]);
        }
    }
    full
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        (name == "COMMANDER_PATH").then(|| r"C:\TC".to_string())
    }

    #[test]
    fn only_numeric_keys_are_plugins() {
        assert_eq!(plugin_index("3"), Some(3));
        assert_eq!(plugin_index("3_detect"), None);
        assert_eq!(plugin_index("RedirectSection"), None);
        assert_eq!(plugin_index(""), None);
    }

    #[test]
    fn free_index_fills_gaps() {
        assert_eq!(first_free_index([].into_iter()), 0);
        assert_eq!(
            first_free_index(["0", "0_detect", "1", "1_detect"].into_iter()),
            2
        );
        assert_eq!(first_free_index(["2", "0", "3"].into_iter()), 1);
    }

    #[test]
    fn vars_are_expanded() {
        assert_eq!(
            expand_vars(r"%COMMANDER_PATH%\plugins", env),
            r"C:\TC\plugins"
        );
        assert_eq!(expand_vars("100% sure", env), "100% sure");
        assert_eq!(expand_vars("%NOPE%\\x", env), "%NOPE%\\x");
    }

    #[test]
    fn recognizes_own_entry() {
        let dll = Path::new(r"C:\TC\Plugins\wlx\mediares\mediares.wlx64");
        assert!(is_ours(
            r"%COMMANDER_PATH%\plugins\WLX\mediares\Mediares.wlx64",
            dll,
            env
        ));
        assert!(is_ours(r"D:\other\mediares.wlx64", dll, env));
        assert!(is_ours(
            r"C:\TC\Plugins\wdx\mediares\mediares.wdx64",
            dll,
            env
        ));
        assert!(!is_ours(r"C:\TC\Plugins\wdx\exif\exif.wdx64", dll, env));
    }

    #[test]
    fn registers_in_real_ini() {
        let dir = std::env::temp_dir().join(format!("mediares_tc_register_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wincmd = dir.join("wincmd.ini");
        std::fs::write(
            &wincmd,
            "[ContentPlugins]\r\nRedirectSection=plugins.ini\r\n",
        )
        .unwrap();
        let plugins = dir.join("plugins.ini");
        let list =
            "[ContentPlugins]\r\n0=C:\\x\\exif.wdx64\r\n0_detect=EXT=\"JPG\"\r\n1_detect=old\r\n";
        std::fs::write(&plugins, list).unwrap();

        let ini = content_plugins_ini(&wincmd);
        assert_eq!(ini, plugins);
        let dll = PathBuf::from(r"D:\nowhere\mediares.wlx64");
        let mut reg = Registration {
            ini: ini.clone(),
            dll: dll.clone(),
            registered: false,
        };
        reg.register().unwrap();

        let entries = read_section(&ini);
        assert!(entries.contains(&("1".to_string(), dll.to_string_lossy().into_owned())));
        assert!(!entries.iter().any(|(key, _)| key == "1_detect"));
        assert!(entries.iter().any(|(key, _)| key == "0_detect"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn value_is_relative_to_commander_path() {
        let dll = Path::new(r"C:\TC\Plugins\wlx\mediares\mediares.wlx64");
        assert_eq!(
            ini_value(dll, Some(r"c:\tc\")),
            r"%COMMANDER_PATH%\Plugins\wlx\mediares\mediares.wlx64"
        );
        assert_eq!(ini_value(dll, Some(r"C:\TCX")), dll.to_string_lossy());
        assert_eq!(ini_value(dll, None), dll.to_string_lossy());
    }
}
