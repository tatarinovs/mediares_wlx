//! Print preview and cancellable background GDI printing.

mod controls;
mod job;
mod layout;
mod printer;

use std::sync::Arc;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dialog;
use crate::i18n::{decimal, tr, tr_fmt};
use crate::image_cache::DecodedImage;
use job::{Outcome, Task};
use layout::{intersect, parse_mm, Options, Placement, SizeMode};
use printer::{PaperChoice, Printer};

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
const STATUS: usize = 190;
const PRINT_TIMER: usize = 1;

/// The Lister SDK passes MM_LOMETRIC (1/10 mm); internally margins use 1/100 mm.
pub(crate) fn tc_margins(m: RECT) -> RECT {
    RECT {
        left: m.left.saturating_mul(10),
        top: m.top.saturating_mul(10),
        right: m.right.saturating_mul(10),
        bottom: m.bottom.saturating_mul(10),
    }
}

struct Preview {
    printer: Printer,
    available: bool,
    image: Arc<DecodedImage>,
    preview: Arc<DecodedImage>,
    name: String,
    printed: bool,
    initializing: bool,
    papers: Vec<PaperChoice>,
    alignment: usize,
    rotated: Option<Arc<DecodedImage>>,
    job: Option<Task>,
    caption_font: HFONT,
}
impl Preview {
    fn rotated(&mut self) -> &DecodedImage {
        self.rotated
            .get_or_insert_with(|| Arc::new(crate::image_cache::rotated(&self.preview, true)))
    }
}

/// Keep repainting and preview rotation cheap even when printing a very large original.
fn preview_copy(image: &Arc<DecodedImage>) -> Arc<DecodedImage> {
    use mediares_core::image::{imageops, ImageBuffer, Rgba};
    const MAX_SIDE: u32 = 1600;
    let longest = image.width.max(image.height);
    if longest <= MAX_SIDE {
        return image.clone();
    }
    let Some(source) =
        ImageBuffer::<Rgba<u8>, _>::from_raw(image.width, image.height, image.bgra.as_slice())
    else {
        return image.clone();
    };
    let width = (image.width as u64 * MAX_SIDE as u64 / longest as u64).max(1) as u32;
    let height = (image.height as u64 * MAX_SIDE as u64 / longest as u64).max(1) as u32;
    let pixels = imageops::resize(&source, width, height, imageops::FilterType::Triangle);
    Arc::new(DecodedImage {
        width,
        height,
        full: image.full,
        bgra: pixels.into_raw(),
        is_preview: image.is_preview,
        exif: None,
        pages: image.pages,
    })
}

