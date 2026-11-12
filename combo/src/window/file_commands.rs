//! File actions kept separate from the message dispatcher.

use super::*;

pub(crate) unsafe fn leave_fullscreen(hwnd: HWND) {
    if get_state(hwnd).is_some_and(|s| s.fullscreen.is_some()) {
        execute(hwnd, Command::ToggleFullscreen);
    }
}

/// Del: the file goes to the Recycle Bin and the next one is shown (the Lister closes when none
/// is left). State is re-fetched after each call that may show a dialog.
pub(crate) unsafe fn delete_current(hwnd: HWND) {
    let Some(state) = get_state(hwnd) else { return };
    let (path, confirm) = (state.file_path.clone(), state.config.confirm_delete);
    if !path.is_file() {
        return;
    }
    let owner = dialog::modal_owner(hwnd);
    if confirm && !file_actions::confirm_delete(owner, &path) {
        return;
    }
    let Some(state) = get_live_state(hwnd) else {
        return;
    };
    // A player keeps its file open: close it first.
    let had_player = state.media.take().is_some();
    let deleted = file_actions::recycle(owner, &path);
    let Some(state) = get_live_state(hwnd) else {
        return;
    };
    if !deleted {
        if had_player {
            state.reload();
            refresh(state);
        }
        return;
    }
    if state.remove_current() {
        refresh(state);
    } else if !state.lister.is_invalid() {
        let _ = PostMessageW(
            Some(state.lister),
            WM_KEYDOWN,
            WPARAM(VK_ESCAPE as usize),
            LPARAM(0),
        );
    }
}

/// Writes the turn made with L / R into the JPEG as its EXIF orientation: lossless, the image data
/// stays byte for byte. Other formats are pointed to "Save as".
pub(crate) unsafe fn save_rotation(hwnd: HWND) {
    let Some(state) = get_state(hwnd) else { return };
    if state.media.is_some() || state.image.is_none() {
        return;
    }
    let owner = dialog::modal_owner(hwnd);
    let path = state.file_path.clone();
    if state.quarter_turns == 0 {
        file_actions::show_error(owner, tr("Turn the photo with L / R first."));
        return;
    }
    if !is_jpeg(&path) {
        file_actions::show_error(
            owner,
            tr("Lossless rotation can be written to JPEG only.\nFor other formats use Save as (Ctrl+S)."),
        );
        return;
    }
    // With auto-rotation off the screen shows the stored pixels, so the turn counts from them.
    let auto_rotate = state.config.auto_rotate_exif;
    let current = if auto_rotate {
        read_orientation(&path).unwrap_or(1)
    } else {
        1
    };
    let orientation = mediares_core::orientation::turned(current, state.quarter_turns);
    if let Err(e) = mediares_core::orientation::set_jpeg_orientation(&path, orientation) {
        file_actions::show_error(
            owner,
            &format!(
                "{}:\n{}\n\n{}",
                tr("Could not save the rotation"),
                path.display(),
                e
            ),
        );
        return;
    }
    if auto_rotate {
        // The file now shows upright by itself; the turned picture stays until it is re-read.
        state.reload();
        refresh(state);
    } else {
        file_actions::show_error(
            owner,
            tr("Rotation saved. Auto-rotation by EXIF is off in Settings, so the photo is shown here without it."),
        );
    }
}

/// The photo as the desktop wallpaper: the file itself when Windows can show it as is, otherwise
/// (RAW, PSD, turned with R / L, EXIF-rotated) the picture on screen saved as PNG.
pub(crate) unsafe fn set_wallpaper(hwnd: HWND) {
    let Some(state) = get_state(hwnd) else { return };
    let path = state.file_path.clone();
    let as_is = state.image.is_some()
        && state.quarter_turns == 0
        && mediares_core::probe::has_extension(&path, &["jpg", "jpeg", "png", "bmp"])
        && read_orientation(&path).is_none_or(|o| o == 1);
    let source = if as_is {
        Some(path)
    } else {
        let Some(img) = state.whole_image() else {
            return;
        };
        let target = config::local_path("mediares_wallpaper.png");
        snapshot::save_png(&img, &target).then_some(target)
    };
    if !source.is_some_and(|s| file_actions::set_wallpaper(&s)) {
        file_actions::show_error(
            dialog::modal_owner(hwnd),
            tr("Could not set the desktop background."),
        );
    }
}

/// Prints the photo, or the video frame / album art. `margins` in 1/100 mm.
pub(crate) unsafe fn print(hwnd: HWND, printer: Option<&str>, margins: Option<RECT>) -> bool {
    let Some(state) = get_state(hwnd) else {
        return false;
    };
    let picture = match &state.media {
        Some(media) => media.current_picture(),
        None => state.whole_image(),
    };
    let Some(picture) = picture else { return false };
    let name = state
        .file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    crate::print::print(dialog::modal_owner(hwnd), picture, &name, printer, margins)
}
