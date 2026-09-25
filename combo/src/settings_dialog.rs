//! Modal settings dialog for the viewer configuration.

use std::sync::Mutex;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, InvalidateRect, SetBkColor, SetBkMode, SetTextColor, HDC, OPAQUE};
use windows::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, GetDlgCtrlID, GetDlgItem, GetWindowLongPtrW, SendDlgItemMessageW,
    SetWindowLongPtrW, BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_GROUPBOX,
    BS_PUSHBUTTON, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, GWLP_USERDATA,
    IDCANCEL, IDOK, WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC, WM_NCDESTROY, WS_TABSTOP, WS_VSCROLL,
};

use crate::config::ViewerConfig;
use crate::dialog::{self, Brush, GdiObject};

const CLASS_NAME: PCWSTR = w!("MediaresSettingsDialogClass");

const IDC_START_FULLSCREEN: i32 = 101;
const IDC_AUTO_ROTATE_EXIF: i32 = 102;
const IDC_LOUPE_SCALE: i32 = 103;
const IDC_SHOW_OSD: i32 = 104;
const IDC_FONT_SIZE: i32 = 105;
const IDC_CHOOSE_COLOR: i32 = 106;
const IDC_COLOR_PREVIEW: i32 = 107;

const BST_CHECKED: usize = 1;
const SS_LEFT: u32 = 0x0000;
const SS_CENTER: u32 = 0x0001;

const LOUPE_SCALES: &[f32] = &[1.0, 1.5, 2.0, 2.5, 3.0];
const FONT_SIZES: &[i32] = &[10, 12, 14, 16, 18, 20, 24, 28, 32];

/// Custom colors of the color picker, kept for the session.
static CUSTOM_COLORS: Mutex<[COLORREF; 16]> = Mutex::new([COLORREF(0); 16]);

struct Context {
    config: ViewerConfig,
    color: u32,
    loupe_scales: Vec<f32>,
    font_sizes: Vec<i32>,
    preview_brush: Brush,
    result: Option<ViewerConfig>,
}

/// Shows the dialog; returns the new (already saved) configuration if the user pressed OK.
pub unsafe fn show(owner: HWND, current: &ViewerConfig) -> Option<ViewerConfig> {
    dialog::register_class(CLASS_NAME, Some(wnd_proc));
    let dlg = dialog::create_frame(owner, CLASS_NAME, "Настройки Mediares", 440, 410)?;

    let ctx = Box::into_raw(Box::new(Context {
        config: current.clone(),
        color: current.osd_font_color,
        loupe_scales: with_current(LOUPE_SCALES, current.loupe_scale, |a, b| (a - b).abs() < 0.05),
        font_sizes: with_current(FONT_SIZES, current.osd_font_size, |a, b| a == b),
        preview_brush: GdiObject(CreateSolidBrush(COLORREF(0))),
        result: None,
    }));
    SetWindowLongPtrW(dlg, GWLP_USERDATA, ctx as isize);

    let font = dialog::create_font(w!("Segoe UI"), -12);
    let ok = build_controls(dlg, &*ctx, font.0);
    dialog::run_modal(dlg, ok);

    // run_modal returns only after the window is destroyed, so nothing references `ctx` anymore.
    Box::from_raw(ctx).result
}

/// Standard options plus the configured value if it isn't one of them (e.g. edited in the INI).
fn with_current<T: Copy + PartialOrd>(standard: &[T], current: T, same: impl Fn(T, T) -> bool) -> Vec<T> {
    let mut options = standard.to_vec();
    if !options.iter().any(|&o| same(o, current)) {
        options.push(current);
        options.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    }
    options
}

