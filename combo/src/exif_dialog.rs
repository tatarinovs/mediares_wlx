//! Modal dialog showing EXIF metadata.

use std::path::Path;

use mediares_core::exif::{read_exif, ExifInfo};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, GetWindowLongPtrW, SetWindowLongPtrW, BS_DEFPUSHBUTTON, ES_AUTOVSCROLL,
    ES_MULTILINE, ES_READONLY, GWLP_USERDATA, IDCANCEL, IDOK, WM_CLOSE, WM_COMMAND, WS_BORDER,
    WS_TABSTOP, WS_VSCROLL,
};

use crate::dialog;
use crate::i18n::tr;

const CLASS_NAME: PCWSTR = w!("MediaresExifDialogClass");
const IDC_EXIF_TEXT: usize = 201;
const IDC_OPEN_MAP: i32 = 202;

pub unsafe fn show(owner: HWND, file_path: &Path) {
    dialog::register_class(CLASS_NAME, Some(wnd_proc));
    let filename = file_path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let Some(dlg) = dialog::create_frame(
        owner,
        CLASS_NAME,
        &format!("{} - {}", tr("EXIF metadata"), filename),
        540,
        440,
    ) else {
        return;
    };

    let info = read_exif(file_path);
    // Monospace so the label column lines up.
    let font = dialog::font(dlg, "Consolas", -13);
    let ui_font = dialog::font(dlg, "Segoe UI", -12);
    let edit_style = WS_TABSTOP.0
        | WS_VSCROLL.0
        | WS_BORDER.0
        | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32;
    dialog::control(
        dlg,
        w!("EDIT"),
        &exif_text(file_path, info.as_ref()),
        edit_style,
        (15, 15, 495, 335),
        IDC_EXIF_TEXT,
        font.0,
    );
    let close = dialog::control(
        dlg,
        w!("BUTTON"),
        tr("Close"),
        WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
        (410, 360, 100, 28),
        IDOK.0 as usize,
        ui_font.0,
    );

    // The coordinates for the map button live in the window until it is gone.
    let place = info.as_ref().and_then(ExifInfo::gps).map(|place| {
        dialog::control(
            dlg,
            w!("BUTTON"),
            tr("Open on map"),
            WS_TABSTOP.0,
            (15, 360, 150, 28),
            IDC_OPEN_MAP as usize,
            ui_font.0,
        );
        let place = Box::into_raw(Box::new(place));
        SetWindowLongPtrW(dlg, GWLP_USERDATA, place as isize);
        place
    });

    dialog::run_modal(dlg, close);
    // run_modal returns only after the window is destroyed, so nothing references `place` anymore.
    if let Some(place) = place {
        drop(Box::from_raw(place));
    }
}

fn exif_text(file_path: &Path, info: Option<&ExifInfo>) -> String {
    let rows = info.map(display_rows).unwrap_or_default();
    let mut text = if rows.is_empty() {
        format!(
            "{}\r\n",
            tr("No EXIF metadata found, or the format is not supported.")
        )
    } else {
        let width = rows
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0);
        rows.iter()
            .map(|(k, v)| format!("{:<width$} : {}\r\n", k, v))
            .collect()
    };
    text.push_str(&format!("\r\n--- {} ---\r\n", tr("File path")));
    text.push_str(&file_path.to_string_lossy());
    text.push_str("\r\n");
    text
}

fn display_rows(info: &ExifInfo) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    let mut push = |label: &'static str, value: Option<String>| {
        if let Some(v) = value {
            rows.push((label, v));
        }
    };

    push(tr("Make"), info.make.clone());
    push(tr("Camera model"), info.model.clone());
    push(tr("Lens"), info.lens_model.clone());
    match (&info.date_time_original, &info.date_time) {
        (Some(taken), _) => push(tr("Date and time taken"), Some(taken.clone())),
        (None, modified) => push(tr("Date modified"), modified.clone()),
    }
    push(
        tr("Exposure"),
        info.exposure_time
            .as_ref()
            .map(|s| format!("{} {}", s, tr("s"))),
    );
    push(tr("Aperture"), info.f_number.map(|f| format!("f/{:.1}", f)));
    push(tr("Sensitivity (ISO)"), info.iso.map(|v| v.to_string()));
    push(
        tr("Focal length"),
        info.focal_length.map(|f| match info.focal_length_35mm {
            Some(eq) => format!(
                "{:.1} {mm} ({} {} {mm})",
                f,
                tr("equiv."),
                eq,
                mm = tr("mm")
            ),
            None => format!("{:.1} {}", f, tr("mm")),
        }),
    );
    push(
        tr("Flash"),
        info.flash_fired
            .map(|f| if f { tr("Fired") } else { tr("Did not fire") }.to_string()),
    );
    push(
        tr("EXIF orientation"),
        info.orientation
            .map(|o| format!("{} ({} {})", orientation_name(o), tr("code"), o)),
    );
    push(
        tr("EXIF dimensions"),
        info.width
            .zip(info.height)
            .map(|(w, h)| format!("{} x {}", w, h)),
    );
    push(tr("Software"), info.software.clone());
    push(
        tr("GPS coordinates"),
        info.gps()
            .map(|(lat, lon)| format!("{:.6}, {:.6}", lat, lon)),
    );
    rows
}

fn orientation_name(code: u16) -> &'static str {
    match code {
        1 => tr("Normal (0°)"),
        2 => tr("Mirrored horizontally"),
        3 => tr("Rotated 180°"),
        4 => tr("Mirrored vertically"),
        5 => tr("Rotated 90° counterclockwise and mirrored"),
        6 => tr("Rotated 90° clockwise"),
        7 => tr("Rotated 90° clockwise and mirrored"),
        8 => tr("Rotated 270° clockwise"),
        _ => tr("Unknown"),
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    mediares_core::ffi::guard(LRESULT(0), || unsafe {
        let id = dialog::loword(wparam) as i32;
        match msg {
            WM_COMMAND if id == IDOK.0 || id == IDCANCEL.0 => {
                dialog::close(hwnd);
                LRESULT(0)
            }
            WM_COMMAND if id == IDC_OPEN_MAP => {
                let place = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const (f64, f64);
                if let Some(&place) = place.as_ref() {
                    crate::file_actions::open_map(hwnd, place);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                dialog::close(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_rows_format_values() {
        let info = ExifInfo {
            make: Some("Canon".into()),
            model: Some("EOS R5".into()),
            orientation: Some(6),
            f_number: Some(2.8),
            iso: Some(400),
            exposure_time: Some("1/250".into()),
            gps_latitude: Some(55.751244),
            gps_longitude: Some(-37.618423),
            ..Default::default()
        };
        let rows = display_rows(&info);
        let has = |k: &str, v: &str| rows.iter().any(|(rk, rv)| *rk == k && rv == v);
        assert!(has(tr("Make"), "Canon"));
        assert!(has(tr("Camera model"), "EOS R5"));
        assert!(has(
            tr("EXIF orientation"),
            "Поворот на 90° по часовой (код 6)"
        ));
        assert!(has(tr("Aperture"), "f/2.8"));
        assert!(has(tr("Sensitivity (ISO)"), "400"));
        assert!(has(tr("GPS coordinates"), "55.751244, -37.618423"));
        // Cyrillic "с" (seconds); the old pair had a Latin "c" by mistake.
        assert!(has(tr("Exposure"), "1/250 \u{0441}"));
    }
}
