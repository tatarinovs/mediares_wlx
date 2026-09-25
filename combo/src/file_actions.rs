//! Shell actions on the file shown: Recycle Bin, external editor, Explorer, wallpaper.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    SHFileOperationW, ShellExecuteW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING, FO_DELETE, SHFILEOPSTRUCTW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, SystemParametersInfoW, IDYES, MB_ICONERROR, MB_ICONQUESTION, MB_OK, MB_YESNO, SPIF_SENDCHANGE,
    SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER, SW_SHOWNORMAL,
};

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub unsafe fn confirm_delete(owner: HWND, path: &Path) -> bool {
    let text = HSTRING::from(format!("Удалить «{}» в корзину?", file_name(path)));
    MessageBoxW(Some(owner), &text, w!("Mediares"), MB_YESNO | MB_ICONQUESTION) == IDYES
}

pub unsafe fn show_error(owner: HWND, text: &str) {
    MessageBoxW(Some(owner), &HSTRING::from(text), w!("Mediares"), MB_OK | MB_ICONERROR);
}

/// Moves `path` to the Recycle Bin; Windows warns first if it would be deleted for good (e.g. on a
/// network drive). True if the file is gone.
pub unsafe fn recycle(owner: HWND, path: &Path) -> bool {
    let from: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    let mut op = SHFILEOPSTRUCTW {
        hwnd: owner,
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING).0 as u16,
        ..Default::default()
    };
    SHFileOperationW(&mut op) == 0 && !op.fAnyOperationsAborted.as_bool() && !path.exists()
}

unsafe fn shell_execute(owner: HWND, verb: PCWSTR, file: &HSTRING, params: Option<&HSTRING>) -> bool {
    let params = params.map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr()));
    // Values above 32 mean success.
    ShellExecuteW(Some(owner), verb, file, params, PCWSTR::null(), SW_SHOWNORMAL).0 as isize > 32
}

/// Opens `path` in `editor` (a program path; quotes around it are fine). Without one: the file
/// type's "Edit" program, or its default program if there is none.
pub unsafe fn open_in_editor(owner: HWND, path: &Path, editor: &str) -> bool {
    let editor = editor.trim().trim_matches('"');
    if !editor.is_empty() {
        let params = HSTRING::from(format!("\"{}\"", path.display()));
        return shell_execute(owner, w!("open"), &HSTRING::from(editor), Some(&params));
    }
    let file = HSTRING::from(path.as_os_str());
    shell_execute(owner, w!("edit"), &file, None) || shell_execute(owner, w!("open"), &file, None)
}

/// An Explorer window with the file selected.
pub unsafe fn show_in_folder(owner: HWND, path: &Path) -> bool {
    let params = HSTRING::from(format!("/select,\"{}\"", path.display()));
    shell_execute(owner, w!("open"), &HSTRING::from("explorer.exe"), Some(&params))
}

/// `image` must be a format Windows accepts as wallpaper (JPEG, PNG, BMP).
pub unsafe fn set_wallpaper(image: &Path) -> bool {
    let mut wide: Vec<u16> = image.as_os_str().encode_wide().chain([0]).collect();
    SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, Some(wide.as_mut_ptr() as *mut _), SPIF_UPDATEINIFILE | SPIF_SENDCHANGE).is_ok()
}
