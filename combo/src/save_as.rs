//! "Сохранить как" for photos: JPEG or PNG.
//!
//! Losslessly when possible: a file already in the chosen format is copied, and a RAW saved as
//! JPEG gets its embedded preview as is, with the RAW's metadata written into it (the way the
//! camera's own JPEG would have it). Otherwise the file is decoded again at full resolution,
//! turned like on screen and encoded with the main EXIF fields.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use mediares_core::exif::{build_exif, read_exif, ExifInfo};
use mediares_core::image::codecs::jpeg::JpegEncoder;
use mediares_core::image::codecs::png::PngEncoder;
use mediares_core::image::{DynamicImage, ExtendedColorType, ImageEncoder, Rgb, RgbImage};
use mediares_core::image_decode::decode_oriented;
use mediares_core::jpeg::with_exif;
use mediares_core::probe::{probe_file, MediaType};
use mediares_core::raw_preview::extract_raw_preview;
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::Dialogs::{
    GetSaveFileNameW, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST,
    OPENFILENAMEW,
};
use windows::Win32::UI::WindowsAndMessaging::{LoadCursorW, SetCursor, IDC_WAIT};

use crate::file_actions::show_error;
use crate::i18n::tr;
use crate::snapshot::{unique_path, PictureFormat as Format, JPEG_QUALITY};
use crate::state::same_path;

/// The format chosen last time; the dialog offers it again.
static LAST_PNG: AtomicBool = AtomicBool::new(false);

/// Asks for a target and saves the photo `source`, turned by `quarter_turns` clockwise like on
/// screen (`auto_rotate`: EXIF orientation applied, as in the viewer).
pub unsafe fn save_as(owner: HWND, source: &Path, quarter_turns: u8, auto_rotate: bool) {
    let Some((target, format)) = ask_target(owner, source) else {
        return;
    };
    LAST_PNG.store(format == Format::Png, Ordering::Relaxed);
    if same_path(&target, source) {
        show_error(owner, tr("Cannot save over the open file."));
        return;
    }
    let previous = LoadCursorW(None, IDC_WAIT).map(|c| SetCursor(Some(c)));
    let ok = write(source, &target, format, quarter_turns % 4, auto_rotate);
    if let Ok(previous) = previous {
        SetCursor(Some(previous));
    }
    if !ok {
        show_error(
            owner,
            &format!("{}:\n{}", tr("Could not save"), target.display()),
        );
    }
}

/// Writes into a temporary file next to `target` and only then replaces it: a failure leaves an
/// existing file the user chose to overwrite as it was.
fn write(
    source: &Path,
    target: &Path,
    format: Format,
    quarter_turns: u8,
    auto_rotate: bool,
) -> bool {
    let mut temp = target.as_os_str().to_owned();
    temp.push(".mediares-tmp");
    let temp = PathBuf::from(temp);
    let ok = write_to(source, &temp, format, quarter_turns, auto_rotate)
        && std::fs::rename(&temp, target).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&temp);
    }
    ok
}

fn write_to(
    source: &Path,
    target: &Path,
    format: Format,
    quarter_turns: u8,
    auto_rotate: bool,
) -> bool {
    let kind = probe_file(source);
    if quarter_turns == 0 {
        if let Some(bytes) = lossless(source, kind, format) {
            return std::fs::write(target, bytes).is_ok();
        }
    }
    let Some((mut img, exif)) = decode_oriented(source, kind, auto_rotate) else {
        return false;
    };
    for _ in 0..quarter_turns {
        img = img.rotate90();
    }
    encode(img, exif.as_ref(), target, format)
}

/// The file's own bytes, or a RAW's embedded JPEG with the RAW's metadata, when they already are
/// in the requested format.
fn lossless(source: &Path, kind: MediaType, format: Format) -> Option<Vec<u8>> {
    match (kind, format) {
        (MediaType::RawImage, Format::Jpeg) => {
            let preview = extract_raw_preview(source)?;
            let exif = read_exif(source).unwrap_or_default();
            with_exif(&preview, &build_exif(&exif, exif.orientation.unwrap_or(1)))
        }
        (MediaType::StandardImage, _) => {
            let bytes = std::fs::read(source).ok()?;
            let same = match format {
                Format::Jpeg => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
                Format::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            };
            same.then_some(bytes)
        }
        _ => None,
    }
}

/// Pixels are already upright, so the orientation written is 1.
fn encode(img: DynamicImage, exif: Option<&ExifInfo>, target: &Path, format: Format) -> bool {
    let Ok(file) = File::create(target) else {
        return false;
    };
    let out = BufWriter::new(file);
    let exif = exif.map(|e| build_exif(e, 1));
    match format {
        Format::Png => {
            let mut encoder = PngEncoder::new(out);
            if let Some(exif) = exif {
                let _ = encoder.set_exif_metadata(exif);
            }
            img.write_with_encoder(encoder).is_ok()
        }
        Format::Jpeg => {
            let rgb = on_white(img);
            let mut encoder = JpegEncoder::new_with_quality(out, JPEG_QUALITY);
            if let Some(exif) = exif {
                let _ = encoder.set_exif_metadata(exif);
            }
            encoder
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    ExtendedColorType::Rgb8,
                )
                .is_ok()
        }
    }
}

