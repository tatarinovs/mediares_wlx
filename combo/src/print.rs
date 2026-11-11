//! One-page print preview and GDI printing, using the selected printer's paper geometry.

use std::mem::size_of;
use std::ptr::null_mut;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HGLOBAL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Printing::*;
use windows::Win32::Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dialog;
use crate::i18n::tr;
use crate::image_cache::DecodedImage;
use crate::image_view::{draw_fitted, draw_stretched};

const CLASS: PCWSTR = w!("MediaresPrintPreview");

/// Static-control styles the `windows` crate doesn't export.
const SS_CENTER: u32 = 0x0001;
const SS_CENTERIMAGE: u32 = 0x0200;

/// The accent of the selected placement button, and the neutral border of the others.
const ACCENT: u32 = 0x00D77800; // RGB(0, 120, 215)
const ACCENT_SOFT: u32 = 0x00FBF1E5; // RGB(229, 241, 251)
const NEUTRAL: u32 = 0x00D0D0D0; // RGB(208, 208, 208)

const PRINTER_COMBO: usize = 100;
const PRINTER_PROPS: usize = 101;
const ORIENTATION: usize = 102;
const MARGIN_FIRST: usize = 110;
const COPIES: usize = 116;
const PAPER_INFO: i32 = 120;
const SPIN_FIRST: usize = 130;
const ALIGN_FIRST: usize = 150;
const MODE_FIRST: usize = 170;
const AUTO_CROP: usize = 176;
const AUTO_ROTATE: usize = 177;
const PRINTER_REFRESH: usize = 178;
const CUSTOM_WIDTH: usize = 180;
const CUSTOM_HEIGHT: usize = 181;
const CUSTOM_W_LABEL: usize = 182;
const CUSTOM_H_LABEL: usize = 183;
const CUSTOM_W_SPIN: usize = 184;
const CUSTOM_H_SPIN: usize = 185;
const CUSTOM_W_UNIT: usize = 186;
const CUSTOM_H_UNIT: usize = 187;

const SIDEBAR_WIDTH: i32 = 510;
const DEFAULT_MARGIN_MM: i32 = 10;
const MM100_PER_MM: i32 = 100;
const MM100_PER_INCH: i64 = 2540;

#[link(name = "winspool")]
unsafe extern "system" {
    #[link_name = "DeviceCapabilitiesW"]
    fn printer_device_capabilities(
        device: PCWSTR,
        port: PCWSTR,
        capability: u16,
        output: *mut u16,
        devmode: *const DEVMODEW,
    ) -> i32;
}
const DC_PAPERS_ID: u16 = 2;
const DC_PAPERSIZE_ID: u16 = 3;
const DC_PAPERNAMES_ID: u16 = 16;

#[derive(Clone)]
struct PaperChoice {
    id: u16,
    label: String,
}

unsafe fn paper_choices(printer: &PrinterState) -> Vec<PaperChoice> {
    let name = HSTRING::from(printer.name.as_str());
    let devmode = if printer.devmode.is_invalid() {
        std::ptr::null_mut()
    } else {
        GlobalLock(printer.devmode).cast::<DEVMODEW>()
    };
    let device = PCWSTR(name.as_ptr());
    let port = PCWSTR::null();
    let count = printer_device_capabilities(device, port, DC_PAPERS_ID, null_mut(), devmode);
    let result = if (1..=1024).contains(&count) {
        let n = count as usize;
        let mut ids = vec![0u16; n];
        let mut names = vec![0u16; n * 64];
        let mut sizes = vec![0u16; n * 4];
        let a = printer_device_capabilities(device, port, DC_PAPERS_ID, ids.as_mut_ptr(), devmode);
        let b = printer_device_capabilities(
            device,
            port,
            DC_PAPERNAMES_ID,
            names.as_mut_ptr(),
            devmode,
        );
        let c =
            printer_device_capabilities(device, port, DC_PAPERSIZE_ID, sizes.as_mut_ptr(), devmode);
        if a != count || b != count {
            Vec::new()
        } else {
            let dimensions = if c == count {
                Some(std::slice::from_raw_parts(
                    sizes.as_ptr().cast::<i32>(),
                    n * 2,
                ))
            } else {
                None
            };
            (0..n)
                .map(|i| {
                    let raw = &names[i * 64..(i + 1) * 64];
                    let end = raw.iter().position(|&ch| ch == 0).unwrap_or(64);
                    let mut label = String::from_utf16_lossy(&raw[..end]).trim().to_string();
                    if label.is_empty() {
                        label = format!("Paper {}", ids[i]);
                    }
                    if let Some(dim) = dimensions {
                        let w = dim[i * 2];
                        let h = dim[i * 2 + 1];
                        if w > 0 && h > 0 {
                            label.push_str(&format!(
                                " ({} × {} mm)",
                                w as f64 / 10.0,
                                h as f64 / 10.0
                            ));
                        }
                    }
                    PaperChoice { id: ids[i], label }
                })
                .collect()
        }
    } else {
        Vec::new()
    };
    if !devmode.is_null() {
        let _ = GlobalUnlock(printer.devmode);
    }
    result
}

unsafe fn selected_paper_id(printer: &PrinterState) -> Option<u16> {
    if printer.devmode.is_invalid() {
        return None;
    }
    let mode = GlobalLock(printer.devmode).cast::<DEVMODEW>();
    if mode.is_null() {
        return None;
    }
    let id = (*mode).Anonymous1.Anonymous1.dmPaperSize as u16;
    let _ = GlobalUnlock(printer.devmode);
    Some(id)
}

