//! Shell actions on the file shown: Recycle Bin, external editor, Explorer, wallpaper.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    SHFileOperationW, ShellExecuteW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING,
    FO_DELETE, SHFILEOPSTRUCTW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, SystemParametersInfoW, IDYES, MB_ICONERROR, MB_ICONQUESTION, MB_OK, MB_YESNO,
    SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER, SW_SHOWNORMAL,
};

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub unsafe fn confirm_delete(owner: HWND, path: &Path) -> bool {
    let text = HSTRING::from(crate::i18n::tr_fmt(
        "Move \"{name}\" to the Recycle Bin?",
        &[("name", &file_name(path))],
    ));
    MessageBoxW(
        Some(owner),
        &text,
        w!("Mediares"),
        MB_YESNO | MB_ICONQUESTION,
    ) == IDYES
}

pub unsafe fn show_error(owner: HWND, text: &str) {
    MessageBoxW(
        Some(owner),
        &HSTRING::from(text),
        w!("Mediares"),
        MB_OK | MB_ICONERROR,
    );
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

unsafe fn shell_execute(
    owner: HWND,
    verb: PCWSTR,
    file: &HSTRING,
    params: Option<&HSTRING>,
) -> bool {
    let params = params.map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr()));
    // Values above 32 mean success.
    ShellExecuteW(
        Some(owner),
        verb,
        file,
        params,
        PCWSTR::null(),
        SW_SHOWNORMAL,
    )
    .0 as isize
        > 32
}

/// Splits an editor command line into the program and its arguments. The program may be quoted
/// (`"C:\Program Files\X\x.exe" -n`) or not: an unquoted path with spaces runs up to the first
/// prefix that is an existing file or ends in an executable extension, else up to the first space
/// (`code -n` with `code` on PATH).
pub fn split_command(command: &str, is_file: impl Fn(&str) -> bool) -> (&str, &str) {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        return match rest.find('"') {
            Some(end) => (&rest[..end], rest[end + 1..].trim_start()),
            None => (rest, ""),
        };
    }
    if is_file(command) {
        return (command, "");
    }
    let is_program = |p: &str| {
        let lower = p.to_ascii_lowercase();
        is_file(p)
            || [".exe", ".com", ".bat", ".cmd"]
                .iter()
                .any(|ext| lower.ends_with(ext))
    };
    let spaces = command.match_indices(char::is_whitespace).map(|(i, _)| i);
    match spaces
        .clone()
        .find(|&i| is_program(&command[..i]))
        .or_else(|| spaces.clone().next())
    {
        Some(i) => (&command[..i], command[i..].trim_start()),
        None => (command, ""),
    }
}

/// Editor arguments: `%1` (quoted or not) becomes the quoted file path; without it the path is
/// appended.
fn editor_params(args: &str, path: &Path) -> String {
    let quoted = format!("\"{}\"", path.display());
    if args.contains("%1") {
        args.replace("\"%1\"", "%1").replace("%1", &quoted)
    } else if args.is_empty() {
        quoted
    } else {
        format!("{} {}", args, quoted)
    }
}

/// Opens `path` in `editor`: a program path, quoted or not, optionally with arguments (`%1` marks
/// where the file goes). Without one: the file type's "Edit" program, or its default program if
/// there is none.
pub unsafe fn open_in_editor(owner: HWND, path: &Path, editor: &str) -> bool {
    let (program, args) = split_command(editor, |p| Path::new(p).is_file());
    if !program.is_empty() {
        let params = HSTRING::from(editor_params(args, path));
        return shell_execute(owner, w!("open"), &HSTRING::from(program), Some(&params));
    }
    let file = HSTRING::from(path.as_os_str());
    shell_execute(owner, w!("edit"), &file, None) || shell_execute(owner, w!("open"), &file, None)
}

/// An Explorer window with the file selected.
pub unsafe fn show_in_folder(owner: HWND, path: &Path) -> bool {
    let params = HSTRING::from(format!("/select,\"{}\"", path.display()));
    shell_execute(
        owner,
        w!("open"),
        &HSTRING::from("explorer.exe"),
        Some(&params),
    )
}

/// The place on OpenStreetMap in the default browser.
pub unsafe fn open_map(owner: HWND, (latitude, longitude): (f64, f64)) -> bool {
    let (lat, lon) = (format!("{latitude:.6}"), format!("{longitude:.6}"));
    let url = format!("https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=16/{lat}/{lon}");
    shell_execute(owner, w!("open"), &HSTRING::from(url), None)
}

/// `image` must be a format Windows accepts as wallpaper (JPEG, PNG, BMP).
pub unsafe fn set_wallpaper(image: &Path) -> bool {
    let mut wide: Vec<u16> = image.as_os_str().encode_wide().chain([0]).collect();
    SystemParametersInfoW(
        SPI_SETDESKWALLPAPER,
        0,
        Some(wide.as_mut_ptr() as *mut _),
        SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_finds_program_and_args() {
        let exists = |p: &str| p == r"C:\Program Files\Foo\foo.exe" || p == r"C:\Tools\My Editor";
        assert_eq!(split_command("  ", exists), ("", ""));
        assert_eq!(
            split_command(r"C:\Program Files\Foo\foo.exe", exists),
            (r"C:\Program Files\Foo\foo.exe", "")
        );
        assert_eq!(
            split_command(r"C:\Program Files\Foo\foo.exe -n", exists),
            (r"C:\Program Files\Foo\foo.exe", "-n")
        );
        assert_eq!(
            split_command(r#""C:\Program Files\Foo\foo.exe""#, exists),
            (r"C:\Program Files\Foo\foo.exe", "")
        );
        assert_eq!(
            split_command(r#" "D:\a b\x.exe"  -n "%1" "#, exists),
            (r"D:\a b\x.exe", r#"-n "%1""#)
        );
        assert_eq!(
            split_command(r"C:\Tools\My Editor --wait", exists),
            (r"C:\Tools\My Editor", "--wait")
        );
        assert_eq!(
            split_command(r"D:\Apps x\ed.exe -n", exists),
            (r"D:\Apps x\ed.exe", "-n")
        );
        assert_eq!(split_command("code -n", exists), ("code", "-n"));
        assert_eq!(
            split_command(r#""unterminated"#, exists),
            ("unterminated", "")
        );
    }

    #[test]
    fn editor_params_place_the_file() {
        let path = Path::new(r"D:\Фото\a b.jpg");
        assert_eq!(editor_params("", path), r#""D:\Фото\a b.jpg""#);
        assert_eq!(editor_params("-n", path), r#"-n "D:\Фото\a b.jpg""#);
        assert_eq!(
            editor_params(r#"--open "%1" --x"#, path),
            r#"--open "D:\Фото\a b.jpg" --x"#
        );
        assert_eq!(
            editor_params("--file=%1", path),
            r#"--file="D:\Фото\a b.jpg""#
        );
    }
}
