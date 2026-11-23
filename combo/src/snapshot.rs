//! Pictures out of the viewer: Ctrl+C (clipboard), Shift+S (the video frame saved next to the
//! video) and TC's thumbnail view (`ListGetPreviewBitmap`).

use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use mediares_core::audio_tags::read_tags;
use mediares_core::image::codecs::jpeg::JpegEncoder;
use mediares_core::image::codecs::png::PngEncoder;
use mediares_core::image::{DynamicImage, ExtendedColorType, ImageEncoder};
use mediares_core::image_decode::{decode_bytes, decode_oriented_fitted};
use mediares_core::probe::{probe_file, MediaType};
use mediares_core::video_frame::video_frame_rgba_fitted;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::Storage::FileSystem::{
    FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, SetClipboardData,
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

const EXPORT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// Reclaim old exports periodically without delaying the viewer's first paint.
pub fn cleanup_on_start(source: &Path) {
    static LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    let now = Instant::now();
    let Ok(mut last) = LAST.try_lock() else {
        return;
    };
    if last.is_some_and(|t| now.duration_since(t) < Duration::from_secs(3600)) {
        return;
    }
    *last = Some(now);
    drop(last);
    let source = source.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("mediares-clipboard-cleanup".into())
        .spawn(move || unsafe {
            if OpenClipboard(None).is_err() {
                return;
            }
            // Do not render or inspect another application's delayed file clipboard.
            // A later copy through this plugin can clean up with its exact protected filename.
            let sequence = IsClipboardFormatAvailable(CF_HDROP)
                .is_err()
                .then(|| GetClipboardSequenceNumber());
            let _ = CloseClipboard();
            if let Some(sequence) = sequence {
                // A PNG being opened may not have a reader yet, or may already be decoded.
                cleanup_exports(
                    &temp_dir(),
                    Some(&source),
                    SystemTime::now(),
                    Some(sequence),
                );
            }
        });
}

/// Walk the filesystem outside the clipboard lock. Verify the clipboard at each deletion.
fn cleanup_exports(dir: &Path, current: Option<&Path>, now: SystemTime, clipboard: Option<u32>) {
    let Ok(root) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if !root.is_dir() || root.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let expired = |m: &std::fs::Metadata| {
        m.modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= EXPORT_RETENTION)
    };
    let current = current
        .and_then(Path::file_name)
        .map(|name| name.encode_wide().collect::<Vec<_>>());
    for entry in entries.flatten() {
        let path = entry.path();
        if !mediares_core::probe::has_extension(&path, &["png"])
            || !entry.file_type().is_ok_and(|t| t.is_file())
            || current.as_ref().is_some_and(|name| unsafe {
                // Protect the basename even if the clipboard uses a verbatim path prefix.
                CompareStringOrdinal(
                    name,
                    &entry.file_name().encode_wide().collect::<Vec<_>>(),
                    true,
                ) == CSTR_EQUAL
            })
            || !entry.metadata().is_ok_and(|m| expired(&m))
        {
            continue;
        }
        // Exclusive access rejects even readers which allow FILE_SHARE_DELETE. Keep the
        // handle until deletion is marked: checking for a reader and then unlinking by name
        // would leave a race. OPEN_REPARSE_POINT prevents following a replaced symlink.
        let Ok(file) = std::fs::OpenOptions::new()
            .access_mode(DELETE.0 | FILE_READ_ATTRIBUTES.0)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&path)
        else {
            continue;
        };
        let Ok(meta) = file.metadata() else { continue };
        if !meta.is_file()
            || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            || !expired(&meta)
        {
            continue;
        }
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        unsafe {
            if let Some(sequence) = clipboard {
                if sequence == 0 || OpenClipboard(None).is_err() {
                    return;
                }
                if GetClipboardSequenceNumber() != sequence {
                    let _ = CloseClipboard();
                    return;
                }
            }
            let _ = SetFileInformationByHandle(
                HANDLE(file.as_raw_handle()),
                FileDispositionInfo,
                &disposition as *const _ as *const _,
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            );
            if clipboard.is_some() {
                let _ = CloseClipboard();
            }
        }
    }
}