unsafe fn refresh_papers(dlg: HWND, data: &mut Preview<'_>) {
    data.papers = paper_choices(&data.printer);
    let combo = GetDlgItem(Some(dlg), PAPER_INFO).unwrap_or_default();
    SendMessageW(combo, CB_RESETCONTENT, None, None);
    let current = selected_paper_id(&data.printer);
    for (i, paper) in data.papers.iter().enumerate() {
        let label = HSTRING::from(paper.label.as_str());
        SendMessageW(
            combo,
            CB_ADDSTRING,
            None,
            Some(LPARAM(label.as_ptr() as isize)),
        );
        if Some(paper.id) == current {
            SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(i)), None);
        }
    }
    if !data.papers.is_empty() {
        let selection = SendMessageW(combo, CB_GETCURSEL, None, None).0;
        if selection < 0 {
            SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(0)), None);
        }
    } else {
        let pw = GetDeviceCaps(Some(data.printer.dc), PHYSICALWIDTH);
        let ph = GetDeviceCaps(Some(data.printer.dc), PHYSICALHEIGHT);
        let dx = GetDeviceCaps(Some(data.printer.dc), LOGPIXELSX).max(1);
        let dy = GetDeviceCaps(Some(data.printer.dc), LOGPIXELSY).max(1);
        let label = format!(
            "{:.0} × {:.0} mm",
            pw as f64 * 25.4 / dx as f64,
            ph as f64 * 25.4 / dy as f64
        );
        let text = HSTRING::from(label);
        SendMessageW(
            combo,
            CB_ADDSTRING,
            None,
            Some(LPARAM(text.as_ptr() as isize)),
        );
        SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(0)), None);
    }
    let _ = EnableWindow(combo, !data.papers.is_empty());
}

struct PrinterState {
    name: String,
    dc: HDC,
    devmode: HGLOBAL,
}

impl Drop for PrinterState {
    fn drop(&mut self) {
        unsafe {
            if !self.dc.is_invalid() {
                let _ = DeleteDC(self.dc);
            }
            if !self.devmode.is_invalid() {
                let _ = windows::Win32::Foundation::GlobalFree(Some(self.devmode));
            }
        }
    }
}

impl PrinterState {
    unsafe fn create(name: &str) -> Option<Self> {
        let name_h = HSTRING::from(name);
        let mut handle = PRINTER_HANDLE::default();
        if OpenPrinterW(PCWSTR(name_h.as_ptr()), &mut handle, None).is_err() {
            return None;
        }

        let needed = DocumentPropertiesW(None, handle, PCWSTR(name_h.as_ptr()), None, None, 0);

        let devmode = if needed > 0 {
            let mem = GlobalAlloc(GMEM_MOVEABLE, needed as usize).ok()?;
            let ptr = GlobalLock(mem).cast::<DEVMODEW>();
            if !ptr.is_null() {
                DocumentPropertiesW(
                    None,
                    handle,
                    PCWSTR(name_h.as_ptr()),
                    Some(ptr),
                    None,
                    DM_OUT_BUFFER.0,
                );
                let _ = GlobalUnlock(mem);
            }
            mem
        } else {
            HGLOBAL::default()
        };
        let _ = ClosePrinter(handle);

        let dev_ptr = if !devmode.is_invalid() {
            GlobalLock(devmode).cast::<DEVMODEW>()
        } else {
            null_mut()
        };

        let dc = CreateDCW(
            w!("WINSPOOL"),
            PCWSTR(name_h.as_ptr()),
            PCWSTR::null(),
            if dev_ptr.is_null() {
                None
            } else {
                Some(dev_ptr)
            },
        );

        if !dev_ptr.is_null() {
            let _ = GlobalUnlock(devmode);
        }

        if dc.is_invalid() {
            if !devmode.is_invalid() {
                let _ = windows::Win32::Foundation::GlobalFree(Some(devmode));
            }
            None
        } else {
            Some(Self {
                name: name.to_string(),
                dc,
                devmode,
            })
        }
    }

    unsafe fn configure(&mut self, landscape: Option<bool>) -> bool {
        if self.devmode.is_invalid() {
            return false;
        }
        let mode = GlobalLock(self.devmode).cast::<DEVMODEW>();
        if mode.is_null() {
            return false;
        }
        let old = *mode;
        (*mode).dmFields |= DM_COPIES;
        (*mode).Anonymous1.Anonymous1.dmCopies = 1;
        if let Some(landscape) = landscape {
            (*mode).dmFields |= DM_ORIENTATION;
            (*mode).Anonymous1.Anonymous1.dmOrientation = if landscape {
                DMORIENT_LANDSCAPE as i16
            } else {
                DMORIENT_PORTRAIT as i16
            };
        }
        let new_dc = ResetDCW(self.dc, mode);
        if new_dc.is_invalid() {
            *mode = old;
        } else {
            self.dc = new_dc;
        }
        let _ = GlobalUnlock(self.devmode);
        !new_dc.is_invalid()
    }

    unsafe fn set_paper(&mut self, paper_id: u16) -> bool {
        if self.devmode.is_invalid() {
            return false;
        }
        let mode = GlobalLock(self.devmode).cast::<DEVMODEW>();
        if mode.is_null() {
            return false;
        }
        let old = *mode;
        (*mode).dmFields |= DM_PAPERSIZE;
        (*mode).dmFields &= !(DM_PAPERWIDTH | DM_PAPERLENGTH);
        (*mode).Anonymous1.Anonymous1.dmPaperSize = paper_id as i16;
        let new_dc = ResetDCW(self.dc, mode);
        if new_dc.is_invalid() {
            *mode = old;
        } else {
            self.dc = new_dc;
        }
        let _ = GlobalUnlock(self.devmode);
        !new_dc.is_invalid()
    }

