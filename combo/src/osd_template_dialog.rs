//! Modal editor of an OSD template (see [`crate::osd_template`]).

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DefWindowProcW, DestroyMenu, GetDlgItem, GetDlgItemTextW,
    GetWindowLongPtrW, GetWindowRect, SendDlgItemMessageW, SetDlgItemTextW, SetWindowLongPtrW,
    TrackPopupMenu, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, ES_AUTOVSCROLL, ES_MULTILINE, GWLP_USERDATA,
    IDCANCEL, IDOK, MF_POPUP, MF_STRING, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_TOPALIGN, WM_CLOSE,
    WM_COMMAND, WM_NCDESTROY, WS_BORDER, WS_HSCROLL, WS_TABSTOP, WS_VSCROLL,
};

use crate::dialog;
use crate::i18n::tr;
use crate::osd_template::FieldGroups;

const CLASS_NAME: PCWSTR = w!("MediaresOsdTemplateDialogClass");

const IDC_TEMPLATE: i32 = 301;
const IDC_ADD_FIELD: i32 = 302;
const IDC_DEFAULT: i32 = 303;

const ES_AUTOHSCROLL: u32 = 0x0080;
const ES_WANTRETURN: u32 = 0x1000;
const EM_REPLACESEL: u32 = 0x00C2;
const SS_LEFT: u32 = 0x0000;

/// Menu command of field `i` in group `g`.
const FIELD_CMD_BASE: usize = 1000;
const FIELDS_PER_GROUP: usize = 100;

struct Context {
    fields: FieldGroups,
    default: &'static str,
    result: Option<String>,
}

/// Edits `current`; the new template if the user pressed OK.
pub unsafe fn show(
    owner: HWND,
    title: &str,
    current: &str,
    default: &'static str,
    fields: FieldGroups,
) -> Option<String> {
    dialog::register_class(CLASS_NAME, Some(wnd_proc));
    let dlg = dialog::create_frame(owner, CLASS_NAME, title, 660, 390)?;
    let ctx = Box::into_raw(Box::new(Context {
        fields,
        default,
        result: None,
    }));
    SetWindowLongPtrW(dlg, GWLP_USERDATA, ctx as isize);

    let font = crate::gdi::create_font("Segoe UI", -12, false);
    let mono = crate::gdi::create_font("Consolas", -14, false);
    let tab = WS_TABSTOP.0;
    let hint = tr(
        "{поле} — значение поля. <…> — блок, который скрывается, если в нём есть пустое поле.\n\
                {{ }} << >> — сами символы. Enter — новая строка OSD.",
        "{field} — the field's value. <…> — a block hidden when a field in it is empty.\n\
                {{ }} << >> — the characters themselves. Enter — a new OSD line.",
    );
    dialog::control(
        dlg,
        w!("STATIC"),
        hint,
        SS_LEFT,
        (15, 12, 620, 36),
        0,
        font.0,
    );
    let edit_style = tab
        | WS_BORDER.0
        | WS_VSCROLL.0
        | WS_HSCROLL.0
        | (ES_MULTILINE | ES_AUTOVSCROLL) as u32
        | ES_AUTOHSCROLL
        | ES_WANTRETURN;
    let edit = dialog::control(
        dlg,
        w!("EDIT"),
        &to_edit(current),
        edit_style,
        (15, 55, 615, 220),
        IDC_TEMPLATE as usize,
        mono.0,
    );
    let button = |text: &str, x: i32, w: i32, id: i32, style: u32| {
        dialog::control(
            dlg,
            w!("BUTTON"),
            text,
            tab | style,
            (x, 290, w, 28),
            id as usize,
            font.0,
        );
    };
    button(
        tr("Добавить поле...", "Add field..."),
        15,
        140,
        IDC_ADD_FIELD,
        BS_PUSHBUTTON as u32,
    );
    button(
        tr("По умолчанию", "Default"),
        165,
        125,
        IDC_DEFAULT,
        BS_PUSHBUTTON as u32,
    );
    button(tr("ОК", "OK"), 425, 95, IDOK.0, BS_DEFPUSHBUTTON as u32);
    button(
        tr("Отмена", "Cancel"),
        535,
        95,
        IDCANCEL.0,
        BS_PUSHBUTTON as u32,
    );

    dialog::run_modal(dlg, edit);
    // run_modal returns only after the window is destroyed, so nothing references `ctx` anymore.
    Box::from_raw(ctx).result
}

