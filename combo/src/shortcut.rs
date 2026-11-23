use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use mediares_core::mf_init::ComScope;
use mediares_core::probe::{probe_file, MediaGroup, MediaType};
use windows::core::{Interface, HSTRING};
use windows::Win32::System::Com::{
    CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

/// Queue order and selection use the shortcut name. Decoders use the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaEntry {
    pub target: PathBuf,
    pub kind: MediaType,
    shortcut: Option<PathBuf>,
}

pub fn is_shortcut(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
}

impl MediaEntry {
    pub fn source(&self) -> &Path {
        self.shortcut.as_deref().unwrap_or(&self.target)
    }

    pub fn group(&self) -> Option<MediaGroup> {
        self.kind.group()
    }

    pub fn resolve(path: PathBuf) -> Option<Self> {
        Self::resolve_allowed(path, |_| true)
    }

    pub fn resolve_allowed(path: PathBuf, mut allowed: impl FnMut(&Path) -> bool) -> Option<Self> {
        if !allowed(&path) {
            return None;
        }
        if !is_shortcut(&path) {
            let kind = probe_file(&path);
            return Some(Self {
                target: path,
                kind,
                shortcut: None,
            });
        }
        let target = read_link(&path)?;
        (allowed(&target) && target.is_file()).then(|| {
            let kind = probe_file(&target);
            Self {
                target,
                kind,
                shortcut: Some(path),
            }
        })
    }
}

/// Read a shell link's filesystem target without invoking its command or arguments.
fn read_link(path: &Path) -> Option<PathBuf> {
    let _com = ComScope::new();
    let mut buf = vec![0u16; 32768];
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        file.Load(&HSTRING::from(path), STGM_READ).ok()?;
        link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
    }
    let len = buf.iter().position(|&c| c == 0)?;
    if len == 0 {
        return None;
    }
    Some(path.parent()?.join(OsString::from_wide(&buf[..len])))
}
