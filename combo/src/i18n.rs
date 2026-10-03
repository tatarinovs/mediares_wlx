//! Interface language. UI strings are written in English at their call sites — `tr("Settings")` —
//! and translated through `mediares_ui.lng` (UTF-8, one section per language, named like Total
//! Commander's language files: `wcmd_rus.lng` → `[Rus]`). The file is built into the DLL; a copy
//! next to the plugin overrides its strings and may add languages without rebuilding.
//!
//! Strings in tables (menus, field lists) are marked with [`n`] and translated where shown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use windows::core::{w, HSTRING};
use windows::Win32::Globalization::{GetLocaleInfoW, GetUserDefaultUILanguage, LOCALE_SABBREVLANGNAME};
use windows::Win32::System::WindowsProgramming::GetPrivateProfileStringW;

const FILE_NAME: &str = "mediares_ui.lng";
const BUILT_IN: &str = include_str!("../../pluginst/mediares_ui.lng");
/// The language of the source code: needs no section.
const ENGLISH: &str = "eng";

/// One language of the catalog.
pub struct Lang {
    /// Lower-case section name ("rus").
    pub code: &'static str,
    /// Shown in the settings, in the language itself.
    pub name: &'static str,
    decimal: &'static str,
    strings: HashMap<&'static str, &'static str>,
}

/// English first, then the languages of the file(s) in their order.
fn catalog() -> &'static [Lang] {
    static CATALOG: OnceLock<Vec<Lang>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let external = crate::config::dll_path()
            .and_then(|dll| std::fs::read(dll.with_file_name(FILE_NAME)).ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        build_catalog(BUILT_IN, external.as_deref())
    })
}

/// Sections of `external` override `built_in` key by key (and may add languages).
fn build_catalog(built_in: &str, external: Option<&str>) -> Vec<Lang> {
    let mut langs = vec![Lang {
        code: ENGLISH,
        name: "English",
        decimal: ".",
        strings: HashMap::new(),
    }];
    for text in std::iter::once(built_in).chain(external) {
        for (section, entries) in parse(text) {
            let code = section.to_ascii_lowercase();
            let pos = match langs.iter().position(|l| l.code == code) {
                Some(pos) => pos,
                None => {
                    langs.push(Lang {
                        code: leak(code),
                        name: "",
                        decimal: ".",
                        strings: HashMap::new(),
                    });
                    langs.len() - 1
                }
            };
            let lang = &mut langs[pos];
            for (key, value) in entries {
                match key.as_str() {
                    "@name" => lang.name = leak(value),
                    "@decimal" => lang.decimal = leak(value),
                    _ => {
                        lang.strings.insert(leak(key), leak(value));
                    }
                }
            }
        }
    }
    for lang in &mut langs {
        if lang.name.is_empty() {
            lang.name = lang.code;
        }
    }
    langs
}

/// The catalog lives for the whole process.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// `[Section]` followed by `key=value` lines; `;` starts a comment line.
fn parse(text: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start_matches('\u{FEFF}').trim_end();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((name.trim().to_string(), Vec::new()));
            continue;
        }
        let (Some(section), Some((key, value))) = (sections.last_mut(), split_entry(line)) else {
            continue;
        };
        section.1.push((key, value));
    }
    sections
}

/// Splits at the first `=` not escaped as `\=`, unescaping both sides.
fn split_entry(line: &str) -> Option<(String, String)> {
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '=' => return Some((unescape(&line[..i]), unescape(&line[i + 1..]))),
            _ => {}
        }
    }
    None
}

/// `\n`, `\t`, `\0`, `\=` and `\\`; any other backslash stays as written.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            // File dialog filters are NUL-separated lists.
            Some('0') => out.push('\0'),
            Some(c @ ('=' | '\\')) => out.push(c),
            Some(c) => {
                out.push('\\');
                out.push(c);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn find(code: &str) -> Option<usize> {
    catalog()
        .iter()
        .position(|l| l.code.eq_ignore_ascii_case(code))
}

/// Russian until a configuration is applied (unit tests check the Russian texts).
static CURRENT: AtomicUsize = AtomicUsize::new(usize::MAX);

pub fn set(lang: &'static Lang) {
    let index = catalog()
        .iter()
        .position(|l| std::ptr::eq(l, lang))
        .unwrap_or(0);
    CURRENT.store(index, Ordering::Relaxed);
}

pub fn current() -> &'static Lang {
    let index = match CURRENT.load(Ordering::Relaxed) {
        usize::MAX => find("rus").unwrap_or(0),
        i => i,
    };
    &catalog()[index.min(catalog().len() - 1)]
}

