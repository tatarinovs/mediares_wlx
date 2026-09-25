//! Printing the picture on screen: one page, fitted into the margins, centered.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    DeleteDC, GetDeviceCaps, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY, PHYSICALHEIGHT, PHYSICALOFFSETX, PHYSICALOFFSETY,
    PHYSICALWIDTH, VERTRES,
};
use windows::Win32::Storage::Xps::{EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
use windows::Win32::UI::Controls::Dialogs::{
    PrintDlgW, PD_NOPAGENUMS, PD_NOSELECTION, PD_RETURNDC, PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGW,
};

use crate::image_cache::DecodedImage;
use crate::image_view::draw_fitted;

/// Used when TC passes no margins: 10 mm, in TC's unit (1/100 mm).
const DEFAULT_MARGIN: i32 = 1000;
const MM100_PER_INCH: i64 = 2540;

/// Asks for the printer and prints `img`. `margins` in 1/100 mm (as TC's `ListPrint` passes them).
pub unsafe fn print(owner: HWND, img: &DecodedImage, doc_name: &str, margins: Option<RECT>) -> bool {
    let mut pd = PRINTDLGW {
        lStructSize: size_of::<PRINTDLGW>() as u32,
        hwndOwner: owner,
        Flags: PD_RETURNDC | PD_NOPAGENUMS | PD_NOSELECTION | PD_USEDEVMODECOPIESANDCOLLATE,
        ..Default::default()
    };
    if !PrintDlgW(&mut pd).as_bool() {
        return false;
    }
    for memory in [pd.hDevMode, pd.hDevNames] {
        if !memory.is_invalid() {
            let _ = GlobalFree(Some(memory));
        }
    }
    if pd.hDC.is_invalid() {
        return false;
    }
    let m = margins.unwrap_or(RECT { left: DEFAULT_MARGIN, top: DEFAULT_MARGIN, right: DEFAULT_MARGIN, bottom: DEFAULT_MARGIN });
    let ok = print_page(pd.hDC, img, doc_name, m);
    let _ = DeleteDC(pd.hDC);
    ok
}

unsafe fn print_page(dc: HDC, img: &DecodedImage, doc_name: &str, margins: RECT) -> bool {
    let name = HSTRING::from(doc_name);
    let doc = DOCINFOW { cbSize: size_of::<DOCINFOW>() as i32, lpszDocName: PCWSTR(name.as_ptr()), ..Default::default() };
    if StartDocW(dc, &doc) <= 0 {
        return false;
    }
    let area = printable_rect(dc, margins);
    let ok = StartPage(dc) > 0 && {
        draw_fitted(dc, img, area);
        EndPage(dc) > 0
    };
    EndDoc(dc) > 0 && ok
}

/// The rectangle inside `margins` (measured from the paper edge) in the device coordinates of
/// the printable area, which starts at the physical offset.
unsafe fn printable_rect(dc: HDC, margins: RECT) -> RECT {
    let caps = |index| GetDeviceCaps(Some(dc), index);
    let (dpi_x, dpi_y) = (caps(LOGPIXELSX), caps(LOGPIXELSY));
    let (off_x, off_y) = (caps(PHYSICALOFFSETX), caps(PHYSICALOFFSETY));
    let (paper_w, paper_h) = (caps(PHYSICALWIDTH), caps(PHYSICALHEIGHT));
    let (page_w, page_h) = (caps(HORZRES), caps(VERTRES));
    let px = |mm100: i32, dpi: i32| (mm100.max(0) as i64 * dpi as i64 / MM100_PER_INCH) as i32;
    RECT {
        left: (px(margins.left, dpi_x) - off_x).clamp(0, page_w),
        top: (px(margins.top, dpi_y) - off_y).clamp(0, page_h),
        right: (paper_w - px(margins.right, dpi_x) - off_x).clamp(0, page_w),
        bottom: (paper_h - px(margins.bottom, dpi_y) - off_y).clamp(0, page_h),
    }
}
