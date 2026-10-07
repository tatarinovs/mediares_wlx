//! GDI plumbing shared by the viewer's painting code and the dialogs.

use std::cell::RefCell;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, FillRect, IntersectClipRect, Polygon, SelectClipRgn,
    SelectObject, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, DRAW_TEXT_FORMAT, FW_BOLD, FW_NORMAL,
    HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, PS_NULL, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    LoadCursorW, RegisterClassExW, IDC_ARROW, WNDCLASSEXW, WNDCLASS_STYLES, WNDPROC,
};

use crate::module;

/// Owned GDI object, deleted on drop.
pub struct GdiObject<T: Into<HGDIOBJ> + Copy>(pub T);

impl<T: Into<HGDIOBJ> + Copy> Drop for GdiObject<T> {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}

pub type Font = GdiObject<HFONT>;
pub type Brush = GdiObject<HBRUSH>;

/// `height` in logical units (negative: character height, as `CreateFontW` takes it).
pub unsafe fn create_font(face: &str, height: i32, bold: bool) -> Font {
    let weight = if bold { FW_BOLD } else { FW_NORMAL };
    GdiObject(CreateFontW(
        height,
        0,
        0,
        0,
        weight.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        DEFAULT_QUALITY,
        0,
        &HSTRING::from(face),
    ))
}

pub unsafe fn solid_brush(color: u32) -> Brush {
    GdiObject(CreateSolidBrush(COLORREF(color)))
}

/// Registers a window class on this DLL; repeated calls fail harmlessly.
pub unsafe fn register_class(
    name: PCWSTR,
    proc: WNDPROC,
    style: WNDCLASS_STYLES,
    background: HBRUSH,
) {
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style,
        lpfnWndProc: proc,
        hInstance: module(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: background,
        lpszClassName: name,
        ..Default::default()
    };
    RegisterClassExW(&wc);
}

/// Scale of the window's DPI relative to 96.
pub fn dpi_scale(hwnd: HWND) -> f32 {
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => 1.0,
        dpi => dpi as f32 / 96.0,
    }
}

/// `(0, 0) - (width, height)`.
pub fn rect(width: i32, height: i32) -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    }
}

pub fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

pub unsafe fn fill(dc: HDC, r: RECT, color: u32) {
    FillRect(dc, &r, solid_brush(color).0);
}

/// A filled polygon without an outline.
pub unsafe fn polygon(dc: HDC, points: &[POINT], color: u32) {
    let brush = solid_brush(color);
    let pen = GdiObject(CreatePen(PS_NULL, 0, COLORREF(0)));
    let old_brush = SelectObject(dc, brush.0.into());
    let old_pen = SelectObject(dc, pen.0.into());
    let _ = Polygon(dc, points);
    SelectObject(dc, old_pen);
    SelectObject(dc, old_brush);
}

/// Text in `r` with a transparent background; `font` (if any) is selected for the call only.
pub unsafe fn text(
    dc: HDC,
    r: RECT,
    s: &str,
    font: Option<HFONT>,
    color: u32,
    flags: DRAW_TEXT_FORMAT,
) {
    // An empty Vec's pointer is dangling, and DrawTextW reads the first char even at length 0.
    if s.is_empty() {
        return;
    }
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    let mut rc = r;
    let old = font.map(|f| SelectObject(dc, f.into()));
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, COLORREF(color));
    DrawTextW(dc, &mut wide, &mut rc, flags);
    if let Some(old) = old {
        SelectObject(dc, old);
    }
}

/// Header of a 32-bit BGRA DIB; negative `height` means top-down rows.
pub fn bitmap_info(width: u32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A memory DC with a bitmap of `size` selected into it.
struct BackBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    size: (i32, i32),
}

impl BackBuffer {
    unsafe fn new(hdc: HDC, width: i32, height: i32) -> Option<Self> {
        let dc = CreateCompatibleDC(Some(hdc));
        if dc.is_invalid() {
            return None;
        }
        let bitmap = CreateCompatibleBitmap(hdc, width, height);
        if bitmap.is_invalid() {
            let _ = DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, bitmap.into());
        Some(Self {
            dc,
            bitmap,
            old,
            size: (width, height),
        })
    }
}

impl Drop for BackBuffer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// The last few back buffers of this thread, most recently used last.
struct BackBuffers(Vec<BackBuffer>);

impl Drop for BackBuffers {
    /// Thread-local destructors run when TC unloads the DLL, under the loader lock: whatever
    /// [`release_buffers`] didn't free is left to the process.
    fn drop(&mut self) {
        std::mem::forget(std::mem::take(&mut self.0));
    }
}

/// The viewer, its panel and a dialog may paint in turn, each at its own size.
const KEPT_BUFFERS: usize = 3;

thread_local! {
    static BUFFERS: RefCell<BackBuffers> = const { RefCell::new(BackBuffers(Vec::new())) };
}

/// Frees this thread's back buffers; called when a viewer window closes.
pub fn release_buffers() {
    let _ = BUFFERS.try_with(|b| b.borrow_mut().0.clear());
}

/// Double-buffered painting of a `width` x `height` surface: `paint` draws into a memory DC
/// clipped to `dirty`, and that part is copied to `hdc`. The buffer is kept for the next paint
/// of the same size: panning repaints on every mouse move, and a window-sized bitmap (33 MB at
/// 4K) made and freed each time costs more than the drawing.
pub unsafe fn with_buffer(hdc: HDC, width: i32, height: i32, dirty: RECT, paint: impl FnOnce(HDC)) {
    if width <= 0 || height <= 0 {
        return;
    }
    let kept = BUFFERS
        .try_with(|b| {
            let list = &mut b.borrow_mut().0;
            let pos = list.iter().position(|buf| buf.size == (width, height))?;
            Some(list.remove(pos))
        })
        .ok()
        .flatten();
    let Some(buffer) = kept.or_else(|| BackBuffer::new(hdc, width, height)) else {
        return;
    };
    let mem = buffer.dc;
    IntersectClipRect(mem, dirty.left, dirty.top, dirty.right, dirty.bottom);
    paint(mem);
    let _ = BitBlt(
        hdc,
        dirty.left,
        dirty.top,
        dirty.right - dirty.left,
        dirty.bottom - dirty.top,
        Some(mem),
        dirty.left,
        dirty.top,
        SRCCOPY,
    );
    SelectClipRgn(mem, None);
    let _ = BUFFERS.try_with(|b| {
        let list = &mut b.borrow_mut().0;
        list.push(buffer);
        if list.len() > KEPT_BUFFERS {
            list.remove(0);
        }
    });
}
