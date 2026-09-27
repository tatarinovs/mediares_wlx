//! Photo view: zoom/pan/loupe geometry and GDI double-buffered rendering with the OSD overlay.

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetDeviceCaps, GetStockObject, SetBrushOrgEx, SetStretchBltMode, StretchDIBits, COLORONCOLOR,
    DEFAULT_GUI_FONT, DIB_RGB_COLORS, DT_CENTER, DT_EXPANDTABS, DT_LEFT, DT_NOCLIP, DT_NOPREFIX,
    DT_SINGLELINE, DT_TOP, DT_VCENTER, HALFTONE, HDC, HFONT, LOGPIXELSY, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::gdi::{self, Font};
use crate::i18n::tr;
use crate::image_cache::{DecodedImage, BACKGROUND};
use crate::osd_template;
use crate::state::{Loupe, ViewerState, ZoomMode};

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 50.0;
const ZOOM_STEP: f32 = 1.25;
/// Zooming out to within this fraction of the fit scale snaps back to Fit.
const FIT_SNAP: f32 = 0.04;
const OSD_MARGIN: i32 = 10;

pub unsafe fn client_size(hwnd: HWND) -> Option<(f32, f32)> {
    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc).ok()?;
    let (w, h) = ((rc.right - rc.left) as f32, (rc.bottom - rc.top) as f32);
    (w > 0.0 && h > 0.0).then_some((w, h))
}

fn fit_scale(state: &ViewerState, img: &DecodedImage, (w, h): (f32, f32)) -> f32 {
    let s = (w / img.width as f32).min(h / img.height as f32);
    if state.config.no_upscale {
        s.min(1.0)
    } else {
        s
    }
}

/// Current display scale (1.0 = 100%).
pub fn scale(state: &ViewerState, img: &DecodedImage, view: (f32, f32)) -> f32 {
    match state.zoom {
        ZoomMode::Fit => fit_scale(state, img, view),
        ZoomMode::Custom(s) => s,
    }
}

/// Top-left corner of the image in client coordinates.
fn origin(state: &ViewerState, img: &DecodedImage, view: (f32, f32)) -> (f32, f32) {
    match state.zoom {
        ZoomMode::Fit => {
            let s = fit_scale(state, img, view);
            (
                ((view.0 - img.width as f32 * s) / 2.0).round(),
                ((view.1 - img.height as f32 * s) / 2.0).round(),
            )
        }
        ZoomMode::Custom(_) => state.offset,
    }
}

/// Keeps a zoomed image covering the window, or centered when it is smaller than the window.
fn clamp_axis(offset: f32, image_len: f32, window_len: f32) -> f32 {
    if image_len <= window_len {
        ((window_len - image_len) / 2.0).round()
    } else {
        offset.clamp(window_len - image_len, 0.0).round()
    }
}

pub fn clamp_offset(state: &mut ViewerState, img: &DecodedImage, view: (f32, f32)) {
    if let ZoomMode::Custom(s) = state.zoom {
        state.offset = (
            clamp_axis(state.offset.0, img.width as f32 * s, view.0),
            clamp_axis(state.offset.1, img.height as f32 * s, view.1),
        );
    }
}

/// Zooms by one step keeping the image point under `anchor` fixed.
pub fn zoom_step(
    state: &mut ViewerState,
    img: &DecodedImage,
    view: (f32, f32),
    zoom_in: bool,
    anchor: (f32, f32),
) {
    state.loupe = None;
    let current = scale(state, img, view);
    let (ox, oy) = origin(state, img, view);
    let fit = fit_scale(state, img, view);
    let new_scale = if zoom_in {
        current * ZOOM_STEP
    } else {
        current / ZOOM_STEP
    }
    .clamp(MIN_ZOOM, MAX_ZOOM);

    if !zoom_in && ((new_scale - fit).abs() / fit < FIT_SNAP) {
        state.zoom = ZoomMode::Fit;
        return;
    }
    let img_x = (anchor.0 - ox) / current;
    let img_y = (anchor.1 - oy) / current;
    state.zoom = ZoomMode::Custom(new_scale);
    state.offset = (anchor.0 - img_x * new_scale, anchor.1 - img_y * new_scale);
    clamp_offset(state, img, view);
}