/// Saves a unique clipboard export. Cleanup happens only after publishing the new clipboard.
pub fn save_temp_png(img: &DecodedImage, source: &Path, suffix: &str) -> Option<PathBuf> {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let stem = source.file_stem()?.to_string_lossy();
    save_unique_picture(
        img,
        &dir.join("clipboard.png"),
        &format!("{stem}_{suffix}"),
        PictureFormat::Png,
    )
}

/// Puts the picture on the clipboard as `CF_DIB` (bottom-up: top-down DIBs confuse many apps),
/// and `file` as `CF_HDROP` so that pasting into a folder creates a file.
pub unsafe fn copy_to_clipboard(owner: HWND, img: &DecodedImage, file: Option<&Path>) -> bool {
    let Some(row) = (img.width as usize).checked_mul(4).filter(|&row| row > 0) else {
        return false;
    };
    if img.height == 0 || row.checked_mul(img.height as usize) != Some(img.bgra.len()) {
        return false;
    }
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
    if EmptyClipboard().is_err() {
        let _ = CloseClipboard();
        let _ = GlobalFree(Some(dib));
        return false;
    }
    let ok = set_clipboard(CF_DIB, dib);
    if let Some(drop) = file.and_then(|f| file_drop(f)) {
        set_clipboard(CF_HDROP, drop);
    }
    let sequence = ok.then(|| GetClipboardSequenceNumber());
    let _ = CloseClipboard();
    if let Some(sequence) = sequence {
        cleanup_exports(&temp_dir(), file, SystemTime::now(), Some(sequence));
    }
    ok
}

/// A video position for a file name: "1-23.456" (no colons), rounded to the millisecond.
pub fn time_label(position: f64) -> String {
    let millis = (position.max(0.0) * 1000.0).round() as u64;
    format!(
        "{}.{:03}",
        format_time((millis / 1000) as f64),
        millis % 1000
    )
    .replace(':', "-")
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
    mediares_core::file_write::replace(path, |out| encode_picture(img, out, format)).is_ok()
}

