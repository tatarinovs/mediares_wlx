//! Photo decoding shared by WDX hashing and the WLX viewer: the `image` crate for standard
//! formats plus embedded previews for RAW/PSD. All paths go through the same memory limits.

use std::path::Path;

use image::metadata::Orientation;
use image::{DynamicImage, ImageReader, Limits};

use crate::probe::MediaType;

/// Largest decoded image we accept (pixels). Bounds memory inside the host process.
pub const MAX_PIXELS: u64 = 64_000_000;
const MAX_DIMENSION: u32 = 16_384;
const MAX_DECODER_ALLOC: u64 = 512 * 1024 * 1024;

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODER_ALLOC);
    limits
}

fn within_pixel_budget(img: &DynamicImage) -> bool {
    (img.width() as u64) * (img.height() as u64) <= MAX_PIXELS
}

/// Decodes an in-memory image (format detected from content) under the shared limits.
pub fn decode_bytes(bytes: &[u8]) -> Option<DynamicImage> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    reader.limits(limits());
    reader.decode().ok().filter(within_pixel_budget)
}

/// Decodes any supported photo kind: standard formats directly, RAW via its embedded JPEG,
/// PSD via the merged composite (or its thumbnail).
pub fn decode_file(path: &Path, kind: MediaType) -> Option<DynamicImage> {
    match kind {
        MediaType::StandardImage => {
            let mut reader = ImageReader::open(path).ok()?.with_guessed_format().ok()?;
            reader.limits(limits());
            reader.decode().ok().filter(within_pixel_budget)
        }
        #[cfg(feature = "raw-preview")]
        MediaType::RawImage => decode_bytes(&crate::raw_preview::extract_raw_preview(path)?),
        #[cfg(feature = "psd-preview")]
        MediaType::PsdImage => crate::psd_preview::load_psd_image(path),
        _ => None,
    }
}

/// Cheap pre-check reading only the header: whether a standard image is likely decodable within
/// the limits. RAW/PSD are accepted as is (their previews are found only by a full parse).
pub fn header_looks_decodable(path: &Path, kind: MediaType) -> bool {
    match kind {
        MediaType::StandardImage => ImageReader::open(path)
            .ok()
            .and_then(|r| r.with_guessed_format().ok())
            .and_then(|r| r.into_dimensions().ok())
            .is_some_and(|(w, h)| w <= MAX_DIMENSION && h <= MAX_DIMENSION && (w as u64) * (h as u64) <= MAX_PIXELS),
        MediaType::RawImage | MediaType::PsdImage => path.is_file(),
        _ => false,
    }
}

/// Width and height of a standard image from its header, without decoding (EXIF rotation not
/// applied — the same sizes the analysis reports).
pub fn header_dimensions(path: &Path) -> Option<(u32, u32)> {
    ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_dimensions().ok()
}

/// Applies an EXIF orientation code (1..=8); other values leave the image untouched.
pub fn apply_exif_orientation(img: &mut DynamicImage, orientation: u16) {
    if let Some(o) = u8::try_from(orientation).ok().and_then(Orientation::from_exif) {
        img.apply_orientation(o);
    }
}