/// JPEG has no transparency: blend it onto white, as image editors do.
fn on_white(img: DynamicImage) -> RgbImage {
    if !img.color().has_alpha() {
        return img.into_rgb8();
    }
    let rgba = img.into_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend = |c: u8| ((c as u32 * a as u32 + 255 * (255 - a as u32)) / 255) as u8;
        Rgb([blend(r), blend(g), blend(b)])
    })
}

/// "<name>.jpg" next to the source, " (2)", " (3)"... if taken.
fn free_name(source: &Path, format: Format) -> PathBuf {
    let stem = source.file_stem().unwrap_or_default().to_string_lossy();
    unique_path(source, &stem, format)
}

/// The save dialog. The format follows the typed extension, else the selected file type (whose
/// extension is then added).
unsafe fn ask_target(owner: HWND, source: &Path) -> Option<(PathBuf, Format)> {
    let format = if LAST_PNG.load(Ordering::Relaxed) {
        Format::Png
    } else {
        Format::Jpeg
    };
    let initial = free_name(source, format);
    let mut file = [0u16; 1024];
    let name: Vec<u16> = initial
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .encode_utf16()
        .take(file.len() - 1)
        .collect();
    file[..name.len()].copy_from_slice(&name);
    let dir = HSTRING::from(source.parent().unwrap_or(Path::new("")).as_os_str());
    let title = HSTRING::from(tr("Save as"));
    let mut ofn = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: w!("JPEG (*.jpg)\0*.jpg;*.jpeg\0PNG (*.png)\0*.png\0"),
        nFilterIndex: if format == Format::Png { 2 } else { 1 },
        lpstrFile: PWSTR(file.as_mut_ptr()),
        nMaxFile: file.len() as u32,
        lpstrInitialDir: PCWSTR(dir.as_ptr()),
        lpstrTitle: PCWSTR(title.as_ptr()),
        lpstrDefExt: w!("jpg"),
        Flags: OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_HIDEREADONLY,
        ..Default::default()
    };
    if !GetSaveFileNameW(&mut ofn).as_bool() {
        return None;
    }
    let len = file.iter().position(|&c| c == 0).unwrap_or(file.len());
    let path = PathBuf::from(String::from_utf16_lossy(&file[..len]));
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    Some(match ext.as_str() {
        "png" => (path, Format::Png),
        "jpg" | "jpeg" => (path, Format::Jpeg),
        _ => {
            let format = if ofn.nFilterIndex == 2 {
                Format::Png
            } else {
                Format::Jpeg
            };
            let mut name = path.into_os_string();
            name.push(".");
            name.push(format.extension());
            (PathBuf::from(name), format)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mediares_core::image::RgbaImage;

    #[test]
    fn transparency_goes_onto_white() {
        let img = RgbaImage::from_pixel(1, 1, mediares_core::image::Rgba([0, 0, 0, 0]));
        assert_eq!(on_white(img.into()).get_pixel(0, 0).0, [255, 255, 255]);
        let img = RgbaImage::from_pixel(1, 1, mediares_core::image::Rgba([10, 20, 30, 255]));
        assert_eq!(on_white(img.into()).get_pixel(0, 0).0, [10, 20, 30]);
    }

    #[test]
    fn standard_file_in_the_same_format_is_copied() {
        let dir = std::env::temp_dir().join(format!("mediares_saveas_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        RgbaImage::from_pixel(2, 1, mediares_core::image::Rgba([1, 2, 3, 128]))
            .save(&png)
            .unwrap();
        let original = std::fs::read(&png).unwrap();
        assert_eq!(
            lossless(&png, MediaType::StandardImage, Format::Png),
            Some(original)
        );
        assert_eq!(lossless(&png, MediaType::StandardImage, Format::Jpeg), None);
        assert_eq!(
            free_name(&png, Format::Png).file_name().unwrap(),
            "a (2).png"
        );
        assert_eq!(free_name(&png, Format::Jpeg).file_name().unwrap(), "a.jpg");

        // Turned: re-encoded; the JPEG gets the pixels turned and no alpha.
        let jpg = dir.join("b.jpg");
        assert!(write(&png, &jpg, Format::Jpeg, 1, true));
        let back = mediares_core::image::open(&jpg).unwrap();
        assert_eq!((back.width(), back.height()), (1, 2));
        assert!(!back.color().has_alpha());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Regression: a failed save deleted the file the user had chosen to overwrite.
    #[test]
    fn failed_save_keeps_the_existing_target() {
        let dir = std::env::temp_dir().join(format!("mediares_keep_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (broken, target) = (dir.join("broken.jpg"), dir.join("target.png"));
        std::fs::write(&broken, [0xFF, 0xD8, 0xFF, 0x00, 0x13, 0x37]).unwrap();
        std::fs::write(&target, b"precious").unwrap();

        assert!(!write(&broken, &target, Format::Png, 1, true));
        assert_eq!(std::fs::read(&target).unwrap(), b"precious");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(left.len(), 2, "no temporary file left behind");

        // A successful save replaces it.
        let png = dir.join("ok.png");
        RgbaImage::from_pixel(2, 1, mediares_core::image::Rgba([1, 2, 3, 255]))
            .save(&png)
            .unwrap();
        assert!(write(&png, &target, Format::Png, 1, true));
        assert_eq!(mediares_core::image::open(&target).unwrap().width(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
