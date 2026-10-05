//! Pictures out of the viewer: Ctrl+C (clipboard), Shift+S (the video frame saved next to the
//! video) and TC's thumbnail view (`ListGetPreviewBitmap`).

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use mediares_core::audio_tags::read_tags;
use mediares_core::image::codecs::jpeg::JpegEncoder;
use mediares_core::image::codecs::png::PngEncoder;
use mediares_core::image::{DynamicImage, ExtendedColorType, ImageEncoder};
use mediares_core::image_decode::{decode_bytes, decode_oriented_fitted};
use mediares_core::probe::{probe_file, MediaType};
use mediares_core::video_frame::video_frame_rgba;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::{CreateDIBSection, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
};
use windows::Win32::UI::Shell::DROPFILES;

use crate::audio_view::folder_cover;
use crate::i18n::tr;
use crate::image_cache::{self, DecodedImage};
use crate::playback_mpv;
use crate::transport_bar::format_time;

const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;
/// Thumbnails of videos show the frame here (the very first ones are often black).
const THUMBNAIL_AT: f64 = 0.1;
/// TC shows thumbnails on the window background: transparency is flattened onto white.
const THUMBNAIL_BACKGROUND: u32 = 0x00FF_FFFF;

/// A global memory block for the clipboard filled by `fill`.
unsafe fn global_block(size: usize, fill: impl FnOnce(*mut u8)) -> Option<HGLOBAL> {
    let memory = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size).ok()?;
    let dest = GlobalLock(memory) as *mut u8;
    if dest.is_null() {
        let _ = GlobalFree(Some(memory));
        return None;
    }
    fill(dest);
    let _ = GlobalUnlock(memory);
    Some(memory)
}

/// `CF_HDROP`: a `DROPFILES` header followed by the double-NUL-terminated wide path.
unsafe fn file_drop(path: &Path) -> Option<HGLOBAL> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        fWide: true.into(),
        ..Default::default()
    };
    global_block(size_of::<DROPFILES>() + wide.len() * 2, |dest| unsafe {
        std::ptr::copy_nonoverlapping(
            &header as *const _ as *const u8,
            dest,
            size_of::<DROPFILES>(),
        );
        std::ptr::copy_nonoverlapping(
            wide.as_ptr() as *const u8,
            dest.add(size_of::<DROPFILES>()),
            wide.len() * 2,
        );
    })
}

unsafe fn set_clipboard(format: u32, memory: HGLOBAL) -> bool {
    // On success the clipboard owns the memory.
    let ok = SetClipboardData(format, Some(HANDLE(memory.0))).is_ok();
    if !ok {
        let _ = GlobalFree(Some(memory));
    }
    ok
}

/// Where Ctrl+C keeps pictures that exist only in memory, so they can be pasted as files.
fn temp_dir() -> PathBuf {
    std::env::temp_dir().join("Mediares")
}

/// Saves `img` as "<name>_<time>.png" (or "<name>_cover.png") in the temp folder, clearing out
/// earlier ones. Returns the file for `CF_HDROP`.
pub fn save_temp_png(img: &DecodedImage, source: &Path, position: Option<f64>) -> Option<PathBuf> {
    let dir = temp_dir();
    // Files from the last minute are kept: a paste of them may still be in progress, possibly
    // from another Total Commander instance sharing the folder.
    let stale = |entry: &std::fs::DirEntry| {
        entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 60)
    };
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten().filter(stale) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    std::fs::create_dir_all(&dir).ok()?;
    let name = match position {
        Some(t) => frame_file_name(source, t, PictureFormat::Png)
            .file_name()?
            .to_os_string(),
        None => format!("{}_cover.png", source.file_stem()?.to_string_lossy()).into(),
    };
    let path = dir.join(name);
    save_png(img, &path).then_some(path)
}

/// Puts the picture on the clipboard as `CF_DIB` (bottom-up: top-down DIBs confuse many apps),
/// and `file` as `CF_HDROP` so that pasting into a folder creates a file.
pub unsafe fn copy_to_clipboard(owner: HWND, img: &DecodedImage, file: Option<&Path>) -> bool {
    let row = img.width as usize * 4;
    let head = crate::gdi::bitmap_info(img.width, img.height as i32).bmiHeader;
    let dib = global_block(
        size_of::<BITMAPINFOHEADER>() + row * img.height as usize,
        |dest| unsafe {
            std::ptr::copy_nonoverlapping(
                &head as *const _ as *const u8,
                dest,
                size_of::<BITMAPINFOHEADER>(),
            );
            let pixels = dest.add(size_of::<BITMAPINFOHEADER>());
            for (y, line) in img.bgra.chunks_exact(row).enumerate() {
                std::ptr::copy_nonoverlapping(
                    line.as_ptr(),
                    pixels.add((img.height as usize - 1 - y) * row),
                    row,
                );
            }
        },
    );
    let Some(dib) = dib else { return false };
    if OpenClipboard(Some(owner)).is_err() {
        let _ = GlobalFree(Some(dib));
        return false;
    }
    let _ = EmptyClipboard();
    let ok = set_clipboard(CF_DIB, dib);
    if let Some(drop) = file.and_then(|f| file_drop(f)) {
        set_clipboard(CF_HDROP, drop);
    }
    let _ = CloseClipboard();
    ok
}