fn format_mm(value: f64) -> String {
    decimal(
        format!("{value:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string(),
    )
}

// Device pixel rounding is useful for layout, but adds misleading hundredths to paper sizes.
fn format_size(value: f64) -> String {
    format_mm((value * 10.0).round() / 10.0)
}

unsafe fn text(dlg: HWND, id: usize, value: &str) {
    let _ = SetWindowTextW(
        GetDlgItem(Some(dlg), id as i32).unwrap_or_default(),
        &HSTRING::from(value),
    );
}

unsafe fn selected_mode(dlg: HWND) -> SizeMode {
    SizeMode::ALL
        .into_iter()
        .enumerate()
        .find_map(|(i, mode)| {
            (SendDlgItemMessageW(
                dlg,
                (MODE_FIRST + i) as i32,
                BM_GETCHECK,
                WPARAM(0),
                LPARAM(0),
            )
            .0 == BST_CHECKED.0 as isize)
                .then_some(mode)
        })
        .unwrap_or(SizeMode::Fit)
}

unsafe fn update_mode_controls_visibility(dlg: HWND) {
    let mode = selected_mode(dlg);
    for id in [
        CUSTOM_W_LABEL,
        CUSTOM_WIDTH,
        CUSTOM_W_SPIN,
        CUSTOM_W_UNIT,
        CUSTOM_H_LABEL,
        CUSTOM_HEIGHT,
        CUSTOM_H_SPIN,
        CUSTOM_H_UNIT,
    ] {
        if let Ok(h) = GetDlgItem(Some(dlg), id as i32) {
            let _ = ShowWindow(
                h,
                if mode == SizeMode::Custom {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
        }
    }
    let _ = EnableWindow(
        GetDlgItem(Some(dlg), AUTO_CROP as i32).unwrap_or_default(),
        matches!(mode, SizeMode::Fit | SizeMode::Custom),
    );
}

unsafe fn refresh_papers(dlg: HWND, data: &mut Preview) {
    data.papers = data.printer.papers();
    let combo = GetDlgItem(Some(dlg), PAPER_INFO).unwrap_or_default();
    SendMessageW(combo, CB_RESETCONTENT, None, None);
    let current = data.printer.paper_id();
    let mut selected = None;
    for (i, paper) in data.papers.iter().enumerate() {
        let label = HSTRING::from(paper.label.as_str());
        SendMessageW(
            combo,
            CB_ADDSTRING,
            None,
            Some(LPARAM(label.as_ptr() as isize)),
        );
        if Some(paper.id) == current {
            selected = Some(i);
        }
    }
    // Custom driver forms may not occur in DC_PAPERS: display their real geometry.
    if selected.is_none() {
        let (w, h) = data
            .printer
            .geometry()
            .map(|g| g.paper_mm())
            .unwrap_or((0.0, 0.0));
        let label = HSTRING::from(format!("{} × {} {}", format_mm(w), format_mm(h), tr("mm")));
        SendMessageW(
            combo,
            CB_ADDSTRING,
            None,
            Some(LPARAM(label.as_ptr() as isize)),
        );
        selected = Some(data.papers.len());
    }
    SendMessageW(
        combo,
        CB_SETCURSEL,
        Some(WPARAM(selected.unwrap_or(0))),
        None,
    );
    let _ = EnableWindow(combo, data.available && !data.papers.is_empty());
}

unsafe fn fill_printers(dlg: HWND, data: &Preview, mut names: Vec<String>) {
    if data.available && !names.contains(&data.printer.name) {
        names.push(data.printer.name.clone());
    }
    let combo = GetDlgItem(Some(dlg), PRINTER_COMBO as i32).unwrap_or_default();
    SendMessageW(combo, CB_RESETCONTENT, None, None);
    for (i, name) in names.iter().enumerate() {
        let label = HSTRING::from(name.as_str());
        SendMessageW(
            combo,
            CB_ADDSTRING,
            None,
            Some(LPARAM(label.as_ptr() as isize)),
        );
        if *name == data.printer.name {
            SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(i)), None);
        }
    }
    let _ = EnableWindow(combo, !names.is_empty());
}

unsafe fn refresh_printers(dlg: HWND, data: &mut Preview) {
    let names = printer::enumerate();
    let next = Printer::create(&data.printer.name)
        .or_else(|| names.iter().find_map(|name| Printer::create(name)));
    data.available = next.is_some();
    if let Some(mut next) = next {
        if let Some(g) = data.printer.geometry() {
            next.orient(g.paper_mm().0 > g.paper_mm().1);
        }
        data.printer = next;
    }
    fill_printers(dlg, data, names);
    sync_orientation(dlg, data.printer.dc);
    refresh_papers(dlg, data);
    validate(dlg, data);
}

pub unsafe fn print(
    owner: HWND,
    img: Arc<DecodedImage>,
    doc_name: &str,
    preferred_printer: Option<&str>,
    margins: Option<RECT>,
) -> bool {
    let printers = printer::enumerate();
    let default = printer::default_name();
    let Some(mut printer) = preferred_printer
        .and_then(|name| Printer::create(name))
        .or_else(|| default.as_deref().and_then(|name| Printer::create(name)))
        .or_else(|| printers.iter().find_map(|name| Printer::create(name)))
    else {
        crate::file_actions::show_error(owner, tr("No default printer is available."));
        return false;
    };
    printer.orient(img.width > img.height);

    dialog::register_class(CLASS, Some(wnd_proc));
    let Some(dlg) = dialog::create_frame(
        owner,
        CLASS,
        &format!("{} — {}", tr("Print preview"), doc_name),
        1120,
        640,
    ) else {
        return false;
    };

    let style = GetWindowLongPtrW(dlg, GWL_STYLE) as u32;
    SetWindowLongPtrW(dlg, GWL_STYLE, (style | WS_CLIPCHILDREN.0) as isize);

    let font = dialog::font(dlg, "Segoe UI", -14);
    let mut data = Preview {
        printer,
        available: true,
        initializing: true,
        job: None,
        caption_font: HFONT::default(),
        preview: preview_copy(&img),
        image: img,
        name: doc_name.to_string(),
        printed: false,
        papers: Vec::new(),
        alignment: 4,
        rotated: None,
    };
    SetWindowLongPtrW(dlg, GWLP_USERDATA, &mut data as *mut _ as isize);

    let (print_button, _small_font) = controls::build(dlg, &mut data, margins, &font, printers);

    data.initializing = false;
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

unsafe fn options(dlg: HWND, alignment: usize) -> Result<Options, &'static str> {
    let margin = |id| {
        parse_mm(&dialog::item_text(dlg, id), 0.0, 300.0)
            .map(|mm| (mm * 100.0).round() as i32)
            .ok_or(tr("Enter margins from 0 to 300 mm."))
    };
    let margins = RECT {
        top: margin(MARGIN_FIRST as i32)?,
        left: margin((MARGIN_FIRST + 1) as i32)?,
        right: margin((MARGIN_FIRST + 2) as i32)?,
        bottom: margin((MARGIN_FIRST + 3) as i32)?,
    };
    let copies = dialog::item_text(dlg, COPIES as i32)
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|n| (1..=99).contains(n))
        .ok_or(tr("Enter 1 to 99 copies."))?;
    let mode = selected_mode(dlg);
    let size_mm = if mode == SizeMode::Custom {
        let dimension = |id| {
            parse_mm(&dialog::item_text(dlg, id), 1.0, 1000.0)
                .ok_or(tr("Enter width and height from 1 to 1000 mm."))
        };
        (
            dimension(CUSTOM_WIDTH as i32)?,
            dimension(CUSTOM_HEIGHT as i32)?,
        )
    } else {
        (100.0, 150.0)
    };
    let checked = |id| {
        SendDlgItemMessageW(dlg, id, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == BST_CHECKED.0 as isize
    };
    Ok(Options {
        margins,
        copies,
        mode,
        size_mm,
        crop: checked(AUTO_CROP as i32),
        auto_rotate: checked(AUTO_ROTATE as i32),
        alignment,
    })
}

unsafe fn preview_rect(dlg: HWND) -> RECT {
    let mut rc = RECT::default();
    let _ = GetClientRect(dlg, &mut rc);
    rc.right -= dialog::px(dlg, SIDEBAR_WIDTH);
    rc.bottom -= dialog::px(dlg, 45);
    rc
}

unsafe fn validate(dlg: HWND, data: &Preview) {
    validate_options(dlg, data, true);
}

unsafe fn validate_options(dlg: HWND, data: &Preview, repaint: bool) {
    if data.initializing || data.job.is_some() {
        return;
    }
    let result = if !data.available {
        Err(tr("No default printer is available."))
    } else {
        options(dlg, data.alignment).and_then(|opt| {
            data.printer
                .geometry()
                .and_then(|g| {
                    g.layout((data.image.width, data.image.height), opt)
                        .map(|p| (g, p))
                })
                .ok_or(tr("The image does not fit within these margins."))
        })
    };
    let _ = EnableWindow(
        GetDlgItem(Some(dlg), IDOK.0).unwrap_or_default(),
        result.is_ok(),
    );
    let status = match result {
        Ok((g, p)) => {
            let r = intersect(p.image, p.clip);
            tr_fmt(
                "Image: {width} × {height} mm",
                &[
                    (
                        "width",
                        &format_size((r.right - r.left) as f64 * 25.4 / g.dpi.0 as f64),
                    ),
                    (
                        "height",
                        &format_size((r.bottom - r.top) as f64 * 25.4 / g.dpi.1 as f64),
                    ),
                ],
            )
        }
        Err(error) => error.to_string(),
    };
    text(dlg, STATUS, &status);
    if repaint {
        let mut canvas = preview_rect(dlg);
        canvas.bottom += dialog::px(dlg, 45);
        let _ = InvalidateRect(Some(dlg), Some(&canvas), false);
    }
}

unsafe fn set_busy(dlg: HWND, busy: bool) {
    let mut child = GetWindow(dlg, GW_CHILD).unwrap_or_default();
    while !child.is_invalid() {
        let id = GetDlgCtrlID(child) as usize;
        if id != IDCANCEL.0 as usize && id != STATUS {
            let _ = EnableWindow(child, !busy);
        }
        child = GetWindow(child, GW_HWNDNEXT).unwrap_or_default();
    }
    text(
        dlg,
        IDCANCEL.0 as usize,
        if busy {
            tr("Cancel printing")
        } else {
            tr("Cancel")
        },
    );
}

unsafe fn start_print(dlg: HWND, data: &mut Preview) {
    if data.job.is_some() || !data.available {
        return;
    }
    let Ok(opt) = options(dlg, data.alignment) else {
        validate(dlg, data);
        return;
    };
    if data
        .printer
        .geometry()
        .and_then(|g| g.layout((data.image.width, data.image.height), opt))
        .is_none()
    {
        return;
    }
    let request = job::Request {
        settings: data.printer.settings(),
        image: data.image.clone(),
        name: data.name.clone(),
        options: opt,
    };
    match Task::start(request) {
        Ok(task) => {
            data.job = Some(task);
            set_busy(dlg, true);
            text(dlg, STATUS, tr("Sending to printer…"));
            if SetTimer(Some(dlg), PRINT_TIMER, 100, None) == 0 {
                data.job = None;
                set_busy(dlg, false);
                validate(dlg, data);
                crate::file_actions::show_error(dlg, tr("Could not print the image."));
            }
        }
        Err(_) => crate::file_actions::show_error(dlg, tr("Could not print the image.")),
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Preview;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    match msg {
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let dc = HDC(wp.0 as _);
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(GetSysColor(COLOR_BTNTEXT)));
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
            let header = &*(lp.0 as *const NMHDR);
            if header.code == UDN_DELTAPOS {
                let notice = &*(lp.0 as *const NMUPDOWN);
                let field = match header.idFrom {
                    id if (SPIN_FIRST..SPIN_FIRST + 4).contains(&id) => {
                        Some((MARGIN_FIRST + id - SPIN_FIRST, 0.0, 300.0))
                    }
                    id if id == SPIN_FIRST + 4 => Some((COPIES, 1.0, 99.0)),
                    CUSTOM_W_SPIN => Some((CUSTOM_WIDTH, 1.0, 1000.0)),
                    CUSTOM_H_SPIN => Some((CUSTOM_HEIGHT, 1.0, 1000.0)),
                    _ => None,
                };
                if let Some((id, min, max)) = field {
                    if (*ptr).job.is_none() {
                        let current =
                            parse_mm(&dialog::item_text(hwnd, id as i32), min, max).unwrap_or(min);
                        text(
                            hwnd,
                            id,
                            &format_mm((current + notice.iDelta as f64).clamp(min, max)),
                        );
                    }
                    return LRESULT(1); // Keep decimal fractions; prevent the integer buddy rewrite.
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = dialog::loword(wp);
            let code = (wp.0 >> 16) as u32;
            if id == IDCANCEL.0 as usize {
                dialog::close(hwnd);
                return LRESULT(0);
            }
            if (*ptr).initializing || (*ptr).job.is_some() {
                return LRESULT(0);
            }
            match id {
                id if id == IDOK.0 as usize => start_print(hwnd, &mut *ptr),
                PRINTER_COMBO if code == CBN_SELCHANGE => {
                    let name = dialog::item_text(hwnd, PRINTER_COMBO as i32);
                    if let Some(mut next) = Printer::create(&name) {
                        if let Some(g) = (*ptr).printer.geometry() {
                            next.orient(g.paper_mm().0 > g.paper_mm().1);
                        }
                        (*ptr).printer = next;
                        (*ptr).available = true;
                        sync_orientation(hwnd, (*ptr).printer.dc);
                        refresh_papers(hwnd, &mut *ptr);
                        validate(hwnd, &*ptr);
                    } else {
                        fill_printers(hwnd, &*ptr, printer::enumerate());
                        crate::file_actions::show_error(
                            hwnd,
                            tr("Could not open the selected printer."),
                        );
                    }
                }
                PRINTER_PROPS => {
                    // The driver runs a nested modal loop. Keep the active printer untouched
                    // while that loop can paint the preview or close it on host shutdown.
                    let candidate = Printer::from_settings((*ptr).printer.settings());
                    if let Some(mut candidate) = candidate {
                        let accepted = candidate.properties(hwnd);
                        if !IsWindow(Some(hwnd)).as_bool() {
                            return LRESULT(0);
                        }
                        match accepted {
                            Ok(true) => {
                                (*ptr).printer = candidate;
                                sync_orientation(hwnd, (*ptr).printer.dc);
                                refresh_papers(hwnd, &mut *ptr);
                                validate(hwnd, &*ptr);
                            }
                            Ok(false) => {}
                            Err(()) => crate::file_actions::show_error(
                                hwnd,
                                tr("Could not apply the printer settings."),
                            ),
                        }
                    } else {
                        crate::file_actions::show_error(
                            hwnd,
                            tr("Could not apply the printer settings."),
                        );
                    }
                }
                id if (ORIENTATION..ORIENTATION + 2).contains(&id) && code == BN_CLICKED => {
                    if !(*ptr).printer.orient(id == ORIENTATION + 1) {
                        crate::file_actions::show_error(
                            hwnd,
                            tr("Could not change the paper orientation."),
                        );
                    }
                    sync_orientation(hwnd, (*ptr).printer.dc);
                    refresh_papers(hwnd, &mut *ptr);
                    validate(hwnd, &*ptr);
                }
                id if (ALIGN_FIRST..ALIGN_FIRST + 9).contains(&id) && code == BN_CLICKED => {
                    let old = (*ptr).alignment;
                    (*ptr).alignment = id - ALIGN_FIRST;
                    for id in [ALIGN_FIRST + old, id] {
                        let _ = InvalidateRect(GetDlgItem(Some(hwnd), id as i32).ok(), None, true);
                    }
                    validate(hwnd, &*ptr);
                }
                id if (MODE_FIRST..MODE_FIRST + 4).contains(&id) => {
                    update_mode_controls_visibility(hwnd);
                    validate(hwnd, &*ptr);
                }
                AUTO_CROP | AUTO_ROTATE => validate(hwnd, &*ptr),
                PRINTER_REFRESH => refresh_printers(hwnd, &mut *ptr),
                id if id == PAPER_INFO as usize && code == CBN_SELCHANGE => {
                    let sel =
                        SendDlgItemMessageW(hwnd, PAPER_INFO, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                    if sel >= 0 {
                        let papers = &(*ptr).papers;
                        if let Some(paper) = papers.get(sel as usize).cloned() {
                            if !(*ptr).printer.set_paper(paper.id) {
                                crate::file_actions::show_error(
                                    hwnd,
                                    tr("Could not change the paper size."),
                                );
                            }
                            sync_orientation(hwnd, (*ptr).printer.dc);
                            refresh_papers(hwnd, &mut *ptr);
                            validate(hwnd, &*ptr);
                        }
                    }
                }
                COPIES if code == EN_CHANGE => {
                    validate_options(hwnd, &*ptr, false);
                }
                _ if code == EN_CHANGE => validate(hwnd, &*ptr),
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == PRINT_TIMER => {
            let outcome = (*ptr).job.as_ref().and_then(Task::poll);
            if let Some(outcome) = outcome {
                let _ = KillTimer(Some(hwnd), PRINT_TIMER);
                (*ptr).job = None;
                match outcome {
                    Outcome::Printed => {
                        (*ptr).printed = true;
                        dialog::close(hwnd);
                    }
                    _ => {
                        set_busy(hwnd, false);
                        update_mode_controls_visibility(hwnd);
                        refresh_papers(hwnd, &mut *ptr);
                        validate(hwnd, &*ptr);
                        if outcome == Outcome::Failed {
                            crate::file_actions::show_error(hwnd, tr("Could not print the image."));
                        }
                    }
                }
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
        WM_NCDESTROY => {
            // The modal loop keeps Preview alive through destruction; workers never reference it.
            (*ptr).job = None;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn paint(hwnd: HWND, dc: HDC, data: &mut Preview) {
    let px = |v| dialog::px(hwnd, v);
    let mut bounds = RECT::default();
    let _ = GetClientRect(hwnd, &mut bounds);
    let separator = bounds.right - px(SIDEBAR_WIDTH);
    FillRect(dc, &bounds, HBRUSH(GetStockObject(WHITE_BRUSH).0));
    let canvas = RECT {
        left: 0,
        top: 0,
        right: separator,
        bottom: bounds.bottom,
    };
    FillRect(dc, &canvas, crate::gdi::solid_brush(0x00F0EEEC).0);
    FrameRect(
        dc,
        &RECT {
            left: separator,
            top: 0,
            right: separator + 1,
            bottom: bounds.bottom,
        },
        crate::gdi::solid_brush(NEUTRAL).0,
    );
    let Some(g) = data.printer.geometry() else {
        return;
    };
    let available = preview_rect(hwnd);
    let (mm_w, mm_h) = g.paper_mm();
    crate::gdi::text(
        dc,
        RECT {
            left: px(20),
            top: available.bottom,
            right: separator - px(20),
            bottom: bounds.bottom,
        },
        &format!(
            "{} × {} {}  ·  {} × {} dpi",
            format_size(mm_w),
            format_size(mm_h),
            tr("mm"),
            g.dpi.0,
            g.dpi.1
        ),
        Some(data.caption_font),
        0x006A6460,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    let scale =
        ((available.right - px(52)) as f64 / mm_w).min((available.bottom - px(48)) as f64 / mm_h);
    if scale <= 0.0 {
        return;
    }
    let (w, h) = ((mm_w * scale).round() as i32, (mm_h * scale).round() as i32);
    let left = (available.right - w) / 2;
    let top = (available.bottom - h) / 2;
    let paper = RECT {
        left,
        top,
        right: left + w,
        bottom: top + h,
    };
    let shadow = RECT {
        left: left + px(3),
        top: top + px(4),
        right: paper.right + px(4),
        bottom: paper.bottom + px(4),
    };
    FillRect(dc, &shadow, crate::gdi::solid_brush(0x00D8D5D2).0);
    FillRect(dc, &paper, HBRUSH(GetStockObject(WHITE_BRUSH).0));
    FrameRect(dc, &paper, crate::gdi::solid_brush(0x00C8C6C3).0);
    let Ok(opt) = options(hwnd, data.alignment) else {
        return;
    };
    let Some(p) = g.layout((data.image.width, data.image.height), opt) else {
        return;
    };
    let placement = Placement {
        image: g.project(p.image, paper),
        clip: g.project(p.clip, paper),
        rotate: p.rotate,
    };
    let image = data.preview.clone();
    let image = if p.rotate { data.rotated() } else { &image };
    job::draw_image(dc, image, placement);
    // Subtle margin guides make clipping visible without being sent to the printer.
    if let Some(area) = g.area(opt.margins) {
        let r = g.project(area, paper);
        let saved = SaveDC(dc);
        if saved != 0 {
            let pen = crate::gdi::GdiObject(CreatePen(PS_DOT, 1, COLORREF(0x00D6D3D0)));
            SelectObject(dc, pen.0.into());
            SelectObject(dc, GetStockObject(NULL_BRUSH));
            let _ = Rectangle(dc, r.left, r.top, r.right, r.bottom);
            let _ = RestoreDC(dc, saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_cache_keeps_the_original_and_bounds_its_own_memory() {
        let image = Arc::new(DecodedImage {
            width: 3200,
            height: 2,
            full: (3200, 2),
            bgra: [0, 128, 255, 255].repeat(6400),
            is_preview: false,
            exif: None,
            pages: 1,
        });
        let preview = preview_copy(&image);
        assert_eq!(
            (preview.width, preview.height, preview.full),
            (1600, 1, (3200, 2))
        );
        assert_eq!(preview.bgra.len(), 6400);
        assert_eq!((image.width, image.height), (3200, 2));
        assert!(Arc::ptr_eq(&preview_copy(&preview), &preview));
    }
    #[test]
    fn lister_margins_keep_tenths_of_a_millimeter() {
        let r = tc_margins(RECT {
            left: 100,
            top: 125,
            right: 200,
            bottom: 0,
        });
        assert_eq!((r.left, r.top, r.right, r.bottom), (1000, 1250, 2000, 0));
        assert_eq!(
            tc_margins(RECT {
                left: i32::MAX,
                ..Default::default()
            })
            .left,
            i32::MAX
        );
    }
}