/// The translation of the English `text`, or `text` itself.
pub fn tr(text: &'static str) -> &'static str {
    current().strings.get(text).copied().unwrap_or(text)
}

/// [`tr`] with `{name}` placeholders filled in (translations may reorder them).
pub fn tr_fmt(text: &'static str, args: &[(&str, &str)]) -> String {
    args.iter()
        .fold(tr(text).to_string(), |s, (name, value)| {
            s.replace(&format!("{{{name}}}"), value)
        })
}

/// Marks a string in a table for translation; it is passed through [`tr`] where shown.
pub const fn n(text: &'static str) -> &'static str {
    text
}

/// A decimal number with the language's separator: "44,1" / "44.1".
pub fn decimal(formatted: String) -> String {
    match current().decimal {
        "." => formatted,
        separator => formatted.replace('.', separator),
    }
}

/// The language setting: follow Total Commander, or a fixed one (by section code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangSetting {
    Auto,
    Fixed(&'static str),
}

impl LangSetting {
    /// "Same as Total Commander", then every language of the catalog.
    pub fn all() -> Vec<LangSetting> {
        std::iter::once(LangSetting::Auto)
            .chain(catalog().iter().map(|l| LangSetting::Fixed(l.code)))
            .collect()
    }

    /// Codes of languages that are no longer available read as `Auto`. Older configs stored
    /// "en" / "ru".
    pub fn from_ini(value: &str) -> Self {
        let code = match value.trim().to_ascii_lowercase().as_str() {
            "en" => ENGLISH.to_string(),
            "ru" => "rus".to_string(),
            other => other.to_string(),
        };
        match find(&code) {
            Some(i) if !code.is_empty() => LangSetting::Fixed(catalog()[i].code),
            _ => LangSetting::Auto,
        }
    }

    pub fn to_ini(self) -> &'static str {
        match self {
            LangSetting::Auto => "auto",
            LangSetting::Fixed(code) => code,
        }
    }

    /// Language names are shown in their own language.
    pub fn label(self) -> &'static str {
        match self {
            LangSetting::Auto => tr("Same as Total Commander"),
            LangSetting::Fixed(code) => find(code).map_or(code, |i| catalog()[i].name),
        }
    }

    pub fn resolve(self) -> &'static Lang {
        let index = match self {
            LangSetting::Fixed(code) => find(code),
            LangSetting::Auto => tc_language()
                .or_else(windows_language)
                .and_then(|code| find(&code)),
        };
        &catalog()[index.unwrap_or(0)]
    }
}

/// `LanguageIni` in `wincmd.ini` as a section code: `wcmd_rus.lng` → "rus"; empty means TC's
/// built-in English. `None` outside TC.
fn tc_language() -> Option<String> {
    let ini = HSTRING::from(crate::tc_register::wincmd_ini()?.as_os_str());
    let mut buf = [0u16; 512];
    let len = unsafe {
        GetPrivateProfileStringW(
            w!("Configuration"),
            w!("LanguageIni"),
            w!(""),
            Some(&mut buf),
            &ini,
        )
    } as usize;
    Some(language_code(&String::from_utf16_lossy(&buf[..len])))
}

/// The section code of a TC language file name (possibly with a path).
fn language_code(language_ini: &str) -> String {
    let name = language_ini
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let code = name.strip_suffix(".lng").unwrap_or(&name);
    let code = code.strip_prefix("wcmd_").unwrap_or(code);
    if code.is_empty() {
        ENGLISH.to_string()
    } else {
        code.to_string()
    }
}