/// Reserve the name while creating it: concurrent snapshots never overwrite one another.
pub fn save_unique_picture(
    img: &DecodedImage,
    neighbour: &Path,
    stem: &str,
    format: PictureFormat,
) -> Option<PathBuf> {
    for n in 1..=10000 {
        let name = if n == 1 {
            stem.to_string()
        } else {
            format!("{stem} ({n})")
        };
        let path = neighbour.with_file_name(format!("{name}.{}", format.extension()));
        match mediares_core::file_write::write_new(&path, |out| encode_picture(img, out, format)) {
            Ok(()) => return Some(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

fn encode_picture(
    img: &DecodedImage,
    out: &mut dyn Write,
    format: PictureFormat,
) -> io::Result<()> {
    let rgb: Vec<u8> = img
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|px| [px[2], px[1], px[0]])
        .collect();
    let (w, h, color) = (img.width, img.height, ExtendedColorType::Rgb8);
    match format {
        PictureFormat::Png => PngEncoder::new(out).write_image(&rgb, w, h, color),
        PictureFormat::Jpeg => {
            JpegEncoder::new_with_quality(out, JPEG_QUALITY).write_image(&rgb, w, h, color)
        }
    }
    .map_err(io::Error::other)
}

/// Picture representing the file: the photo (decoded no larger than needed for a square of
/// `side`), a video frame, or the album art.
fn source_picture(path: &Path, side: u32) -> Option<DynamicImage> {
    match probe_file(path) {
        kind if kind.is_image_kind() => {
            decode_oriented_fitted(path, kind, true, Some((side, side))).map(|(img, ..)| img)
        }
        // libmpv first when installed, like playback: it decodes more, and faster.
        MediaType::Video => playback_mpv::video_frame(path, THUMBNAIL_AT, (side, side))
            .or_else(|| video_frame_rgba_fitted(path, THUMBNAIL_AT, Some((side, side))))
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
        let _ = DeleteObject(bitmap.into());
        return None;
    }
    std::ptr::copy_nonoverlapping(img.bgra.as_ptr(), bits as *mut u8, img.bgra.len());
    Some(bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_only_expired_unreferenced_pngs_without_open_readers() {
        use std::io::Read;

        let now = SystemTime::now();
        let dir = std::env::temp_dir().join(format!(
            "mediares-cleanup-{}-{}",
            std::process::id(),
            now.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        for name in [
            "old.png",
            "busy.png",
            "Текущий.png",
            "recent.png",
            "other.txt",
        ] {
            let path = dir.join(name);
            std::fs::write(&path, b"export").unwrap();
            if name != "recent.png" {
                std::fs::File::options()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_times(
                        std::fs::FileTimes::new()
                            .set_modified(now - EXPORT_RETENTION - Duration::from_secs(1)),
                    )
                    .unwrap();
            }
        }
        std::fs::create_dir(dir.join("directory.png")).unwrap();
        // Rust's normal reader allows FILE_SHARE_DELETE: a plain remove_file would unlink it.
        let mut reader = std::fs::File::open(dir.join("busy.png")).unwrap();
        let protected = dir.join("ТЕКУЩИЙ.PNG");
        cleanup_exports(&dir, Some(&protected), now, None);
        assert!(!dir.join("old.png").exists());
        for name in [
            "busy.png",
            "Текущий.png",
            "recent.png",
            "other.txt",
            "directory.png",
        ] {
            assert!(dir.join(name).exists(), "must keep {name}");
        }
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"export");
        drop(reader);
        cleanup_exports(&dir, None, now, None);
        assert!(!dir.join("busy.png").exists());
        assert!(!dir.join("Текущий.png").exists());
        std::fs::remove_file(dir.join("recent.png")).unwrap();
        std::fs::remove_file(dir.join("other.txt")).unwrap();
        std::fs::remove_dir(dir.join("directory.png")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn concurrent_snapshots_reserve_distinct_names_and_keep_existing_files() {
        let dir = std::env::temp_dir().join(format!("mediares-snap-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("frame.png");
        std::fs::write(&existing, b"keep this file").unwrap();
        let image = std::sync::Arc::new(image_cache::to_bgra(
            mediares_core::image::RgbImage::from_pixel(
                2,
                2,
                mediares_core::image::Rgb([20, 30, 40]),
            )
            .into(),
            0,
            false,
        ));
        let jobs: Vec<_> = (0..4)
            .map(|_| {
                let (image, dir) = (image.clone(), dir.clone());
                std::thread::spawn(move || {
                    save_unique_picture(&image, &dir.join("video.mp4"), "frame", PictureFormat::Png)
                        .unwrap()
                })
            })
            .collect();
        let paths: std::collections::HashSet<_> =
            jobs.into_iter().map(|job| job.join().unwrap()).collect();
        assert_eq!(paths.len(), 4);
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep this file");
        for path in paths {
            assert_eq!(mediares_core::image::open(&path).unwrap().width(), 2);
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_file(existing).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn frame_names_are_unique_and_file_system_safe() {
        let dir = std::env::temp_dir().join(format!("mediares_snap_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mp4");
        let stem = format!("clip_{}", time_label(83.4567));
        let first = unique_path(&video, &stem, PictureFormat::Png);
        assert_eq!(first.file_name().unwrap(), "clip_1-23.457.png");
        std::fs::write(&first, b"x").unwrap();
        assert_eq!(
            unique_path(&video, &stem, PictureFormat::Png)
                .file_name()
                .unwrap(),
            "clip_1-23.457 (2).png"
        );
        assert_eq!(
            unique_path(&video, &stem, PictureFormat::Jpeg)
                .file_name()
                .unwrap(),
            "clip_1-23.457.jpg"
        );
        // Rounding up to the next second carries over.
        assert_eq!(time_label(83.9996), "1-24.000");
        let _ = std::fs::remove_dir_all(dir);
    }
}
