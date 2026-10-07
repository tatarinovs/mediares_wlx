//! Contact sheet of a video: frames from along the whole file in a grid, each with its time,
//! under a line naming the file and its streams. Made on a thread of its own (a seek and a
//! decoded frame per picture) and saved next to the video in the format chosen for frames.

use std::path::{Path, PathBuf};

use mediares_core::image::{DynamicImage, RgbaImage};
use mediares_core::video_frame::FrameGrabber;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, DIB_RGB_COLORS,
    DT_BOTTOM, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_TOP,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::gdi;
use crate::i18n::tr;
use crate::image_cache::{to_bgra, DecodedImage};
use crate::image_view::draw_fitted;
use crate::snapshot::{save_picture, unique_path, PictureFormat};
use crate::transport_bar::format_time;

/// Posted to the viewer when the sheet is saved (or failed); `lparam` owns a `Box<String>` with
/// the message for the bar.
pub const WM_CONTACT_SHEET: u32 = WM_APP + 0x15;

const COLUMNS: u32 = 4;
const ROWS: u32 = 4;
const CELL_WIDTH: u32 = 480;
const MARGIN: i32 = 12;
const GAP: i32 = 6;
const HEADER: i32 = 64;
const BACKGROUND: u32 = 0x0018_1818;
const TEXT: u32 = 0x00E0_E0E0;
const MUTED: u32 = 0x00A0_A0A0;

/// What the header says, read from the player on the viewer's thread.
pub struct SheetInfo {
    pub duration: f64,
    /// "1920x1080 · H.264 · 1:23:45 · 1.2 GB"
    pub details: String,
}

/// Makes the sheet of `video` in the background; `viewer` gets [`WM_CONTACT_SHEET`].
pub fn start(viewer: HWND, video: &Path, info: SheetInfo, format: PictureFormat) {
    let (video, viewer) = (video.to_path_buf(), viewer.0 as isize);
    // Without a thread the viewer gets no answer; nothing was started either.
    let _ = std::thread::Builder::new()
        .name("mediares-contact-sheet".into())
        .spawn(move || {
            let text = match make(&video, &info, format) {
                Some(saved) => format!(
                    "{}: {}",
                    tr("Contact sheet saved"),
                    saved.file_name().unwrap_or_default().to_string_lossy()
                ),
                None => tr("Could not make the contact sheet").to_string(),
            };
            let message = Box::into_raw(Box::new(text));
            let posted = unsafe {
                PostMessageW(
                    Some(HWND(viewer as *mut _)),
                    WM_CONTACT_SHEET,
                    WPARAM(0),
                    LPARAM(message as isize),
                )
            };
            if posted.is_err() {
                // The viewer is gone: nobody takes the message.
                drop(unsafe { Box::from_raw(message) });
            }
        });
}

/// The message posted with [`WM_CONTACT_SHEET`].
///
/// # Safety
/// `lparam` must come from that message, taken once.
pub unsafe fn take_message(lparam: LPARAM) -> String {
    *Box::from_raw(lparam.0 as *mut String)
}

/// The saved file.
fn make(video: &Path, info: &SheetInfo, format: PictureFormat) -> Option<PathBuf> {
    let count = COLUMNS * ROWS;
    let times: Vec<f64> = (0..count)
        .map(|i| info.duration * (f64::from(i) + 0.5) / f64::from(count))
        .collect();
    let frames = frames(video, info.duration, &times)?;
    let sheet = unsafe { compose(video, info, &frames)? };
    let stem = video.file_stem()?.to_string_lossy().into_owned();
    let target = unique_path(video, &format!("{stem}_sheet"), format);
    save_picture(&sheet, &target, format).then_some(target)
}

/// A frame of the sheet and the time it shows.
type Shot = (DecodedImage, f64);