    unsafe fn open_properties(&mut self, owner: HWND) -> bool {
        let name_h = HSTRING::from(self.name.as_str());
        let mut handle = PRINTER_HANDLE::default();
        if OpenPrinterW(PCWSTR(name_h.as_ptr()), &mut handle, None).is_err() {
            return false;
        }
        let mode = GlobalLock(self.devmode).cast::<DEVMODEW>();
        let ok = if !mode.is_null() {
            let res = DocumentPropertiesW(
                Some(owner),
                handle,
                PCWSTR(name_h.as_ptr()),
                Some(mode),
                Some(mode),
                (DM_IN_PROMPT | DM_IN_BUFFER | DM_OUT_BUFFER).0,
            );
            let _ = GlobalUnlock(self.devmode);
            res == 1
        } else {
            false
        };
        let _ = ClosePrinter(handle);
        if ok {
            let mode = GlobalLock(self.devmode).cast::<DEVMODEW>();
            let new_dc = ResetDCW(self.dc, mode);
            if !new_dc.is_invalid() {
                self.dc = new_dc;
            }
            let _ = GlobalUnlock(self.devmode);
        }
        ok
    }
}

unsafe fn enumerate_printers() -> Vec<String> {
    let mut needed = 0u32;
    let mut returned = 0u32;
    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let _ = EnumPrintersW(flags, PCWSTR::null(), 4, None, &mut needed, &mut returned);
    if needed == 0 {
        return Vec::new();
    }
    let mut buffer = vec![0u8; needed as usize];
    if EnumPrintersW(
        flags,
        PCWSTR::null(),
        4,
        Some(buffer.as_mut_slice()),
        &mut needed,
        &mut returned,
    )
    .is_err()
    {
        return Vec::new();
    }
    let entries =
        std::slice::from_raw_parts(buffer.as_ptr().cast::<PRINTER_INFO_4W>(), returned as usize);
    entries
        .iter()
        .filter_map(|info| {
            if info.pPrinterName.is_null() {
                None
            } else {
                Some(info.pPrinterName.to_string().unwrap_or_default())
            }
        })
        .collect()
}

unsafe fn default_printer_name() -> Option<String> {
    let mut len = 0u32;
    let _ = GetDefaultPrinterW(None, &mut len);
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    if GetDefaultPrinterW(Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len).as_bool() {
        Some(String::from_utf16_lossy(
            &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())],
        ))
    } else {
        None
    }
}

struct Preview<'a> {
    printer: PrinterState,
    image: &'a DecodedImage,
    name: &'a str,
    printed: bool,
    papers: Vec<PaperChoice>,
    alignment: usize,
    /// The picture turned a quarter clockwise, made the first time "Auto rotate" needs it.
    rotated: Option<DecodedImage>,
}

impl Preview<'_> {
    /// The picture turned a quarter clockwise, for "Auto rotate".
    fn rotated(&mut self) -> &DecodedImage {
        let image = self.image;
        self.rotated
            .get_or_insert_with(|| crate::image_cache::rotated(image, true))
    }
}

/// The checked "Size" radio button (1 = "Fit to page", as the dialog starts).
unsafe fn selected_mode(dlg: HWND) -> usize {
    for i in 0..4 {
        let checked = SendDlgItemMessageW(
            dlg,
            (MODE_FIRST + i) as i32,
            BM_GETCHECK,
            WPARAM(0),
            LPARAM(0),
        )
        .0 == BST_CHECKED.0 as isize;
        if checked {
            return i;
        }
    }
    1
}

unsafe fn update_mode_controls_visibility(dlg: HWND) {
    let mode = selected_mode(dlg);
    let show = if mode == 3 { SW_SHOW } else { SW_HIDE };
    let controls = [
        CUSTOM_W_LABEL,
        CUSTOM_WIDTH,
        CUSTOM_W_SPIN,
        CUSTOM_W_UNIT,
        CUSTOM_H_LABEL,
        CUSTOM_HEIGHT,
        CUSTOM_H_SPIN,
        CUSTOM_H_UNIT,
    ];
    for id in controls {
        if let Ok(h) = GetDlgItem(Some(dlg), id as i32) {
            let _ = ShowWindow(h, show);
        }
    }
    // "Fill and crop" only changes how a picture is fitted into a frame; "Original size"
    // and "Stretch to page" have nothing to crop.
    if let Ok(crop) = GetDlgItem(Some(dlg), AUTO_CROP as i32) {
        let _ = EnableWindow(crop, mode == 1 || mode == 3);
    }
}

