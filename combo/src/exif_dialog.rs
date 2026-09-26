//! Modal dialog showing EXIF metadata.

use std::path::Path;

use mediares_core::exif::{read_exif, ExifInfo};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, BS_DEFPUSHBUTTON, ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, IDCANCEL, IDOK,
    WM_CLOSE, WM_COMMAND, WS_BORDER, WS_TABSTOP, WS_VSCROLL,
};

use crate::dialog;

const CLASS_NAME: PCWSTR = w!("MediaresExifDialogClass");
const IDC_EXIF_TEXT: usize = 201;

pub unsafe fn show(owner: HWND, file_path: &Path) {
    dialog::register_class(CLASS_NAME, Some(wnd_proc));
    let filename = file_path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let Some(dlg) = dialog::create_frame(
        owner,
        CLASS_NAME,
        &format!("EXIF метаданные - {}", filename),
        540,
        440,
    ) else {
        return;
    };

    // Monospace so the label column lines up.
    let font = dialog::create_font(w!("Consolas"), -13);
    let ui_font = dialog::create_font(w!("Segoe UI"), -12);
    let edit_style = WS_TABSTOP.0
        | WS_VSCROLL.0
        | WS_BORDER.0
        | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32;
    dialog::control(
        dlg,
        w!("EDIT"),
        &exif_text(file_path),
        edit_style,
        (15, 15, 495, 335),
        IDC_EXIF_TEXT,
        font.0,
    );
    let close = dialog::control(
        dlg,
        w!("BUTTON"),
        "Закрыть",
        WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
        (410, 360, 100, 28),
        IDOK.0 as usize,
        ui_font.0,
    );

    dialog::run_modal(dlg, close);
}

fn exif_text(file_path: &Path) -> String {
    let rows = read_exif(file_path)
        .map(|info| display_rows(&info))
        .unwrap_or_default();
    let mut text = if rows.is_empty() {
        "EXIF метаданные не найдены или формат не поддерживается.\r\n".to_string()
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
    text.push_str("\r\n--- Путь к файлу ---\r\n");
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

    push("Производитель", info.make.clone());
    push("Модель камеры", info.model.clone());
    push("Объектив", info.lens_model.clone());
    match (&info.date_time_original, &info.date_time) {
        (Some(taken), _) => push("Дата и время съемки", Some(taken.clone())),
        (None, modified) => push("Дата изменения", modified.clone()),
    }
    push(
        "Выдержка",
        info.exposure_time.as_ref().map(|s| format!("{} c", s)),
    );
    push("Диафрагма", info.f_number.map(|f| format!("f/{:.1}", f)));
    push(
        "Светочувствительность (ISO)",
        info.iso.map(|v| v.to_string()),
    );
    push(
        "Фокусное расстояние",
        info.focal_length.map(|f| match info.focal_length_35mm {
            Some(eq) => format!("{:.1} мм (экв. {} мм)", f, eq),
            None => format!("{:.1} мм", f),
        }),
    );
    push(
        "Вспышка",
        info.flash_fired.map(|f| {
            if f {
                "Сработала"
            } else {
                "Не сработала"
            }
            .to_string()
        }),
    );
    push(
        "Ориентация EXIF",
        info.orientation
            .map(|o| format!("{} (код {})", orientation_name(o), o)),
    );
    push(
        "Разрешение EXIF",
        info.width
            .zip(info.height)
            .map(|(w, h)| format!("{} x {}", w, h)),
    );
    push("Программное обеспечение", info.software.clone());
    rows
}

fn orientation_name(code: u16) -> &'static str {
    match code {
        1 => "Обычная (0°)",
        2 => "Отражение по горизонтали",
        3 => "Поворот на 180°",
        4 => "Отражение по вертикали",
        5 => "Поворот на 90° против часовой и отражение",
        6 => "Поворот на 90° по часовой",
        7 => "Поворот на 90° по часовой и отражение",
        8 => "Поворот на 270° по часовой",
        _ => "Неизвестно",
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
            ..Default::default()
        };
        let rows = display_rows(&info);
        let has = |k: &str, v: &str| rows.iter().any(|(rk, rv)| *rk == k && rv == v);
        assert!(has("Производитель", "Canon"));
        assert!(has("Модель камеры", "EOS R5"));
        assert!(has("Ориентация EXIF", "Поворот на 90° по часовой (код 6)"));
        assert!(has("Диафрагма", "f/2.8"));
        assert!(has("Светочувствительность (ISO)", "400"));
        assert!(has("Выдержка", "1/250 c"));
    }
}