pub fn zoom_actual_size(state: &mut ViewerState, img: &DecodedImage, view: (f32, f32)) {
    state.loupe = None;
    state.zoom = ZoomMode::Custom(1.0);
    clamp_offset(state, img, view);
}

/// Starts the loupe: magnify at the configured scale and map the cursor proportionally over the image.
pub fn loupe_begin(
    state: &mut ViewerState,
    img: &DecodedImage,
    view: (f32, f32),
    cursor: (i32, i32),
) {
    state.loupe = Some(Loupe {
        saved_zoom: state.zoom,
        saved_offset: state.offset,
    });
    state.zoom = ZoomMode::Custom(state.config.loupe_scale);
    loupe_follow(state, img, view, cursor);
}

pub fn loupe_follow(
    state: &mut ViewerState,
    img: &DecodedImage,
    view: (f32, f32),
    cursor: (i32, i32),
) {
    let s = state.config.loupe_scale;
    let axis = |pos: i32, image_len: f32, window_len: f32| {
        let t = (pos as f32 / window_len).clamp(0.0, 1.0);
        clamp_axis(-(image_len - window_len) * t, image_len, window_len)
    };
    state.offset = (
        axis(cursor.0, img.width as f32 * s, view.0),
        axis(cursor.1, img.height as f32 * s, view.1),
    );
}

pub fn loupe_end(state: &mut ViewerState) -> bool {
    match state.loupe.take() {
        Some(l) => {
            state.zoom = l.saved_zoom;
            state.offset = l.saved_offset;
            true
        }
        None => false,
    }
}

/// Zoom as an integer percentage for the title/OSD.
pub unsafe fn zoom_percent(state: &ViewerState) -> Option<i32> {
    let img = state.image.as_ref()?;
    let view = client_size(state.hwnd)?;
    Some((scale(state, img, view) * 100.0).round() as i32)
}

/// Source span of one axis that is actually visible, and where it lands in the window.
/// Returns `(src_start, src_len, dst_start, dst_len)`.
fn visible_span(
    origin: f32,
    scale: f32,
    image_len: u32,
    window_len: f32,
) -> Option<(i32, i32, i32, i32)> {
    let (origin, scale) = (origin as f64, scale as f64);
    let s0 = ((-origin) / scale).floor().max(0.0) as u32;
    let s1 = (((window_len as f64 - origin) / scale).ceil().max(0.0) as u32).min(image_len);
    if s1 <= s0 {
        return None;
    }
    let d0 = (origin + s0 as f64 * scale).round() as i32;
    let d1 = (origin + s1 as f64 * scale).round() as i32;
    Some((s0 as i32, (s1 - s0) as i32, d0, (d1 - d0).max(1)))
}

