//! Photo view: zoom/pan/loupe geometry and GDI double-buffered rendering with the OSD overlay.

use std::sync::Arc;

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
use crate::smooth;
use crate::state::{Loupe, ViewerState, ZoomMode};

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 50.0;
const ZOOM_STEP: f32 = 1.25;
/// Zooming out to within this fraction of the fit scale snaps back to Fit.
const FIT_SNAP: f32 = 0.04;
/// Manual zoom and the loupe beyond this show the pixels as they are (to judge sharpness);
/// fitting a small image into the window is smoothed at any scale.
const MAX_SMOOTH_ZOOM: f32 = 4.0;
const OSD_MARGIN: i32 = 10;

pub unsafe fn client_size(hwnd: HWND) -> Option<(f32, f32)> {
    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc).ok()?;
    let (w, h) = ((rc.right - rc.left) as f32, (rc.bottom - rc.top) as f32);
    (w > 0.0 && h > 0.0).then_some((w, h))
}

/// Width of the line between the two photos compared.
const DIVIDER: i32 = 2;

/// The area the photo on screen is laid out in: the window, or its right half while comparing
/// (see [`crate::state::Compare`]). Zoom, pan and the loupe work in it.
pub unsafe fn view_size(state: &ViewerState) -> Option<(f32, f32)> {
    let (w, h) = client_size(state.hwnd)?;
    if state.compare.is_none() {
        return Some((w, h));
    }
    Some((((w as i32 - DIVIDER) / 2).max(1) as f32, h))
}

/// A cursor position in the window as a position in the view (see [`view_size`]): over either
/// half while comparing, so the loupe and wheel zoom act on the same spot of both photos.
pub unsafe fn to_view(state: &ViewerState, (x, y): (i32, i32)) -> (i32, i32) {
    match view_size(state) {
        Some((w, _)) if state.compare.is_some() && x >= w as i32 + DIVIDER => {
            (x - w as i32 - DIVIDER, y)
        }
        _ => (x, y),
    }
}

/// Size of the whole picture: the geometry below works in its pixels (100% = one of them per
/// screen pixel), whichever copy is drawn.
fn size(img: &DecodedImage) -> (f32, f32) {
    (img.full.0 as f32, img.full.1 as f32)
}

fn fit_scale(state: &ViewerState, img: &DecodedImage, (w, h): (f32, f32)) -> f32 {
    let (iw, ih) = size(img);
    let s = (w / iw).min(h / ih);
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
            let (iw, ih) = size(img);
            (
                ((view.0 - iw * s) / 2.0).round(),
                ((view.1 - ih * s) / 2.0).round(),
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
        let (iw, ih) = size(img);
        state.offset = (
            clamp_axis(state.offset.0, iw * s, view.0),
            clamp_axis(state.offset.1, ih * s, view.1),
        );
    }
}

/// One zoom step from `current`. A step that would jump over 100% stops there, so + and -
/// reach it whatever scale fitting started from.
fn step_scale(current: f32, zoom_in: bool) -> f32 {
    let next = if zoom_in {
        current * ZOOM_STEP
    } else {
        current / ZOOM_STEP
    };
    let crosses = if zoom_in {
        current < 1.0 - 1e-3 && next > 1.0
    } else {
        current > 1.0 + 1e-3 && next < 1.0
    };
    if crosses { 1.0 } else { next }.clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Zooms by one step keeping the image point under `anchor` fixed.
pub fn zoom_step(
    state: &mut ViewerState,
    img: &DecodedImage,
    view: (f32, f32),
    zoom_in: bool,
    anchor: (f32, f32),
) {
    let new_scale = step_scale(scale(state, img, view), zoom_in);
    let fit = fit_scale(state, img, view);
    if !zoom_in && ((new_scale - fit).abs() / fit < FIT_SNAP) {
        state.loupe = None;
        state.zoom = ZoomMode::Fit;
        return;
    }
    zoom_to(state, img, view, new_scale, anchor);
}

/// Sets the scale keeping the image point under `anchor` fixed.
pub fn zoom_to(
    state: &mut ViewerState,
    img: &DecodedImage,
    view: (f32, f32),
    new_scale: f32,
    anchor: (f32, f32),
) {
    state.loupe = None;
    let current = scale(state, img, view);
    let (ox, oy) = origin(state, img, view);
    let (iw, ih) = size(img);
    let point = (
        (anchor.0 - ox) / current / iw,
        (anchor.1 - oy) / current / ih,
    );
    show_at(state, img, view, new_scale, point, anchor);
}

/// Zooms to `new_scale` with the image point `point` (fractions of the width and height) at
/// window position `at`, as far as the image can still cover the window.
fn show_at(
    state: &mut ViewerState,
    img: &DecodedImage,
    view: (f32, f32),
    new_scale: f32,
    point: (f32, f32),
    at: (f32, f32),
) {
    state.zoom = ZoomMode::Custom(new_scale);
    let (iw, ih) = size(img);
    state.offset = (
        at.0 - point.0 * iw * new_scale,
        at.1 - point.1 * ih * new_scale,
    );
    clamp_offset(state, img, view);
}

/// A zoomed-in view, independent of the picture: its scale and the point of the picture
/// (fractions of the width and height) at the middle of the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeptView {
    scale: f32,
    point: (f32, f32),
}

