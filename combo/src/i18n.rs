//! Interface language. Every UI string is written at its call site as a Russian / English pair:
//! `tr("Настройки", "Settings")`.

use std::sync::atomic::{AtomicU8, Ordering};

use windows::core::{w, HSTRING};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::WindowsProgramming::GetPrivateProfileStringW;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ru,
    En,
}

/// The language setting: follow Total Commander, or a fixed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangSetting {
    Auto,
    Fixed(Lang),
}

impl LangSetting {
    pub const ALL: [LangSetting; 3] = [
        LangSetting::Auto,
        LangSetting::Fixed(Lang::En),
        LangSetting::Fixed(Lang::Ru),
    ];

    pub fn from_ini(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "en" => LangSetting::Fixed(Lang::En),
            "ru" => LangSetting::Fixed(Lang::Ru),
            _ => LangSetting::Auto,
        }
    }

    pub fn to_ini(self) -> &'static str {
        match self {
            LangSetting::Auto => "auto",
            LangSetting::Fixed(Lang::En) => "en",
            LangSetting::Fixed(Lang::Ru) => "ru",
        }
    }

    /// Language names are shown in their own language.
    pub fn label(self) -> &'static str {
        match self {
            LangSetting::Auto => tr("Как в Total Commander", "Same as Total Commander"),
            LangSetting::Fixed(Lang::En) => "English",
            LangSetting::Fixed(Lang::Ru) => "Русский",
        }
    }

    pub fn resolve(self) -> Lang {
        match self {
            LangSetting::Fixed(lang) => lang,
            LangSetting::Auto => tc_language().unwrap_or_else(windows_language),
        }
    }
}

/// Russian until a configuration is applied (unit tests check the Russian texts).
static CURRENT: AtomicU8 = AtomicU8::new(Lang::Ru as u8);

pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == Lang::En as u8 {
        Lang::En
    } else {
        Lang::Ru
    }
}

pub fn tr(ru: &'static str, en: &'static str) -> &'static str {
    match current() {
        Lang::Ru => ru,
        Lang::En => en,
    }
}

/// A decimal number with the language's separator: "44,1" / "44.1".
pub fn decimal(formatted: String) -> String {
    match current() {
        Lang::Ru => formatted.replace('.', ","),
        Lang::En => formatted,
    }
}

/// `LanguageIni` in `wincmd.ini`: empty means TC's built-in English.
fn tc_language() -> Option<Lang> {
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
    let file = String::from_utf16_lossy(&buf[..len]).to_ascii_uppercase();
    Some(if file.contains("RUS") {
        Lang::Ru
    } else {
        Lang::En
    })
}

fn windows_language() -> Lang {
    const LANG_RUSSIAN: u16 = 0x19;
    if unsafe { GetUserDefaultUILanguage() } & 0x3FF == LANG_RUSSIAN {
        Lang::Ru
    } else {
        Lang::En
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_round_trip() {
        for s in LangSetting::ALL {
            assert_eq!(LangSetting::from_ini(s.to_ini()), s);
        }
        assert_eq!(LangSetting::from_ini(""), LangSetting::Auto);
        assert_eq!(LangSetting::from_ini(" EN "), LangSetting::Fixed(Lang::En));
    }
}