pub unsafe fn paint(
    hdc: HDC,
    state: Option<&mut ViewerState>,
    win_w: i32,
    win_h: i32,
    dirty: RECT,
) {
    let bg = state
        .as_ref()
        .map_or(BACKGROUND, |s| s.config.photo_background);
    gdi::with_buffer(hdc, win_w, win_h, dirty, |dc| unsafe {
        let window = gdi::rect(win_w, win_h);
        gdi::fill(dc, window, bg);
        let Some(state) = state else { return };
        if let Some(img) = state.image.clone() {
            draw_image(dc, state, &img, (win_w as f32, win_h as f32));
        } else if let Some(prev) = state.previous.clone().filter(|_| state.pending.is_some()) {
            draw_fitted(dc, &prev, window);
        } else if state.load_failed {
            let font = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
            gdi::text(
                dc,
                window,
                tr("Не удалось открыть изображение", "Cannot open the image"),
                Some(font),
                muted_text_color(bg),
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
        if state.config.osd.photo() {
            draw_osd(dc, state);
        }
    });
}

/// Gray text that stays readable on the COLORREF `background`.
fn muted_text_color(background: u32) -> u32 {
    let [r, g, b] = [
        background & 0xFF,
        (background >> 8) & 0xFF,
        (background >> 16) & 0xFF,
    ];
    let luma = (r * 299 + g * 587 + b * 114) / 1000;
    if luma > 140 {
        0x0040_4040
    } else {
        0x00A0_A0A0
    }
}

/// Draws the whole image scaled to fit `rect`, centered, keeping its aspect ratio.
pub unsafe fn draw_fitted(dc: HDC, img: &DecodedImage, rect: RECT) {
    let (rw, rh) = (
        (rect.right - rect.left) as f32,
        (rect.bottom - rect.top) as f32,
    );
    if rw <= 0.0 || rh <= 0.0 || img.width == 0 || img.height == 0 {
        return;
    }
    let s = (rw / img.width as f32).min(rh / img.height as f32);
    let (dw, dh) = (
        ((img.width as f32 * s).round() as i32).max(1),
        ((img.height as f32 * s).round() as i32).max(1),
    );
    let (dx, dy) = (
        rect.left + (rw as i32 - dw) / 2,
        rect.top + (rh as i32 - dh) / 2,
    );
    let (w, h) = (img.width as i32, img.height as i32);
    stretch(dc, img, (0, w, dx, dw), (0, h, dy, dh), s < 1.0);
}

unsafe fn draw_image(dc: HDC, state: &ViewerState, img: &DecodedImage, view: (f32, f32)) {
    let s = scale(state, img, view);
    let (ox, oy) = origin(state, img, view);
    if let (Some(x), Some(y)) = (
        visible_span(ox, s, img.width, view.0),
        visible_span(oy, s, img.height, view.1),
    ) {
        stretch(dc, img, x, y, s < 1.0);
    }
}

/// Copies source columns / rows `(start, len)` onto destination `(start, len)` spans.
/// Shrinking uses HALFTONE (averaging), enlarging plain pixel replication.
unsafe fn stretch(
    dc: HDC,
    img: &DecodedImage,
    (sx, sw, dx, dw): (i32, i32, i32, i32),
    (sy, sh, dy, dh): (i32, i32, i32, i32),
    shrinking: bool,
) {
    // Point the DIB at the first visible row instead of using ySrc: StretchDIBits interprets
    // ySrc of top-down DIBs inconsistently across Windows versions.
    let stride = img.width as usize * 4;
    let rows = &img.bgra[sy as usize * stride..(sy + sh) as usize * stride];
    let bmi = gdi::bitmap_info(img.width, -sh);
    if shrinking {
        SetStretchBltMode(dc, HALFTONE);
        let _ = SetBrushOrgEx(dc, 0, 0, None);
    } else {
        SetStretchBltMode(dc, COLORONCOLOR);
    }
    StretchDIBits(
        dc,
        dx,
        dy,
        dw,
        dh,
        sx,
        0,
        sw,
        sh,
        Some(rows.as_ptr() as *const _),
        &bmi,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
}

/// "812 KB", "45.3 MB", "1.27 GB".
pub fn format_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    match bytes as f64 {
        b if b >= 1024.0 * MB => format!("{:.2} GB", b / (1024.0 * MB)),
        b if b >= MB => format!("{:.1} MB", b / MB),
        _ => format!("{} KB", format_thousands(bytes.div_ceil(1024))),
    }
}

pub fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn osd_text(state: &ViewerState, zoom: Option<i32>) -> String {
    let position =
        (!state.dir_files.is_empty()).then_some((state.current_idx, state.dir_files.len()));
    let file =
        |key: &str| osd_template::file_field(&state.file_path, state.file_size, position, key);
    let Some(img) = &state.image else {
        // Still decoding: only what is known without the picture.
        return osd_template::render("{name}< [ {index} / {count} ]>", file);
    };
    osd_template::render(&state.config.photo_osd, |key| {
        file(key)
            .or_else(|| osd_template::exif_field(img.exif.as_ref(), key))
            .or_else(|| {
                Some(match key {
                    "width" => img.width.to_string(),
                    "height" => img.height.to_string(),
                    "mp" => format!("{:.2}", img.width as f64 * img.height as f64 / 1_000_000.0),
                    "zoom" => zoom.map(|z| z.to_string()).unwrap_or_default(),
                    "preview" => {
                        if img.is_preview {
                            tr("превью RAW", "RAW preview").to_string()
                        } else {
                            String::new()
                        }
                    }
                    _ => return None,
                })
            })
    })
}

unsafe fn osd_font(dc: HDC, state: &mut ViewerState) -> HFONT {
    let config = &state.config;
    state
        .osd_font
        .get_or_insert_with(|| unsafe {
            create_osd_font(
                &config.osd_font_name,
                config.osd_font_size,
                GetDeviceCaps(Some(dc), LOGPIXELSY),
            )
        })
        .0
}

/// The OSD font (bold, `size_pt` points at `dpi`); shared by the photo and video OSD.
pub unsafe fn create_osd_font(face: &str, size_pt: i32, dpi: i32) -> Font {
    gdi::create_font(face, -((size_pt * dpi + 36) / 72), true)
}

/// OSD text (may span lines) with a 1px black drop shadow, readable over any picture.
pub unsafe fn draw_osd_text(dc: HDC, text: &str, font: HFONT, color: u32) {
    for (offset, color) in [(1, 0), (0, color)] {
        let (x, y) = (OSD_MARGIN + offset, OSD_MARGIN + offset);
        let at = RECT {
            left: x,
            top: y,
            right: x + 1,
            bottom: y + 1,
        };
        let flags = DT_LEFT | DT_TOP | DT_NOCLIP | DT_NOPREFIX | DT_EXPANDTABS;
        gdi::text(dc, at, text, Some(font), color, flags);
    }
}

unsafe fn draw_osd(dc: HDC, state: &mut ViewerState) {
    let text = osd_text(state, zoom_percent(state));
    let font = osd_font(dc, state);
    draw_osd_text(dc, &text, font, state.config.osd_font_color);
}

/// Client-relative cursor from a mouse message `LPARAM`.
pub fn point_from_lparam(lparam: isize) -> POINT {
    POINT {
        x: (lparam & 0xFFFF) as i16 as i32,
        y: ((lparam >> 16) & 0xFFFF) as i16 as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_image_on_screen() {
        assert_eq!(clamp_axis(100.0, 2000.0, 800.0), 0.0);
        assert_eq!(clamp_axis(-5000.0, 2000.0, 800.0), -1200.0);
        assert_eq!(clamp_axis(-300.0, 2000.0, 800.0), -300.0);
        assert_eq!(clamp_axis(-300.0, 400.0, 800.0), 200.0);
    }

    #[test]
    fn visible_span_crops_to_window() {
        // 1000px image at 4x, shifted 1000px left, 800px window: source 250..450 visible.
        assert_eq!(
            visible_span(-1000.0, 4.0, 1000, 800.0),
            Some((250, 200, 0, 800))
        );
        // Fully visible image is drawn whole.
        assert_eq!(
            visible_span(10.0, 0.5, 1000, 800.0),
            Some((0, 1000, 10, 500))
        );
        // Off-screen.
        assert_eq!(visible_span(900.0, 1.0, 100, 800.0), None);
    }

    #[test]
    fn thousands() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(1234567), "1,234,567");
    }
}
