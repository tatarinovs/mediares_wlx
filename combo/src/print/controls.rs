//! Printer, placement and size controls of the print preview.

use super::*;

pub(super) unsafe fn build(
    dlg: HWND,
    data: &mut Preview,
    margins: Option<RECT>,
    font: &crate::gdi::Font,
    printers: Vec<String>,
) -> (HWND, crate::gdi::Font) {
    let mut rc = RECT::default();
    let _ = GetClientRect(dlg, &mut rc);

    let px = |v| dialog::px(dlg, v);
    let right_x = rc.right - px(SIDEBAR_WIDTH) + px(8);
    let ctrl_w = px(SIDEBAR_WIDTH - 20);
    // The layout below is in device pixels, like the preview painting, so create the controls
    // at those coordinates directly (`dialog::control` would scale them a second time).
    let control = |class, text: &str, style, bounds, id| {
        dialog::control_px(dlg, class, text, style, bounds, id, font.0)
    };
    let small_font = dialog::font(dlg, "Segoe UI", -13);
    data.caption_font = small_font.0;
    let label = |text: &str, bounds, id| {
        dialog::control_px(
            dlg,
            w!("STATIC"),
            text,
            SS_CENTER | SS_CENTERIMAGE,
            bounds,
            id,
            small_font.0,
        )
    };
    let left_label = |text: &str, bounds, id| {
        dialog::control_px(
            dlg,
            w!("STATIC"),
            text,
            dialog::SS_LEFT | SS_CENTERIMAGE,
            bounds,
            id,
            small_font.0,
        )
    };
    let group = |title: &str, x: i32, y: i32, w: i32, h: i32| {
        control(w!("BUTTON"), title, BS_GROUPBOX as u32, (x, y, w, h), 0);
    };

    // Принтер и бумага
    group(tr("Printer"), right_x, px(6), ctrl_w, px(88));
    control(
        w!("COMBOBOX"),
        "",
        WS_TABSTOP.0 | CBS_DROPDOWNLIST as u32,
        (right_x + px(9), px(24), ctrl_w - px(190), px(240)),
        PRINTER_COMBO,
    );
    control(
        w!("BUTTON"),
        "↻",
        WS_TABSTOP.0,
        (right_x + ctrl_w - px(175), px(23), px(30), px(26)),
        PRINTER_REFRESH,
    );
    control(
        w!("BUTTON"),
        tr("Print settings"),
        WS_TABSTOP.0,
        (right_x + ctrl_w - px(139), px(23), px(130), px(26)),
        PRINTER_PROPS,
    );
    left_label(tr("Paper:"), (right_x + px(9), px(57), px(52), px(18)), 0);
    control(
        w!("COMBOBOX"),
        "",
        WS_TABSTOP.0 | CBS_DROPDOWNLIST as u32,
        (right_x + px(63), px(54), ctrl_w - px(72), px(220)),
        PAPER_INFO as usize,
    );
    fill_printers(dlg, data, printers);

    let left_x = right_x;
    let left_w = (ctrl_w - px(8)) / 2;
    let right_col = left_x + left_w + px(8);
    let right_w = ctrl_w - left_w - px(8);

    // Поля (мм)
    group(tr("Margins (mm)"), left_x, px(100), left_w, px(150));
    let m = margins.unwrap_or(RECT {
        left: DEFAULT_MARGIN_MM * MM100_PER_MM,
        top: DEFAULT_MARGIN_MM * MM100_PER_MM,
        right: DEFAULT_MARGIN_MM * MM100_PER_MM,
        bottom: DEFAULT_MARGIN_MM * MM100_PER_MM,
    });

    let spin_style = WS_VISIBLE.0
        | WS_CHILD.0
        | UDS_ALIGNRIGHT
        | UDS_ARROWKEYS
        | UDS_NOTHOUSANDS
        | UDS_SETBUDDYINT;

    // Крестовина полей: сверху и снизу по центру, слева и справа у краёв группы.
    let field_w = px(78);
    let field_h = px(24);
    let label_w = px(78);
    let label_h = px(17);
    let top_x = left_x + (left_w - field_w) / 2;
    let mid_y = px(162);

    let positions = [
        (top_x, px(122)),                            // Сверху
        (left_x + px(12), mid_y),                    // Слева
        (left_x + left_w - px(12) - field_w, mid_y), // Справа
        (top_x, px(202)),                            // Снизу
    ];
    let values = [
        m.top as f64 / MM100_PER_MM as f64,
        m.left as f64 / MM100_PER_MM as f64,
        m.right as f64 / MM100_PER_MM as f64,
        m.bottom as f64 / MM100_PER_MM as f64,
    ];
    let ids = [
        MARGIN_FIRST,
        MARGIN_FIRST + 1,
        MARGIN_FIRST + 2,
        MARGIN_FIRST + 3,
    ];
    let labels = [tr("Top"), tr("Left"), tr("Right"), tr("Bottom")];

    for i in 0..4 {
        let (x, y) = positions[i];
        let edit_h = control(
            w!("EDIT"),
            &format_mm(values[i]),
            WS_TABSTOP.0 | WS_BORDER.0 | dialog::ES_AUTOHSCROLL,
            (x, y, field_w, field_h),
            ids[i],
        );

        let spin_id = SPIN_FIRST + i;
        let spin_h = control(w!("msctls_updown32"), "", spin_style, (0, 0, 0, 0), spin_id);

        SendMessageW(spin_h, UDM_SETBUDDY, Some(WPARAM(edit_h.0 as usize)), None);
        SendMessageW(spin_h, UDM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(300)));
        SendMessageW(spin_h, UDM_SETPOS32, None, Some(LPARAM(values[i] as isize)));

        let _ = SetWindowTextW(edit_h, &HSTRING::from(format_mm(values[i])));

        // Подпись шире поля и центрируется относительно него.
        let lx = x + (field_w - label_w) / 2;
        label(labels[i], (lx, y + field_h, label_w, label_h), 0);
    }

    // Расположение (сетка 3x3)
    group(tr("Placement"), right_col, px(100), right_w, px(150));
    let button_w = px(42);
    let button_h = px(36);
    let gap = px(4);
    let grid_x = right_col + (right_w - (button_w * 3 + gap * 2)) / 2;
    for i in 0..9 {
        let x = grid_x + (i % 3) as i32 * (button_w + gap);
        let y = px(122) + (i / 3) as i32 * (button_h + gap);
        let rows = [tr("Top"), tr("Center"), tr("Bottom")];
        let cols = [tr("Left"), tr("Center"), tr("Right")];
        let name = if i == 4 {
            tr("Center").to_string()
        } else if i / 3 == 1 {
            cols[i % 3].to_string()
        } else if i % 3 == 1 {
            rows[i / 3].to_string()
        } else {
            format!("{} {}", rows[i / 3], cols[i % 3])
        };
        control(
            w!("BUTTON"),
            &name,
            WS_TABSTOP.0 | BS_OWNERDRAW as u32,
            (x, y, button_w, button_h),
            ALIGN_FIRST + i,
        );
    }

    // Размер
    group(tr("Size"), left_x, px(256), left_w, px(210));
    let modes = [
        tr("Original size"),
        tr("Fit to page"),
        tr("Stretch to page"),
        tr("Specified size"),
    ];
    for (i, mode_label) in modes.into_iter().enumerate() {
        control(
            w!("BUTTON"),
            mode_label,
            WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32 | if i == 0 { WS_GROUP.0 } else { 0 },
            (
                left_x + px(12),
                px(276) + i as i32 * px(24),
                left_w - px(22),
                px(22),
            ),
            MODE_FIRST + i,
        );
    }

    // "Указанный размер": по строке на ширину и высоту, чтобы спинбоксам и трём цифрам
    // было просторно.
    let edit_cw = px(104);
    for (i, (text, value, label_id, edit_id, spin_id, unit_id)) in [
        (
            tr("W:"),
            100,
            CUSTOM_W_LABEL,
            CUSTOM_WIDTH,
            CUSTOM_W_SPIN,
            CUSTOM_W_UNIT,
        ),
        (
            tr("H:"),
            150,
            CUSTOM_H_LABEL,
            CUSTOM_HEIGHT,
            CUSTOM_H_SPIN,
            CUSTOM_H_UNIT,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let y = px(376) + i as i32 * px(30);
        left_label(text, (left_x + px(14), y + px(3), px(20), px(18)), label_id);
        let edit = control(
            w!("EDIT"),
            &value.to_string(),
            WS_TABSTOP.0 | WS_BORDER.0 | dialog::ES_AUTOHSCROLL,
            (left_x + px(38), y, edit_cw, px(24)),
            edit_id,
        );
        let spin = control(w!("msctls_updown32"), "", spin_style, (0, 0, 0, 0), spin_id);
        SendMessageW(spin, UDM_SETBUDDY, Some(WPARAM(edit.0 as usize)), None);
        SendMessageW(spin, UDM_SETRANGE32, Some(WPARAM(1)), Some(LPARAM(1000)));
        SendMessageW(spin, UDM_SETPOS32, None, Some(LPARAM(value)));
        left_label(
            tr("mm"),
            (left_x + px(148), y + px(3), px(26), px(18)),
            unit_id,
        );
    }

    // Заполнить рамку, обрезав выходящие за неё части.
    control(
        w!("BUTTON"),
        tr("Fill and crop"),
        WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        (left_x + px(12), px(436), left_w - px(22), px(23)),
        AUTO_CROP,
    );

    SendDlgItemMessageW(
        dlg,
        (MODE_FIRST + 1) as i32,
        BM_SETCHECK,
        WPARAM(BST_CHECKED.0 as usize),
        LPARAM(0),
    );
    update_mode_controls_visibility(dlg);

    // Количество копий
    group(tr("Copies"), right_col, px(256), right_w, px(66));
    left_label(
        tr("Copies:"),
        (right_col + px(12), px(282), px(90), px(18)),
        0,
    );
    let copies_edit = control(
        w!("EDIT"),
        "1",
        WS_TABSTOP.0 | WS_BORDER.0 | ES_NUMBER as u32,
        (right_col + right_w - px(82), px(278), px(70), px(24)),
        COPIES,
    );
    let copies_spin = control(
        w!("msctls_updown32"),
        "",
        spin_style,
        (0, 0, 0, 0),
        SPIN_FIRST + 4,
    );
    SendMessageW(
        copies_spin,
        UDM_SETBUDDY,
        Some(WPARAM(copies_edit.0 as usize)),
        None,
    );
    SendMessageW(
        copies_spin,
        UDM_SETRANGE32,
        Some(WPARAM(1)),
        Some(LPARAM(99)),
    );
    SendMessageW(copies_spin, UDM_SETPOS32, None, Some(LPARAM(1)));

    // Ориентация
    group(tr("Orientation"), right_col, px(328), right_w, px(138));
    for (i, orientation) in [tr("Portrait"), tr("Landscape")].into_iter().enumerate() {
        control(
            w!("BUTTON"),
            orientation,
            WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32 | if i == 0 { WS_GROUP.0 } else { 0 },
            (
                right_col + px(12),
                px(350) + i as i32 * px(28),
                right_w - px(22),
                px(22),
            ),
            ORIENTATION + i,
        );
    }
    control(
        w!("BUTTON"),
        tr("Auto rotate"),
        WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        (right_col + px(12), px(416), right_w - px(22), px(23)),
        AUTO_ROTATE,
    );

    control(
        w!("STATIC"),
        "",
        dialog::SS_LEFT,
        (right_x + px(12), px(480), ctrl_w - px(24), px(52)),
        STATUS,
    );

    // Футер
    let footer_y = rc.bottom - px(46);
    control(
        w!("BUTTON"),
        tr("Cancel"),
        WS_TABSTOP.0,
        (right_x + ctrl_w - px(144), footer_y, px(134), px(30)),
        IDCANCEL.0 as usize,
    );
    let print_button = control(
        w!("BUTTON"),
        tr("Print"),
        WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
        (right_x + ctrl_w - px(260), footer_y, px(108), px(30)),
        IDOK.0 as usize,
    );

    (print_button, small_font)
}