unsafe fn build_controls(dlg: HWND, ctx: &Context, font: windows::Win32::Graphics::Gdi::HFONT) -> HWND {
    let cfg = &ctx.config;
    let tab = WS_TABSTOP.0;
    let control = |class, text: &str, style: u32, rect, id: i32| dialog::control(dlg, class, text, style, rect, id as usize, font);
    let checkbox = |text: &str, rect, id: i32, checked: bool| {
        control(w!("BUTTON"), text, tab | BS_AUTOCHECKBOX as u32, rect, id);
        if checked {
            SendDlgItemMessageW(dlg, id, BM_SETCHECK, WPARAM(BST_CHECKED), LPARAM(0));
        }
    };
    let combo = |rect, id: i32, items: Vec<String>, selected: usize| {
        control(w!("COMBOBOX"), "", tab | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32, rect, id);
        for item in items {
            let text = HSTRING::from(item);
            SendDlgItemMessageW(dlg, id, CB_ADDSTRING, WPARAM(0), LPARAM(text.as_ptr() as isize));
        }
        SendDlgItemMessageW(dlg, id, CB_SETCURSEL, WPARAM(selected), LPARAM(0));
    };

    checkbox("Запускать в полноэкранном режиме", (20, 15, 380, 22), IDC_START_FULLSCREEN, cfg.start_fullscreen);
    checkbox("Автоповорот по ориентации EXIF", (20, 42, 380, 22), IDC_AUTO_ROTATE_EXIF, cfg.auto_rotate_exif);

    control(w!("BUTTON"), "Лупа (удержание ЛКМ)", BS_GROUPBOX as u32, (15, 72, 395, 62), 0);
    control(w!("STATIC"), "Масштаб лупы:", SS_LEFT, (28, 98, 140, 20), 0);
    let loupe_labels = ctx.loupe_scales.iter().map(|s| format!("{}:1", s).replace('.', ",")).collect();
    let loupe_sel = ctx.loupe_scales.iter().position(|s| (s - cfg.loupe_scale).abs() < 0.05).unwrap_or(0);
    combo((175, 95, 90, 160), IDC_LOUPE_SCALE, loupe_labels, loupe_sel);

    control(w!("BUTTON"), "Информационная строка (OSD)", BS_GROUPBOX as u32, (15, 145, 395, 165), 0);
    checkbox("Отображать OSD (разрешение, масштаб, размер)", (28, 172, 365, 22), IDC_SHOW_OSD, cfg.show_osd);
    control(w!("STATIC"), "Размер шрифта:", SS_LEFT, (28, 207, 140, 20), 0);
    let size_labels = ctx.font_sizes.iter().map(|s| format!("{} pt", s)).collect();
    let size_sel = ctx.font_sizes.iter().position(|&s| s == cfg.osd_font_size).unwrap_or(0);
    combo((175, 204, 90, 200), IDC_FONT_SIZE, size_labels, size_sel);

    control(w!("STATIC"), "Цвет шрифта:", SS_LEFT, (28, 245, 140, 20), 0);
    control(w!("BUTTON"), "Выбрать цвет...", tab | BS_PUSHBUTTON as u32, (175, 242, 130, 26), IDC_CHOOSE_COLOR);
    control(w!("STATIC"), "Aa", SS_CENTER, (320, 242, 45, 26), IDC_COLOR_PREVIEW);

    let ok = control(w!("BUTTON"), "ОК", tab | BS_DEFPUSHBUTTON as u32, (205, 325, 95, 28), IDOK.0);
    control(w!("BUTTON"), "Отмена", tab | BS_PUSHBUTTON as u32, (315, 325, 95, 28), IDCANCEL.0);
    ok
}

unsafe fn is_checked(dlg: HWND, id: i32) -> bool {
    SendDlgItemMessageW(dlg, id, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize == BST_CHECKED
}

unsafe fn selected<T: Copy>(dlg: HWND, id: i32, options: &[T]) -> Option<T> {
    let idx = SendDlgItemMessageW(dlg, id, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
    usize::try_from(idx).ok().and_then(|i| options.get(i).copied())
}

unsafe fn accept(dlg: HWND, ctx: &mut Context) {
    let cfg = &mut ctx.config;
    cfg.start_fullscreen = is_checked(dlg, IDC_START_FULLSCREEN);
    cfg.auto_rotate_exif = is_checked(dlg, IDC_AUTO_ROTATE_EXIF);
    cfg.show_osd = is_checked(dlg, IDC_SHOW_OSD);
    cfg.loupe_scale = selected(dlg, IDC_LOUPE_SCALE, &ctx.loupe_scales).unwrap_or(cfg.loupe_scale);
    cfg.osd_font_size = selected(dlg, IDC_FONT_SIZE, &ctx.font_sizes).unwrap_or(cfg.osd_font_size);
    cfg.osd_font_color = ctx.color;
    cfg.save();
    ctx.result = Some(cfg.clone());
}

unsafe fn choose_color(dlg: HWND, ctx: &mut Context) {
    let mut custom = *CUSTOM_COLORS.lock().unwrap_or_else(|e| e.into_inner());
    let mut cc = CHOOSECOLORW {
        lStructSize: size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: dlg,
        rgbResult: COLORREF(ctx.color),
        lpCustColors: custom.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        ..Default::default()
    };
    if ChooseColorW(&mut cc).as_bool() {
        ctx.color = cc.rgbResult.0;
        if let Ok(preview) = GetDlgItem(Some(dlg), IDC_COLOR_PREVIEW) {
            let _ = InvalidateRect(Some(preview), None, true);
        }
    }
    *CUSTOM_COLORS.lock().unwrap_or_else(|e| e.into_inner()) = custom;
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    mediares_core::ffi::guard(LRESULT(0), || unsafe {
        let ctx = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Context).as_mut();
        let Some(ctx) = ctx else {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        };

        match msg {
            WM_CTLCOLORSTATIC if GetDlgCtrlID(HWND(lparam.0 as *mut _)) == IDC_COLOR_PREVIEW => {
                let hdc = HDC(wparam.0 as *mut _);
                SetBkMode(hdc, OPAQUE);
                SetBkColor(hdc, COLORREF(0));
                SetTextColor(hdc, COLORREF(ctx.color));
                LRESULT(ctx.preview_brush.0 .0 as isize)
            }
            WM_COMMAND => {
                match dialog::loword(wparam) as i32 {
                    IDC_CHOOSE_COLOR => choose_color(hwnd, ctx),
                    id if id == IDOK.0 => {
                        accept(hwnd, ctx);
                        dialog::close(hwnd);
                    }
                    id if id == IDCANCEL.0 => dialog::close(hwnd),
                    _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                dialog::close(hwnd);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_value_is_kept_in_options() {
        assert_eq!(with_current(LOUPE_SCALES, 2.0, |a, b| a == b), LOUPE_SCALES);
        assert_eq!(with_current(FONT_SIZES, 15, |a, b| a == b)[3], 15);
        assert!(with_current(LOUPE_SCALES, 4.0, |a, b| a == b).contains(&4.0));
    }
}