/// The zoomed-in view of `img`, to show the next photo the same way; `None` when fitted.
pub fn kept_view(state: &ViewerState, img: &DecodedImage, view: (f32, f32)) -> Option<KeptView> {
    let ZoomMode::Custom(scale) = state.zoom else {
        return None;
    };
    let (ox, oy) = origin(state, img, view);
    let (iw, ih) = size(img);
    Some(KeptView {
        scale,
        point: (
            (view.0 / 2.0 - ox) / scale / iw,
            (view.1 / 2.0 - oy) / scale / ih,
        ),
    })
}

/// Shows `img` as `kept` was.
pub fn restore_view(state: &mut ViewerState, img: &DecodedImage, view: (f32, f32), kept: KeptView) {
    let middle = (view.0 / 2.0, view.1 / 2.0);
    show_at(state, img, view, kept.scale, kept.point, middle);
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
    loupe_follow(state, img, view, cursor);
}

/// The cursor's place in the window picks the same place in the image, so moving across the
/// window scans the whole picture.
pub fn loupe_follow(
    state: &mut ViewerState,
    img: &DecodedImage,
    view: (f32, f32),
    cursor: (i32, i32),
) {
    let t = (
        (cursor.0 as f32 / view.0).clamp(0.0, 1.0),
        (cursor.1 as f32 / view.1).clamp(0.0, 1.0),
    );
    let at = (t.0 * view.0, t.1 * view.1);
    show_at(state, img, view, state.config.loupe_scale, t, at);
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
    let view = view_size(state)?;
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
        // Comparing: the reference on the left, the photo on screen on the right.
        let compare = state.compare.clone();
        let (view, at) = match &compare {
            Some(_) => {
                let pane = (win_w - DIVIDER) / 2;
                ((pane as f32, win_h as f32), (pane + DIVIDER, 0))
            }
            None => ((win_w as f32, win_h as f32), (0, 0)),
        };
        let area = view_rect(view, at);
        if let Some(reference) = &compare {
            let current = state.image.clone();
            draw_reference(dc, state, reference, current.as_deref(), view);
            let line = RECT {
                left: view.0 as i32,
                right: at.0,
                ..window
            };
            gdi::fill(dc, line, muted_text_color(bg));
        }
        if let Some(img) = state.image.clone() {
            draw_image(dc, state, img, view, at);
        } else if let Some(prev) = state.previous.clone().filter(|_| state.pending.is_some()) {
            draw_fitted(dc, &prev, area, state.config.smooth_zoom);
        } else if state.load_failed {
            let font = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
            gdi::text(
                dc,
                area,
                tr("Cannot open the image"),
                Some(font),
                muted_text_color(bg),
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
        if state.config.osd.photo() {
            draw_osd(dc, state, at.0);
            if let Some(reference) = &compare {
                let name = reference
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let font = osd_font(dc, state);
                draw_osd_text(dc, &name, font, state.config.osd_font_color);
            }
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

/// Draws the whole image scaled to fit `rect`, centered, keeping its aspect ratio; `smooth`:
/// enlarge with bicubic filtering.
pub unsafe fn draw_fitted(dc: HDC, img: &DecodedImage, rect: RECT, smooth: bool) {
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
    stretch(
        dc,
        img,
        (0, w, dx, dw),
        (0, h, dy, dh),
        filter(s, smooth),
        rect,
    );
}

/// The copy to draw at `scale` (of the whole picture): the one fitted to the screen while its
/// pixels suffice, the whole picture beyond that once it has arrived.
fn copy_for_scale(
    state: &mut ViewerState,
    img: Arc<DecodedImage>,
    scale: f32,
) -> Arc<DecodedImage> {
    let needed = scale * img.full.0 as f32;
    if needed > img.width as f32 + 0.5 {
        if let Some(whole) = state.whole_turned() {
            return whole;
        }
    }
    img
}

/// The photo on screen laid out in `view`, drawn with the view's corner at `at`.
unsafe fn draw_image(
    dc: HDC,
    state: &mut ViewerState,
    img: Arc<DecodedImage>,
    view: (f32, f32),
    at: (i32, i32),
) {
    let s = scale(state, &img, view);
    let origin = origin(state, &img, view);
    let src = copy_for_scale(state, img.clone(), s);
    draw_picture(dc, &src, img.full, s, origin, view, at, smooths(state, s));
}

/// Whether a picture drawn at `scale` is smoothed.
fn smooths(state: &ViewerState, scale: f32) -> bool {
    state.config.smooth_zoom && (state.zoom == ZoomMode::Fit || scale <= MAX_SMOOTH_ZOOM)
}

/// The view of size `view` with its corner at `at`, in window coordinates.
fn view_rect(view: (f32, f32), at: (i32, i32)) -> RECT {
    RECT {
        left: at.0,
        top: at.1,
        right: at.0 + view.0 as i32,
        bottom: at.1 + view.1 as i32,
    }
}

/// The reference photo in the left half, showing what the photo on screen shows: fitted when it
/// is, else the same place at the same zoom relative to its size (photos of one scene shot at
/// different resolutions line up).
unsafe fn draw_reference(
    dc: HDC,
    state: &ViewerState,
    reference: &crate::state::Compare,
    current: Option<&DecodedImage>,
    view: (f32, f32),
) {
    let img = &*reference.image;
    let (iw, ih) = size(img);
    let kept = current.and_then(|cur| Some((cur, kept_view(state, cur, view)?)));
    let (s, origin) = match kept {
        Some((cur, kept)) => {
            let s = kept.scale * cur.full.0 as f32 / img.full.0 as f32;
            let origin = (
                (view.0 / 2.0 - kept.point.0 * iw * s).round(),
                (view.1 / 2.0 - kept.point.1 * ih * s).round(),
            );
            (s, origin)
        }
        None => {
            let s = fit_scale(state, img, view);
            let origin = (
                ((view.0 - iw * s) / 2.0).round(),
                ((view.1 - ih * s) / 2.0).round(),
            );
            (s, origin)
        }
    };
    let src = match &reference.whole {
        Some(whole) if s * img.full.0 as f32 > img.width as f32 + 0.5 => whole,
        _ => img,
    };
    draw_picture(
        dc,
        src,
        img.full,
        s,
        origin,
        view,
        (0, 0),
        smooths(state, s),
    );
}

/// Draws `src`, a copy of a picture of size `full`, at `scale` (of the whole picture) with its
/// corner at `origin` in `view`, the view's corner at `at`.
#[allow(clippy::too_many_arguments)]
unsafe fn draw_picture(
    dc: HDC,
    src: &DecodedImage,
    full: (u32, u32),
    scale: f32,
    (ox, oy): (f32, f32),
    view: (f32, f32),
    at: (i32, i32),
    smooth: bool,
) {
    // Screen pixels per pixel of the copy drawn.
    let (sx, sy) = (
        scale * full.0 as f32 / src.width as f32,
        scale * full.1 as f32 / src.height as f32,
    );
    if let (Some((x0, xl, dx, dw)), Some((y0, yl, dy, dh))) = (
        visible_span(ox, sx, src.width, view.0),
        visible_span(oy, sy, src.height, view.1),
    ) {
        let x = (x0, xl, dx + at.0, dw);
        let y = (y0, yl, dy + at.1, dh);
        stretch(dc, src, x, y, filter(sx, smooth), view_rect(view, at));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    /// GDI HALFTONE: averages the pixels that merge.
    Shrink,
    /// Each pixel becomes a sharp square.
    Pixels,
    /// Bicubic enlarging (Direct2D).
    Smooth,
}

fn filter(scale: f32, smooth: bool) -> Filter {
    if scale < 1.0 {
        Filter::Shrink
    } else if smooth && scale > 1.0 {
        Filter::Smooth
    } else {
        Filter::Pixels
    }
}

/// Copies source columns / rows `(start, len)` onto destination `(start, len)` spans; `bounds`:
/// the area being painted.
unsafe fn stretch(
    dc: HDC,
    img: &DecodedImage,
    (sx, sw, dx, dw): (i32, i32, i32, i32),
    (sy, sh, dy, dh): (i32, i32, i32, i32),
    filter: Filter,
    bounds: RECT,
) {
    if filter == Filter::Smooth && smooth::draw(dc, img, (sx, sw, dx, dw), (sy, sh, dy, dh), bounds)
    {
        return;
    }
    // Point the DIB at the first visible row instead of using ySrc: StretchDIBits interprets
    // ySrc of top-down DIBs inconsistently across Windows versions.
    let stride = img.width as usize * 4;
    let rows = &img.bgra[sy as usize * stride..(sy + sh) as usize * stride];
    let bmi = gdi::bitmap_info(img.width, -sh);
    if filter == Filter::Shrink {
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
                    "width" => img.full.0.to_string(),
                    "height" => img.full.1.to_string(),
                    "mp" => format!("{:.2}", img.full.0 as f64 * img.full.1 as f64 / 1_000_000.0),
                    "zoom" => zoom.map(|z| z.to_string()).unwrap_or_default(),
                    "preview" => {
                        if img.is_preview {
                            tr("RAW preview").to_string()
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
    draw_osd_text_at(dc, (OSD_MARGIN, OSD_MARGIN), text, font, color);
}

/// [`draw_osd_text`] with its top left corner at `at`.
unsafe fn draw_osd_text_at(dc: HDC, at: (i32, i32), text: &str, font: HFONT, color: u32) {
    for (offset, color) in [(1, 0), (0, color)] {
        let (x, y) = (at.0 + offset, at.1 + offset);
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

/// The OSD of the photo on screen, `x` pixels from the left (the right half while comparing).
unsafe fn draw_osd(dc: HDC, state: &mut ViewerState, x: i32) {
    let text = osd_text(state, zoom_percent(state));
    let font = osd_font(dc, state);
    draw_osd_text_at(
        dc,
        (x + OSD_MARGIN, OSD_MARGIN),
        &text,
        font,
        state.config.osd_font_color,
    );
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
    fn zoom_steps_stop_at_actual_size() {
        // From a fit scale of 37%: 46, 58, 72, 90, then 100 instead of 113.
        let mut s = 0.37;
        for _ in 0..5 {
            s = step_scale(s, true);
        }
        assert_eq!(s, 1.0);
        assert_eq!(step_scale(1.0, true), 1.25);
        assert_eq!(step_scale(1.1, false), 1.0);
        assert_eq!(step_scale(1.0, false), 0.8);
        assert_eq!(step_scale(MAX_ZOOM, true), MAX_ZOOM);
    }

    /// Paints a picture whose left half is black and right half white at `scale` the way the
    /// window does, and returns the blue channel of the middle row.
    fn paint_row(scale: f32, origin_x: f32, width: i32) -> Vec<u8> {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
        };
        let img = DecodedImage {
            width: 4,
            height: 4,
            full: (4, 4),
            bgra: (0..16)
                .flat_map(|i| if i % 4 < 2 { [0, 0, 0, 255] } else { [255; 4] })
                .collect(),
            is_preview: false,
            exif: None,
        };
        let height = (4.0 * scale) as i32;
        unsafe {
            let dc = CreateCompatibleDC(None);
            let mut bits = std::ptr::null_mut();
            let bmi = gdi::bitmap_info(width as u32, -height);
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let old = SelectObject(dc, bmp.into());
            let window = gdi::rect(width, height);
            let x = visible_span(origin_x, scale, 4, width as f32).unwrap();
            let y = visible_span(0.0, scale, 4, height as f32).unwrap();
            stretch(dc, &img, x, y, filter(scale, true), window);
            let row = std::slice::from_raw_parts(
                (bits as *const u8).add((height / 2 * width * 4) as usize),
                width as usize * 4,
            );
            let blue = row.iter().step_by(4).copied().collect();
            SelectObject(dc, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(dc);
            blue
        }
    }

    #[test]
    fn enlarged_photos_are_smoothed() {
        for scale in [2.0, 3.0] {
            let w = (4.0 * scale) as i32;
            let row = paint_row(scale, 0.0, w);
            // Square pixels would jump from 0 to 255 at the edge; bicubic leaves a ramp.
            let ramp = row.iter().filter(|&&b| b > 20 && b < 235).count();
            assert!(ramp >= 2, "{scale}x: {row:?}");
            assert!(
                row[0] < 20 && row[w as usize - 1] > 235,
                "{scale}x: {row:?}"
            );
        }
        // Zoomed in and panned, only part of the picture is in the window.
        let row = paint_row(3.0, -3.0, 6);
        assert!(row.iter().any(|&b| b > 20 && b < 235), "{row:?}");
        // 100% is drawn as is.
        assert_eq!(paint_row(1.0, 0.0, 4), [0, 0, 255, 255]);
    }

    #[test]
    fn thousands() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(1234567), "1,234,567");
    }
}