/// "clip_0-01-23.456.png" next to the video; " (2)", " (3)"... if taken.
pub fn frame_file_name(video: &Path, position: f64, format: PictureFormat) -> PathBuf {
    let stem = video
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let millis = ((position.max(0.0) * 1000.0).round() as u64) % 1000;
    let time = format!("{}.{:03}", format_time(position), millis).replace(':', "-");
    unique_path(video, &format!("{}_{}", stem, time), format)
}

/// "<stem>.<ext>" next to `neighbour`; " (2)", " (3)"... if taken.
pub fn unique_path(neighbour: &Path, stem: &str, format: PictureFormat) -> PathBuf {
    let dir = neighbour.parent().unwrap_or(Path::new(""));
    let ext = format.extension();
    (1..)
        .map(|n| match n {
            1 => dir.join(format!("{stem}.{ext}")),
            n => dir.join(format!("{stem} ({n}).{ext}")),
        })
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

/// How pictures are saved: video frames (Shift+S) and "Save as".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureFormat {
    Png,
    Jpeg,
}

impl PictureFormat {
    pub const ALL: [PictureFormat; 2] = [PictureFormat::Png, PictureFormat::Jpeg];

    pub fn from_ini(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => PictureFormat::Jpeg,
            _ => PictureFormat::Png,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            PictureFormat::Png => "png",
            PictureFormat::Jpeg => "jpg",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PictureFormat::Png => tr("PNG — lossless"),
            PictureFormat::Jpeg => tr("JPEG — smaller"),
        }
    }
}

/// JPEG quality of saved frames: artifacts are hard to see, files are several times smaller than PNG.
pub const JPEG_QUALITY: u8 = 92;

/// PNG without alpha: pictures on screen are always opaque.
pub fn save_png(img: &DecodedImage, path: &Path) -> bool {
    save_picture(img, path, PictureFormat::Png)
}

pub fn save_picture(img: &DecodedImage, path: &Path, format: PictureFormat) -> bool {
    let rgb: Vec<u8> = img
        .bgra
        .chunks_exact(4)
        .flat_map(|px| [px[2], px[1], px[0]])
        .collect();
    let Ok(file) = std::fs::File::create(path) else {
        return false;
    };
    let out = std::io::BufWriter::new(file);
    let (w, h, color) = (img.width, img.height, ExtendedColorType::Rgb8);
    let ok = match format {
        PictureFormat::Png => PngEncoder::new(out).write_image(&rgb, w, h, color),
        PictureFormat::Jpeg => {
            JpegEncoder::new_with_quality(out, JPEG_QUALITY).write_image(&rgb, w, h, color)
        }
    }
    .is_ok();
    if !ok {
        let _ = std::fs::remove_file(path);
    }
    ok
}

/// Picture representing the file: the photo (decoded no larger than needed for a square of
/// `side`), a video frame, or the album art.
fn source_picture(path: &Path, side: u32) -> Option<DynamicImage> {
    match probe_file(path) {
        kind if kind.is_image_kind() => {
            decode_oriented_fitted(path, kind, true, Some((side, side))).map(|(img, ..)| img)
        }
        // libmpv first when installed, like playback: it decodes more, and faster.
        MediaType::Video => playback_mpv::video_frame(path, THUMBNAIL_AT)
            .or_else(|| video_frame_rgba(path, THUMBNAIL_AT))
            .map(DynamicImage::ImageRgba8),
        MediaType::Audio => {
            let embedded = read_tags(path, true)
                .and_then(|t| t.cover)
                .and_then(|bytes| decode_bytes(&bytes));
            match embedded {
                Some(img) => Some(img),
                None => {
                    let cover = folder_cover(path)?;
                    image_cache::to_dynamic(&cover)
                }
            }
        }
        _ => None,
    }
}

/// Fitted into `max_w` x `max_h` (never enlarged), transparency flattened onto white.
pub fn thumbnail(path: &Path, max_w: u32, max_h: u32) -> Option<DecodedImage> {
    let img = source_picture(path, max_w.max(max_h).max(1))?;
    let img = if img.width() > max_w || img.height() > max_h {
        mediares_core::image_decode::thumbnail(&img, max_w.max(1), max_h.max(1))
    } else {
        img
    };
    Some(image_cache::to_bgra(img, THUMBNAIL_BACKGROUND, false))
}

/// A top-down 32-bit DIB section with the picture; the caller (TC) owns it.
pub unsafe fn to_hbitmap(img: &DecodedImage) -> Option<HBITMAP> {
    let bmi = crate::gdi::bitmap_info(img.width, -(img.height as i32));
    let mut bits = std::ptr::null_mut();
    let bitmap = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(img.bgra.as_ptr(), bits as *mut u8, img.bgra.len());
    Some(bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_names_are_unique_and_file_system_safe() {
        let dir = std::env::temp_dir().join(format!("mediares_snap_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mp4");
        let first = frame_file_name(&video, 83.4567, PictureFormat::Png);
        assert_eq!(first.file_name().unwrap(), "clip_1-23.457.png");
        std::fs::write(&first, b"x").unwrap();
        assert_eq!(
            frame_file_name(&video, 83.4567, PictureFormat::Png)
                .file_name()
                .unwrap(),
            "clip_1-23.457 (2).png"
        );
        assert_eq!(
            frame_file_name(&video, 83.4567, PictureFormat::Jpeg)
                .file_name()
                .unwrap(),
            "clip_1-23.457.jpg"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