/// One frame per time, with the time it shows (a key frame: at or before the one asked for):
/// from Media Foundation, else from libmpv (formats only it plays).
fn frames(video: &Path, duration: f64, times: &[f64]) -> Option<Vec<Option<Shot>>> {
    let fit = (CELL_WIDTH, CELL_WIDTH);
    let shot = |frame: RgbaImage, time| (to_bgra(DynamicImage::ImageRgba8(frame), 0, false), time);
    if let Some(grabber) = FrameGrabber::open(video) {
        return Some(
            times
                .iter()
                .map(|&t| grabber.frame_at(t, Some(fit)).map(|(f, t)| shot(f, t)))
                .collect(),
        );
    }
    if !crate::playback_mpv::available() || duration <= 0.0 {
        return None;
    }
    let frames: Vec<Option<Shot>> = times
        .iter()
        .map(|&t| {
            crate::playback_mpv::video_frame(video, t / duration).map(|f| {
                let (w, h) = mediares_core::image_decode::fitted_size(f.dimensions(), fit);
                shot(mediares_core::image::imageops::thumbnail(&f, w, h), t)
            })
        })
        .collect();
    frames.iter().any(Option::is_some).then_some(frames)
}

/// Draws the sheet with GDI into a DIB and returns its pixels.
unsafe fn compose(video: &Path, info: &SheetInfo, frames: &[Option<Shot>]) -> Option<DecodedImage> {
    let first = &frames.iter().flatten().next()?.0;
    let (fw, fh) = (first.width, first.height);
    let cell = (CELL_WIDTH as i32, (CELL_WIDTH * fh / fw.max(1)) as i32);
    let width = 2 * MARGIN + COLUMNS as i32 * cell.0 + (COLUMNS as i32 - 1) * GAP;
    let height = HEADER + MARGIN + ROWS as i32 * cell.1 + (ROWS as i32 - 1) * GAP;

    let dc = CreateCompatibleDC(None);
    if dc.is_invalid() {
        return None;
    }
    let bmi = gdi::bitmap_info(width as u32, -height);
    let mut bits = std::ptr::null_mut();
    let Ok(bitmap) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
        let _ = DeleteDC(dc);
        return None;
    };
    let old = SelectObject(dc, bitmap.into());
    gdi::fill(dc, gdi::rect(width, height), BACKGROUND);

    let title_font = gdi::create_font("Segoe UI", -20, true);
    let font = gdi::create_font("Segoe UI", -14, false);
    let label_font = gdi::create_font("Segoe UI", -15, true);
    let name = video.file_name().unwrap_or_default().to_string_lossy();
    let line = |top: i32, bottom: i32| RECT {
        left: MARGIN,
        top,
        right: width - MARGIN,
        bottom,
    };
    let flags = DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS;
    gdi::text(dc, line(10, 36), &name, Some(title_font.0), TEXT, flags);
    gdi::text(
        dc,
        line(38, HEADER),
        &info.details,
        Some(font.0),
        MUTED,
        flags,
    );

    for (i, shot) in frames.iter().enumerate() {
        let (col, row) = (i as i32 % COLUMNS as i32, i as i32 / COLUMNS as i32);
        let x = MARGIN + col * (cell.0 + GAP);
        let y = HEADER + row * (cell.1 + GAP);
        let Some((frame, time)) = shot else {
            continue;
        };
        // Frames all have the video's shape: each fills its cell, small videos enlarged. Not
        // smoothed: that is Direct2D's, kept for the viewer's thread.
        let cell_rect = RECT {
            left: x,
            top: y,
            right: x + cell.0,
            bottom: y + cell.1,
        };
        draw_fitted(dc, frame, cell_rect, false);
        // The time, bottom right, with a shadow.
        let label = format_time(*time);
        let at = RECT {
            right: cell_rect.right - 6,
            bottom: cell_rect.bottom - 4,
            ..cell_rect
        };
        let shadow = RECT {
            left: at.left + 1,
            top: at.top + 1,
            right: at.right + 1,
            bottom: at.bottom + 1,
        };
        let flags = DT_RIGHT | DT_BOTTOM | DT_SINGLELINE | DT_NOPREFIX;
        gdi::text(dc, shadow, &label, Some(label_font.0), 0, flags);
        gdi::text(dc, at, &label, Some(label_font.0), TEXT, flags);
    }

    let len = width as usize * height as usize * 4;
    let mut pixels = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
    pixels.chunks_exact_mut(4).for_each(|px| px[3] = 255);
    SelectObject(dc, old);
    let _ = DeleteObject(bitmap.into());
    let _ = DeleteDC(dc);
    Some(DecodedImage {
        width: width as u32,
        height: height as u32,
        full: (width as u32, height as u32),
        bgra: pixels,
        is_preview: false,
        exif: None,
    })
}