/// The edit control wants CR LF line breaks; templates keep plain LF.
fn to_edit(template: &str) -> String {
    template.replace("\r\n", "\n").replace('\n', "\r\n")
}

unsafe fn edit_text(dlg: HWND) -> String {
    let mut buf = vec![0u16; 16384];
    let len = GetDlgItemTextW(dlg, IDC_TEMPLATE, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..len]).replace("\r\n", "\n")
}

/// "Добавить поле...": a menu of the fields by group; the chosen `{key}` goes in at the caret.
unsafe fn add_field(dlg: HWND, ctx: &Context) {
    let Ok(menu) = CreatePopupMenu() else { return };
    for (g, (group, fields)) in ctx.fields.iter().enumerate() {
        let Ok(sub) = CreatePopupMenu() else { continue };
        for (i, field) in fields.iter().enumerate() {
            let label = HSTRING::from(format!("{}\t{{{}}}", field.label(), field.key));
            let _ = AppendMenuW(
                sub,
                MF_STRING,
                FIELD_CMD_BASE + g * FIELDS_PER_GROUP + i,
                &label,
            );
        }
        // The submenu is destroyed with its parent.
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            sub.0 as usize,
            &HSTRING::from(tr(group.0, group.1)),
        );
    }

    let mut rc = RECT::default();
    if let Ok(button) = GetDlgItem(Some(dlg), IDC_ADD_FIELD) {
        let _ = GetWindowRect(button, &mut rc);
    }
    let at = POINT {
        x: rc.left,
        y: rc.bottom,
    };
    let cmd = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RETURNCMD,
        at.x,
        at.y,
        Some(0),
        dlg,
        None,
    )
    .0 as usize;
    let _ = DestroyMenu(menu);

    let chosen = cmd.checked_sub(FIELD_CMD_BASE).and_then(|n| {
        let (_, fields) = ctx.fields.get(n / FIELDS_PER_GROUP)?;
        fields.get(n % FIELDS_PER_GROUP)
    });
    if let Some(field) = chosen {
        let text = HSTRING::from(format!("{{{}}}", field.key));
        SendDlgItemMessageW(
            dlg,
            IDC_TEMPLATE,
            EM_REPLACESEL,
            WPARAM(1),
            LPARAM(text.as_ptr() as isize),
        );
    }
    focus_edit(dlg);
}

unsafe fn focus_edit(dlg: HWND) {
    if let Ok(edit) = GetDlgItem(Some(dlg), IDC_TEMPLATE) {
        let _ = SetFocus(Some(edit));
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    mediares_core::ffi::guard(LRESULT(0), || unsafe {
        let Some(ctx) = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Context).as_mut() else {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        };
        match msg {
            WM_COMMAND => {
                match dialog::loword(wparam) as i32 {
                    IDC_ADD_FIELD => add_field(hwnd, ctx),
                    IDC_DEFAULT => {
                        let _ = SetDlgItemTextW(
                            hwnd,
                            IDC_TEMPLATE,
                            &HSTRING::from(to_edit(ctx.default)),
                        );
                        focus_edit(hwnd);
                    }
                    id if id == IDOK.0 => {
                        let text = edit_text(hwnd);
                        ctx.result = Some(if text.trim().is_empty() {
                            ctx.default.to_string()
                        } else {
                            text
                        });
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
    fn line_breaks_for_edit() {
        assert_eq!(to_edit("a\nb\r\nc"), "a\r\nb\r\nc");
    }

    #[test]
    fn field_commands_fit() {
        for fields in [
            crate::osd_template::PHOTO_FIELDS,
            crate::osd_template::VIDEO_FIELDS,
        ] {
            assert!(fields.iter().all(|(_, f)| f.len() < FIELDS_PER_GROUP));
        }
    }
}
