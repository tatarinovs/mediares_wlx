use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf, Prefix};

use mediares_core::probe::{probe_file, MediaGroup, MediaType};
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::System::WindowsProgramming::{
    DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOVABLE,
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
        if !allowed(&target) {
            return None;
        }
        let kind = probe_file(&target);
        if kind == MediaType::Unsupported {
            return None;
        }
        target.is_file().then_some(Self {
            target,
            kind,
            shortcut: Some(path),
        })
    }
}

/// Allows targets on local drives and on `base`'s own UNC share, blocking external network
/// shares so automatic folder scans and playlists cannot trigger unexpected SMB connections.
pub fn same_origin_filter(base: &Path) -> impl FnMut(&Path) -> bool {
    fn prefix(path: &Path) -> Option<Prefix<'_>> {
        match path.components().next()? {
            Component::Prefix(prefix) => Some(prefix.kind()),
            _ => None,
        }
    }
    let mut local_drives = HashMap::new();
    let base_prefix = prefix(base).map(|p| match p {
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => BaseOrigin::Disk(d),
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            BaseOrigin::Unc(server.to_os_string(), share.to_os_string())
        }
        _ => BaseOrigin::Other,
    });
    enum BaseOrigin {
        Disk(u8),
        Unc(OsString, OsString),
        Other,
    }
    move |entry: &Path| {
        entry.is_absolute()
            && match prefix(entry) {
                Some(Prefix::Disk(drive) | Prefix::VerbatimDisk(drive)) => {
                    if matches!(base_prefix, Some(BaseOrigin::Disk(d)) if d == drive) {
                        return true;
                    }
                    *local_drives.entry(drive).or_insert_with(|| {
                        let root = [u16::from(drive), b':' as u16, b'\\' as u16, 0];
                        let kind = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
                        matches!(
                            kind,
                            DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_CDROM | DRIVE_RAMDISK
                        )
                    })
                }
                Some(Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share)) => {
                    matches!(&base_prefix, Some(BaseOrigin::Unc(base_server, base_share))
                    if crate::state::same_name(server, base_server)
                        && crate::state::same_name(share, base_share))
                }
                _ => false,
            }
    }
}

struct StaComScope {
    initialized: bool,
}

impl StaComScope {
    fn new() -> Self {
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        Self { initialized }
    }
}

impl Drop for StaComScope {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

/// Read a shell link's filesystem target without invoking its command or arguments.
fn read_link(path: &Path) -> Option<PathBuf> {
    let _com = StaComScope::new();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_origin_filter_blocks_external_unc_and_allows_local_and_same_share() {
        let mut from_local = same_origin_filter(Path::new(r"C:\Photos\img.jpg"));
        assert!(from_local(Path::new(r"C:\Photos\other.jpg")));
        assert!(!from_local(Path::new(r"\\other.invalid\share\a.jpg")));
        assert!(!from_local(Path::new("relative.jpg")));

        let mut from_unc = same_origin_filter(Path::new(r"\\nas\Media\list.m3u"));
        assert!(from_unc(Path::new(r"\\NAS\media\sub\clip.mp4")));
        assert!(!from_unc(Path::new(r"\\nas\OtherShare\clip.mp4")));
        assert!(!from_unc(Path::new(r"\\other.invalid\Media\clip.mp4")));
    }
}