pub unsafe fn print(
    owner: HWND,
    img: &DecodedImage,
    doc_name: &str,
    margins: Option<RECT>,
) -> bool {
    let printers = enumerate_printers();
    let Some(mut printer) = default_printer_name()
        .as_deref()
        .or_else(|| printers.first().map(|s| s.as_str()))
        .and_then(|name| PrinterState::create(name))
        .or_else(|| printers.first().and_then(|p| PrinterState::create(p)))
    else {
        crate::file_actions::show_error(owner, tr("No default printer is available."));
        return false;
    };

    printer.configure(Some(img.width > img.height));

    dialog::register_class(CLASS, Some(wnd_proc));
    let Some(dlg) = dialog::create_frame(owner, CLASS, tr("Print preview"), 1040, 560) else {
        return false;
    };

    let style = GetWindowLongPtrW(dlg, GWL_STYLE) as u32;
    SetWindowLongPtrW(dlg, GWL_STYLE, (style | WS_CLIPCHILDREN.0) as isize);

    let font = dialog::font(dlg, "Segoe UI", -14);
    let mut data = Preview {
        printer,
        image: img,
        name: doc_name,
        printed: false,
        papers: Vec::new(),
        alignment: 4,
        rotated: None,
    };
    SetWindowLongPtrW(dlg, GWLP_USERDATA, &mut data as *mut _ as isize);

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
    let small_font = dialog::font(dlg, "Segoe UI", -12);
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
    let printer_combo = control(
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
    for (i, printer) in printers.iter().enumerate() {
        SendMessageW(
            printer_combo,
            CB_ADDSTRING,
            None,
            Some(LPARAM(HSTRING::from(printer.as_str()).as_ptr() as isize)),
        );
        if *printer == data.printer.name {
            SendMessageW(printer_combo, CB_SETCURSEL, Some(WPARAM(i)), None);
        }
    }

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
        | UDS_AUTOBUDDY
        | UDS_SETBUDDYINT;

    // Крестовина полей: сверху и снизу по центру, слева и справа у краёв группы.
    let field_w = px(58);
    let field_h = px(24);
    let label_w = px(78);
    let label_h = px(17);
    let top_x = left_x + (left_w - field_w) / 2;
    let mid_y = px(158);

    let positions = [
        (top_x, px(114)),                            // Сверху
        (left_x + px(12), mid_y),                    // Слева
        (left_x + left_w - px(12) - field_w, mid_y), // Справа
        (top_x, px(202)),                            // Снизу
    ];
    let values = [
        m.top / MM100_PER_MM,
        m.left / MM100_PER_MM,
        m.right / MM100_PER_MM,
        m.bottom / MM100_PER_MM,
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
            &values[i].to_string(),
            WS_TABSTOP.0 | WS_BORDER.0 | ES_NUMBER as u32 | dialog::ES_AUTOHSCROLL,
            (x, y, field_w, field_h),
            ids[i],
        );

        let spin_id = SPIN_FIRST + i;
        let spin_h = control(w!("msctls_updown32"), "", spin_style, (0, 0, 0, 0), spin_id);

        SendMessageW(spin_h, UDM_SETBUDDY, Some(WPARAM(edit_h.0 as usize)), None);
        SendMessageW(spin_h, UDM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(300)));
        SendMessageW(spin_h, UDM_SETPOS32, None, Some(LPARAM(values[i] as isize)));

        // Подпись шире поля и центрируется относительно него.
        let lx = x + (field_w - label_w) / 2;
        label(labels[i], (lx, y + field_h, label_w, label_h), 0);
    }

    // Расположение (сетка 3x3)
    group(tr("Placement"), right_col, px(100), right_w, px(150));
    let button_w = px(42);
    let button_h = px(38);
    let gap = px(6);
    let grid_x = right_col + (right_w - (button_w * 3 + gap * 2)) / 2;
    for i in 0..9 {
        let x = grid_x + (i % 3) as i32 * (button_w + gap);
        let y = px(114) + (i / 3) as i32 * (button_h + gap);
        control(
            w!("BUTTON"),
            "",
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
            WS_TABSTOP.0 | WS_BORDER.0 | ES_NUMBER as u32 | dialog::ES_AUTOHSCROLL,
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

    // Футер
    let footer_y = rc.bottom - px(46);
    control(
        w!("BUTTON"),
        tr("Cancel"),
        WS_TABSTOP.0,
        (right_x + ctrl_w - px(104), footer_y, px(94), px(30)),
        IDCANCEL.0 as usize,
    );
    let print_button = control(
        w!("BUTTON"),
        tr("Print"),
        WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
        (right_x + ctrl_w - px(210), footer_y, px(98), px(30)),
        IDOK.0 as usize,
    );

    sync_orientation(dlg, data.printer.dc);
    refresh_papers(dlg, &mut data);
    validate(dlg, &data);
    dialog::run_modal(dlg, print_button);
    data.printed
}

unsafe fn sync_orientation(dlg: HWND, dc: HDC) {
    let pw = GetDeviceCaps(Some(dc), PHYSICALWIDTH);
    let ph = GetDeviceCaps(Some(dc), PHYSICALHEIGHT);
    let dpi_x = GetDeviceCaps(Some(dc), LOGPIXELSX).max(1);
    let dpi_y = GetDeviceCaps(Some(dc), LOGPIXELSY).max(1);
    let landscape = (pw as i64 * dpi_y as i64) > (ph as i64 * dpi_x as i64);

    for i in 0..2 {
        SendDlgItemMessageW(
            dlg,
            (ORIENTATION + i) as i32,
            BM_SETCHECK,
            WPARAM(if i == usize::from(landscape) {
                BST_CHECKED.0 as usize
            } else {
                0
            }),
            LPARAM(0),
        );
    }
}

fn parse_margin(text: &str) -> Option<i32> {
    let mm = text.trim().replace(',', ".").parse::<f64>().ok()?;
    (mm.is_finite() && (0.0..=300.0).contains(&mm))
        .then(|| (mm * MM100_PER_MM as f64).round() as i32)
}

struct PrintOptions {
    margins: RECT,
    copies: u32,
    mode: usize,
    custom_size_mm: (f64, f64),
    auto_crop: bool,
    auto_rotate: bool,
}

unsafe fn options(dlg: HWND) -> Option<PrintOptions> {
    let top = parse_margin(&dialog::item_text(dlg, MARGIN_FIRST as i32))?;
    let left = parse_margin(&dialog::item_text(dlg, (MARGIN_FIRST + 1) as i32))?;
    let right = parse_margin(&dialog::item_text(dlg, (MARGIN_FIRST + 2) as i32))?;
    let bottom = parse_margin(&dialog::item_text(dlg, (MARGIN_FIRST + 3) as i32))?;

    let copies = dialog::item_text(dlg, COPIES as i32)
        .trim()
        .parse::<u32>()
        .ok()?;

    let mode = selected_mode(dlg);

    let custom_w = dialog::item_text(dlg, CUSTOM_WIDTH as i32)
        .trim()
        .replace(',', ".")
        .parse::<f64>()
        .unwrap_or(100.0)
        .max(1.0);
    let custom_h = dialog::item_text(dlg, CUSTOM_HEIGHT as i32)
        .trim()
        .replace(',', ".")
        .parse::<f64>()
        .unwrap_or(150.0)
        .max(1.0);

    let auto_crop = SendDlgItemMessageW(dlg, AUTO_CROP as i32, BM_GETCHECK, WPARAM(0), LPARAM(0)).0
        == BST_CHECKED.0 as isize;
    let auto_rotate =
        SendDlgItemMessageW(dlg, AUTO_ROTATE as i32, BM_GETCHECK, WPARAM(0), LPARAM(0)).0
            == BST_CHECKED.0 as isize;

    (1..=99).contains(&copies).then_some(PrintOptions {
        margins: RECT {
            left,
            top,
            right,
            bottom,
        },
        copies,
        mode,
        custom_size_mm: (custom_w, custom_h),
        auto_crop,
        auto_rotate,
    })
}

unsafe fn validate(dlg: HWND, data: &Preview<'_>) {
    let valid = options(dlg).is_some_and(|opt| {
        let r = printable_rect(data.printer.dc, opt.margins);
        r.right > r.left && r.bottom > r.top
    });
    let _ = EnableWindow(GetDlgItem(Some(dlg), IDOK.0).unwrap_or_default(), valid);
    let _ = InvalidateRect(Some(dlg), None, false);
}

/// Whether "Auto rotate" should turn the picture a quarter to match the printable area.
fn should_rotate(img: &DecodedImage, area: RECT, auto_rotate: bool) -> bool {
    if !auto_rotate || img.width == img.height {
        return false;
    }
    let area_is_landscape = area.right - area.left > area.bottom - area.top;
    (img.width > img.height) != area_is_landscape
}

/// Where to draw the picture, and the rectangle to clip it to.
struct Placement {
    image: RECT,
    clip: RECT,
}

fn intersect(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_placement(
    area: RECT,
    img_w: u32,
    img_h: u32,
    mode: usize,
    custom_size_mm: (f64, f64),
    dpi: (i32, i32),
    printer_dpi: (i32, i32),
    auto_crop: bool,
    alignment: usize,
) -> Placement {
    let target_w = (area.right - area.left) as f64;
    let target_h = (area.bottom - area.top) as f64;
    if img_w == 0 || img_h == 0 || target_w <= 0.0 || target_h <= 0.0 {
        return Placement {
            image: area,
            clip: area,
        };
    }

    // Scale the image into a `req_w` x `req_h` box, cropping or letterboxing as asked.
    let fit = |req_w: f64, req_h: f64, cover: bool| {
        let (sx, sy) = (req_w / img_w as f64, req_h / img_h as f64);
        let scale = if cover { sx.max(sy) } else { sx.min(sy) };
        (
            (img_w as f64 * scale).round() as i32,
            (img_h as f64 * scale).round() as i32,
        )
    };
    let place = |dw: i32, dh: i32| {
        let left = area.left + (target_w as i32 - dw) * (alignment % 3) as i32 / 2;
        let top = area.top + (target_h as i32 - dh) * (alignment / 3) as i32 / 2;
        RECT {
            left,
            top,
            right: left + dw,
            bottom: top + dh,
        }
    };

    if mode == 3 && auto_crop {
        // "Specified size" + "Fill and crop": the W x H box is placed on the page by the
        // alignment, the picture covers it, and the overflow is clipped to the box (not just
        // to the printable area), so the printed size stays the one that was asked for.
        let box_w = ((custom_size_mm.0 / 25.4 * dpi.0 as f64).round() as i32).max(1);
        let box_h = ((custom_size_mm.1 / 25.4 * dpi.1 as f64).round() as i32).max(1);
        let frame = place(box_w, box_h);
        let (dw, dh) = fit(box_w as f64, box_h as f64, true);
        let left = frame.left + (box_w - dw) / 2;
        let top = frame.top + (box_h - dh) / 2;
        return Placement {
            image: RECT {
                left,
                top,
                right: left + dw,
                bottom: top + dh,
            },
            clip: intersect(frame, area),
        };
    }

    let (dw, dh) = match mode {
        0 => {
            // Исходный размер: один пиксель изображения на пиксель устройства печати. В
            // предпросмотре разрешение принтера проецируется на экран.
            (
                (img_w as f64 * dpi.0 as f64 / printer_dpi.0.max(1) as f64).round() as i32,
                (img_h as f64 * dpi.1 as f64 / printer_dpi.1.max(1) as f64).round() as i32,
            )
        }
        2 => (target_w as i32, target_h as i32),
        3 => fit(
            custom_size_mm.0 / 25.4 * dpi.0 as f64,
            custom_size_mm.1 / 25.4 * dpi.1 as f64,
            false,
        ),
        _ => fit(target_w, target_h, auto_crop),
    };

    Placement {
        image: place(dw, dh),
        clip: area,
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Preview<'_>;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    match msg {
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let hdc = HDC(wp.0 as _);
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(GetSysColor(COLOR_BTNTEXT)));
            LRESULT(GetStockObject(WHITE_BRUSH).0 as isize)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DRAWITEM => {
            let dis = &*(lp.0 as *const DRAWITEMSTRUCT);
            if (ALIGN_FIRST..ALIGN_FIRST + 9).contains(&(dis.CtlID as usize)) {
                let index = dis.CtlID as usize - ALIGN_FIRST;
                let is_selected = (*ptr).alignment == index;
                let rc = dis.rcItem;
                let dc = dis.hDC;
                let px = |v| dialog::px(hwnd, v);

                // The selected button gets a soft accent fill and a two-pixel accent border,
                // so the active placement stands out at a glance. The brush must outlive the
                // `FillRect` call, so it is not extracted into a temporary handle.
                if is_selected {
                    FillRect(dc, &rc, crate::gdi::solid_brush(ACCENT_SOFT).0);
                } else {
                    FillRect(dc, &rc, HBRUSH(GetStockObject(WHITE_BRUSH).0));
                }

                let border = if is_selected { ACCENT } else { NEUTRAL };
                FrameRect(dc, &rc, crate::gdi::solid_brush(border).0);
                if is_selected {
                    let inner = RECT {
                        left: rc.left + 1,
                        top: rc.top + 1,
                        right: rc.right - 1,
                        bottom: rc.bottom - 1,
                    };
                    FrameRect(dc, &inner, crate::gdi::solid_brush(ACCENT).0);
                }

                let sheet_w = px(22);
                let sheet_h = px(20);
                let cx = (rc.left + rc.right) / 2;
                let cy = (rc.top + rc.bottom) / 2;
                let sheet_rc = RECT {
                    left: cx - sheet_w / 2,
                    top: cy - sheet_h / 2,
                    right: cx + (sheet_w + 1) / 2,
                    bottom: cy + (sheet_h + 1) / 2,
                };
                FillRect(dc, &sheet_rc, HBRUSH(GetStockObject(WHITE_BRUSH).0));
                FrameRect(
                    dc,
                    &sheet_rc,
                    crate::gdi::solid_brush(if is_selected { ACCENT } else { NEUTRAL }).0,
                );

                let col = index % 3;
                let row = index / 3;
                let m_w = px(7).max(3);
                let m_h = px(6).max(3);
                let m_x = match col {
                    0 => sheet_rc.left + px(2),
                    1 => cx - m_w / 2,
                    _ => sheet_rc.right - px(2) - m_w,
                };
                let m_y = match row {
                    0 => sheet_rc.top + px(2),
                    1 => cy - m_h / 2,
                    _ => sheet_rc.bottom - px(2) - m_h,
                };
                let marker_rc = RECT {
                    left: m_x,
                    top: m_y,
                    right: m_x + m_w,
                    bottom: m_y + m_h,
                };
                let marker = if is_selected { ACCENT } else { 0x00808080 };
                FillRect(dc, &marker_rc, crate::gdi::solid_brush(marker).0);

                if dis.itemState.0 & ODS_FOCUS.0 != 0 {
                    let focus = RECT {
                        left: rc.left + px(3),
                        top: rc.top + px(3),
                        right: rc.right - px(3),
                        bottom: rc.bottom - px(3),
                    };
                    let _ = DrawFocusRect(dc, &focus);
                }
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_NOTIFY if lp.0 != 0 => {
            let notice = &*(lp.0 as *const NMUPDOWN);
            let id = notice.hdr.idFrom;
            if notice.hdr.code == UDN_DELTAPOS
                && ((SPIN_FIRST..SPIN_FIRST + 5).contains(&id)
                    || id == CUSTOM_W_SPIN
                    || id == CUSTOM_H_SPIN)
            {
                validate(hwnd, &*ptr);
                LRESULT(0)
            } else {
                LRESULT(0)
            }
        }
        WM_COMMAND => {
            let id = dialog::loword(wp);
            let code = (wp.0 >> 16) as u32;
            match id {
                id if id == IDCANCEL.0 as usize => dialog::close(hwnd),
                id if id == IDOK.0 as usize => {
                    if let Some(opt) = options(hwnd) {
                        let area = printable_rect((*ptr).printer.dc, opt.margins);
                        if area.right > area.left && area.bottom > area.top {
                            let configured = (*ptr).printer.configure(None);
                            let dc = (*ptr).printer.dc;
                            let rotate = should_rotate((*ptr).image, area, opt.auto_rotate);
                            let alignment = (*ptr).alignment;
                            let name = (*ptr).name;
                            let image = (*ptr).image;
                            let image = if rotate { (*ptr).rotated() } else { image };
                            let printed = configured
                                && print_page(
                                    dc,
                                    image,
                                    name,
                                    opt.margins,
                                    opt.copies,
                                    opt.mode,
                                    opt.custom_size_mm,
                                    opt.auto_crop,
                                    alignment,
                                );
                            (*ptr).printed = printed;
                            if printed {
                                dialog::close(hwnd);
                            } else {
                                crate::file_actions::show_error(
                                    hwnd,
                                    tr("Could not print the image."),
                                );
                            }
                        }
                    }
                }
                PRINTER_COMBO if code == CBN_SELCHANGE => {
                    let name = dialog::item_text(hwnd, PRINTER_COMBO as i32);
                    if let Some(new_p) = PrinterState::create(&name) {
                        (*ptr).printer = new_p;
                        sync_orientation(hwnd, (*ptr).printer.dc);
                        refresh_papers(hwnd, &mut *ptr);
                        validate(hwnd, &*ptr);
                    }
                }
                PRINTER_PROPS => {
                    if (*ptr).printer.open_properties(hwnd) {
                        sync_orientation(hwnd, (*ptr).printer.dc);
                        refresh_papers(hwnd, &mut *ptr);
                        validate(hwnd, &*ptr);
                    }
                }
                id if id == ORIENTATION || id == ORIENTATION + 1 => {
                    if code == BN_CLICKED {
                        let landscape = id == ORIENTATION + 1;
                        if !(*ptr).printer.configure(Some(landscape)) {
                            crate::file_actions::show_error(
                                hwnd,
                                tr("Could not change the paper orientation."),
                            );
                        }
                        sync_orientation(hwnd, (*ptr).printer.dc);
                        validate(hwnd, &*ptr);
                    }
                }
                id if (ALIGN_FIRST..ALIGN_FIRST + 9).contains(&id) => {
                    let old_align = (*ptr).alignment;
                    (*ptr).alignment = id - ALIGN_FIRST;
                    if let Ok(old_btn) = GetDlgItem(Some(hwnd), (ALIGN_FIRST + old_align) as i32) {
                        let _ = InvalidateRect(Some(old_btn), None, true);
                    }
                    if let Ok(new_btn) = GetDlgItem(Some(hwnd), id as i32) {
                        let _ = InvalidateRect(Some(new_btn), None, true);
                    }
                    validate(hwnd, &*ptr);
                }
                id if (MODE_FIRST..MODE_FIRST + 4).contains(&id) => {
                    update_mode_controls_visibility(hwnd);
                    validate(hwnd, &*ptr);
                }
                id if id == AUTO_CROP || id == AUTO_ROTATE => {
                    validate(hwnd, &*ptr);
                }
                PRINTER_REFRESH => {
                    refresh_papers(hwnd, &mut *ptr);
                    validate(hwnd, &*ptr);
                }
                id if id == PAPER_INFO as usize && code == CBN_SELCHANGE => {
                    let sel =
                        SendDlgItemMessageW(hwnd, PAPER_INFO, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                    if sel >= 0 {
                        // An explicit reference: rustc's `dangerous_implicit_autorefs` lint rejects
                        // the automatic borrow of a field behind this raw pointer.
                        let papers = &(*ptr).papers;
                        if let Some(paper) = papers.get(sel as usize).cloned() {
                            if !(*ptr).printer.set_paper(paper.id) {
                                refresh_papers(hwnd, &mut *ptr);
                            }
                            sync_orientation(hwnd, (*ptr).printer.dc);
                            validate(hwnd, &*ptr);
                        }
                    }
                }
                _ if code == EN_CHANGE => validate(hwnd, &*ptr),
                _ => {}
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            paint(hwnd, dc, &mut *ptr);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_CLOSE => {
            dialog::close(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn project_rect(area: RECT, paper: RECT, physical: (i32, i32), offset: (i32, i32)) -> RECT {
    let map =
        |v, off, orig, span, phys| orig + ((v + off) as i64 * span as i64 / phys as i64) as i32;
    let span_x = paper.right - paper.left;
    let span_y = paper.bottom - paper.top;
    RECT {
        left: map(area.left, offset.0, paper.left, span_x, physical.0),
        top: map(area.top, offset.1, paper.top, span_y, physical.1),
        right: map(area.right, offset.0, paper.left, span_x, physical.0),
        bottom: map(area.bottom, offset.1, paper.top, span_y, physical.1),
    }
}

unsafe fn paint(hwnd: HWND, dc: HDC, data: &mut Preview<'_>) {
    let px = |v| dialog::px(hwnd, v);
    let mut bounds = RECT::default();
    let _ = GetClientRect(hwnd, &mut bounds);

    let sidebar_w = px(SIDEBAR_WIDTH);
    let separator_x = bounds.right - sidebar_w;

    let preview_area = RECT {
        left: 0,
        top: 0,
        right: separator_x,
        bottom: bounds.bottom,
    };
    FillRect(dc, &preview_area, crate::gdi::solid_brush(0x00D5D5D5).0);

    let sidebar = RECT {
        left: separator_x,
        top: 0,
        right: bounds.right,
        bottom: bounds.bottom,
    };
    FillRect(dc, &sidebar, HBRUSH(GetStockObject(WHITE_BRUSH).0));

    let separator = RECT {
        left: separator_x,
        top: 0,
        right: separator_x + px(1),
        bottom: bounds.bottom,
    };
    FillRect(dc, &separator, crate::gdi::solid_brush(0x00D0D0D0).0);

    let printer = data.printer.dc;
    let caps = |index| GetDeviceCaps(Some(printer), index);
    let (pw, ph) = (caps(PHYSICALWIDTH), caps(PHYSICALHEIGHT));
    let (dx, dy) = (caps(LOGPIXELSX), caps(LOGPIXELSY));
    if pw <= 0 || ph <= 0 || dx <= 0 || dy <= 0 {
        return;
    }
    let (w, h) = (pw as f64 / dx as f64, ph as f64 / dy as f64);
    let pad = px(25);
    let avail_w = (preview_area.right - preview_area.left - pad * 2).max(1);
    let avail_h = (preview_area.bottom - preview_area.top - pad * 2 - px(25)).max(1);
    let scale = (avail_w as f64 / w).min(avail_h as f64 / h);
    if scale <= 0.0 {
        return;
    }

    let (w, h) = ((w * scale).round() as i32, (h * scale).round() as i32);
    let left = (preview_area.left + preview_area.right - w) / 2;
    let top = (preview_area.top + preview_area.bottom - h - px(22)) / 2;

    let shadow_offset = px(4);
    let shadow = RECT {
        left: left + px(2),
        top: top + shadow_offset,
        right: left + w + shadow_offset,
        bottom: top + h + shadow_offset,
    };
    FillRect(dc, &shadow, crate::gdi::solid_brush(0x00B9B6B3).0);

    let paper = RECT {
        left,
        top,
        right: left + w,
        bottom: top + h,
    };
    FillRect(dc, &paper, HBRUSH(GetStockObject(WHITE_BRUSH).0));
    FrameRect(dc, &paper, crate::gdi::solid_brush(0x00C8C6C3).0);

    if let Some(opt) = options(hwnd) {
        let device_area = printable_rect(printer, opt.margins);
        let rotate = should_rotate(data.image, device_area, opt.auto_rotate);
        let area = project_rect(
            device_area,
            paper,
            (pw, ph),
            (caps(PHYSICALOFFSETX), caps(PHYSICALOFFSETY)),
        );

        // Разрешение превью: сколько экранных пикселей приходится на дюйм бумаги.
        let preview_dpi = (
            (((paper.right - paper.left) as f64 * dx as f64 / pw as f64).round() as i32).max(1),
            (((paper.bottom - paper.top) as f64 * dy as f64 / ph as f64).round() as i32).max(1),
        );

        let alignment = data.alignment;
        let image = data.image;
        let image = if rotate { data.rotated() } else { image };
        let placement = compute_placement(
            area,
            image.width,
            image.height,
            opt.mode,
            opt.custom_size_mm,
            preview_dpi,
            (dx, dy),
            opt.auto_crop,
            alignment,
        );
        let saved = SaveDC(dc);
        let clip = placement.clip;
        if clip.right > clip.left && clip.bottom > clip.top {
            let _ = IntersectClipRect(dc, clip.left, clip.top, clip.right, clip.bottom);
            if opt.mode == 2 {
                draw_stretched(dc, image, placement.image);
            } else {
                draw_fitted(dc, image, placement.image, false);
            }
        }
        if saved != 0 {
            let _ = RestoreDC(dc, saved);
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn print_page(
    dc: HDC,
    img: &DecodedImage,
    doc_name: &str,
    margins: RECT,
    copies: u32,
    mode: usize,
    custom_size_mm: (f64, f64),
    auto_crop: bool,
    alignment: usize,
) -> bool {
    let name = HSTRING::from(doc_name);
    let doc = DOCINFOW {
        cbSize: size_of::<DOCINFOW>() as i32,
        lpszDocName: PCWSTR(name.as_ptr()),
        ..Default::default()
    };
    if StartDocW(dc, &doc) <= 0 {
        return false;
    }
    let area = printable_rect(dc, margins);
    for _ in 0..copies {
        if StartPage(dc) <= 0 {
            AbortDoc(dc);
            return false;
        }
        let saved = SaveDC(dc);
        if saved == 0 {
            AbortDoc(dc);
            return false;
        }
        let dpi = (
            GetDeviceCaps(Some(dc), LOGPIXELSX).max(1),
            GetDeviceCaps(Some(dc), LOGPIXELSY).max(1),
        );

        let placement = compute_placement(
            area,
            img.width,
            img.height,
            mode,
            custom_size_mm,
            dpi,
            dpi,
            auto_crop,
            alignment,
        );
        let clip = placement.clip;
        if clip.right > clip.left && clip.bottom > clip.top {
            let _ = IntersectClipRect(dc, clip.left, clip.top, clip.right, clip.bottom);
            if mode == 2 {
                draw_stretched(dc, img, placement.image);
            } else {
                draw_fitted(dc, img, placement.image, false);
            }
        }

        let _ = RestoreDC(dc, saved);
        if EndPage(dc) <= 0 {
            AbortDoc(dc);
            return false;
        }
    }
    EndDoc(dc) > 0
}

unsafe fn printable_rect(dc: HDC, margins: RECT) -> RECT {
    let caps = |index| GetDeviceCaps(Some(dc), index);
    let (dpi_x, dpi_y) = (caps(LOGPIXELSX), caps(LOGPIXELSY));
    let (off_x, off_y) = (caps(PHYSICALOFFSETX), caps(PHYSICALOFFSETY));
    let (paper_w, paper_h) = (caps(PHYSICALWIDTH), caps(PHYSICALHEIGHT));
    let (page_w, page_h) = (caps(HORZRES).max(0), caps(VERTRES).max(0));
    let px = |mm100: i32, dpi: i32| (mm100.max(0) as i64 * dpi as i64 / MM100_PER_INCH) as i32;
    RECT {
        left: (px(margins.left, dpi_x) - off_x).clamp(0, page_w),
        top: (px(margins.top, dpi_y) - off_y).clamp(0, page_h),
        right: (paper_w - px(margins.right, dpi_x) - off_x).clamp(0, page_w),
        bottom: (paper_h - px(margins.bottom, dpi_y) - off_y).clamp(0, page_h),
    }
}