/// Windows' three-letter name of the UI language ("RUS", "DEU", "ENU"), as a section code.
fn windows_language() -> Option<String> {
    let lcid = u32::from(unsafe { GetUserDefaultUILanguage() });
    let mut buf = [0u16; 16];
    let len = unsafe { GetLocaleInfoW(lcid, LOCALE_SABBREVLANGNAME, Some(&mut buf)) };
    let name = String::from_utf16_lossy(&buf[..usize::try_from(len).ok()?.saturating_sub(1)]);
    Some(name.to_ascii_lowercase()).filter(|n| n.len() == 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_round_trip() {
        for s in LangSetting::all() {
            assert_eq!(LangSetting::from_ini(s.to_ini()), s);
        }
        assert_eq!(LangSetting::from_ini(""), LangSetting::Auto);
        assert_eq!(LangSetting::from_ini("xyz"), LangSetting::Auto);
        assert_eq!(LangSetting::from_ini(" EN "), LangSetting::Fixed("eng"));
        assert_eq!(LangSetting::from_ini("ru"), LangSetting::Fixed("rus"));
        assert_eq!(LangSetting::from_ini("Rus"), LangSetting::Fixed("rus"));
    }

    #[test]
    fn tc_language_files_name_the_sections() {
        assert_eq!(language_code("wcmd_rus.lng"), "rus");
        assert_eq!(language_code(r"%COMMANDER_PATH%\Language\WCMD_DEU.LNG"), "deu");
        assert_eq!(language_code(""), "eng");
    }

    #[test]
    fn entries_unescape_and_external_files_override() {
        let built_in = "; comment\r\n[Rus]\r\n@name=Русский\r\n@decimal=,\r\nA\\=B\\tC=Х\\nЦ\r\nKeep=Оставить\r\n";
        let external = "\u{FEFF}[rus]\nKeep=Сохранить\n[Deu]\n@name=Deutsch\nKeep=Behalten\n";
        let langs = build_catalog(built_in, Some(external));
        let codes: Vec<&str> = langs.iter().map(|l| l.code).collect();
        assert_eq!(codes, ["eng", "rus", "deu"]);
        let rus = &langs[1];
        assert_eq!((rus.name, rus.decimal), ("Русский", ","));
        assert_eq!(rus.strings.get("A=B\tC"), Some(&"Х\nЦ"));
        assert_eq!(rus.strings.get("Keep"), Some(&"Сохранить"));
        assert_eq!((langs[2].name, langs[2].decimal), ("Deutsch", "."));
    }

    #[test]
    fn placeholders_are_filled_by_name() {
        assert_eq!(
            tr_fmt("Move \"{name}\" to the Recycle Bin?", &[("name", "a.jpg")]),
            "Удалить «a.jpg» в корзину?"
        );
        assert_eq!(decimal("44.1".into()), "44,1");
    }

    /// The literal strings passed to `tr`, `tr_fmt` and `n` anywhere in the crate.
    fn source_keys() -> Vec<(String, String, bool)> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut keys = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            // Code only: no comments, no unit tests.
            let full = std::fs::read_to_string(&path).unwrap();
            let code = full.split("#[cfg(test)]").next().unwrap_or_default();
            let src: String = code
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .map(|l| format!("{l}\n"))
                .collect();
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            for call in ["tr(", "tr_fmt(", "n("] {
                for (at, _) in src.match_indices(call) {
                    let before = src[..at].chars().next_back();
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    let rest = src[at + call.len()..].trim_start();
                    if let Some(text) = rust_literal(rest) {
                        keys.push((text, file.clone(), call == "tr_fmt("));
                    }
                }
            }
        }
        keys
    }

    /// The value of the plain string literal `s` starts with.
    fn rust_literal(s: &str) -> Option<String> {
        let mut chars = s.strip_prefix('"')?.chars();
        let mut out = String::new();
        loop {
            match chars.next()? {
                '"' => return Some(out),
                '\\' => match chars.next()? {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    '0' => out.push('\0'),
                    c => out.push(c),
                },
                c => out.push(c),
            }
        }
    }

    fn placeholders(s: &str) -> Vec<&str> {
        let mut found: Vec<&str> = s
            .match_indices('{')
            .filter_map(|(i, _)| {
                let end = s[i..].find('}')?;
                let name = &s[i + 1..i + end];
                (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                    .then_some(name)
            })
            .collect();
        found.sort();
        found
    }

    /// Every language of the built-in file translates every string of the code, nothing else,
    /// with the same `{placeholders}`, and a hotkey column (after a tab) where the original has one.
    #[test]
    fn every_language_translates_every_string() {
        let keys = source_keys();
        assert!(keys.len() > 150, "found {} strings", keys.len());
        for lang in build_catalog(BUILT_IN, None).iter().skip(1) {
            for (key, file, formatted) in &keys {
                let value = lang
                    .strings
                    .get(key.as_str())
                    .unwrap_or_else(|| panic!("[{}] lacks {key:?} ({file})", lang.code));
                // Only `tr_fmt` fills placeholders; elsewhere braces are plain text.
                if *formatted {
                    assert_eq!(placeholders(key), placeholders(value), "[{}] {key:?}", lang.code);
                }
                assert_eq!(
                    key.contains('\t'),
                    value.contains('\t'),
                    "[{}] hotkey column of {key:?}",
                    lang.code
                );
            }
            for key in lang.strings.keys() {
                assert!(
                    keys.iter().any(|(k, _, _)| k == key),
                    "[{}] translates {key:?}, which the code no longer uses",
                    lang.code
                );
            }
        }
    }
}
