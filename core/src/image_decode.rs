//! Photo decoding shared by WDX hashing and the WLX viewer: the `image` crate for standard
//! formats, WIC for HEIC/AVIF/JXL/JXR/DDS (and whatever `image` fails on), Direct2D for SVG, plus
//! embedded previews for RAW/PSD. All paths go through the same memory limits.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use image::metadata::Orientation;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};

use crate::probe::MediaType;

/// Largest decoded image we accept (pixels). Bounds memory inside the host process.
pub const MAX_PIXELS: u64 = 64_000_000;
pub const MAX_DIMENSION: u32 = 16_384;
const MAX_DECODER_ALLOC: u64 = 512 * 1024 * 1024;

/// Files and in-memory pictures (RAW/PSD previews, album art) behind one reader type, so `image`
/// compiles each of its decoders once instead of once per source type.
enum Source<'a> {
    File(BufReader<File>),
    Memory(Cursor<&'a [u8]>),
}

impl Read for Source<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Source::File(r) => r.read(buf),
            Source::Memory(r) => r.read(buf),
        }
    }
}

impl BufRead for Source<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        match self {
            Source::File(r) => r.fill_buf(),
            Source::Memory(r) => r.fill_buf(),
        }
    }

    fn consume(&mut self, amount: usize) {
        match self {
            Source::File(r) => r.consume(amount),
            Source::Memory(r) => r.consume(amount),
        }
    }
}

impl Seek for Source<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            Source::File(r) => r.seek(pos),
            Source::Memory(r) => r.seek(pos),
        }
    }
}

/// A reader with the format detected from the content (the extension as a fallback) and the
/// shared limits applied.
fn reader<'a>(source: Source<'a>, path: Option<&Path>) -> Option<ImageReader<Source<'a>>> {
    let mut reader = ImageReader::new(source);
    if let Some(format) = path.and_then(|p| ImageFormat::from_path(p).ok()) {
        reader.set_format(format);
    }
    let mut reader = reader.with_guessed_format().ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODER_ALLOC);
    reader.limits(limits);
    Some(reader)
}

fn open(path: &Path) -> Option<ImageReader<Source<'static>>> {
    let file = BufReader::new(File::open(path).ok()?);
    reader(Source::File(file), Some(path))
}

fn decode(reader: ImageReader<Source<'_>>) -> Option<DynamicImage> {
    reader
        .decode()
        .ok()
        .filter(|img| (img.width() as u64) * (img.height() as u64) <= MAX_PIXELS)
        .map(to_display_range)
}

/// Floating-point pictures (Radiance HDR, OpenEXR) hold linear light; everything downstream
/// expects 8-bit sRGB, so encode them with the sRGB curve (values above white clip).
fn to_display_range(img: DynamicImage) -> DynamicImage {
    if !matches!(
        img,
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
    ) {
        return img;
    }
    let mut rgba = img.into_rgba32f();
    for px in rgba.pixels_mut() {
        for c in &mut px.0[..3] {
            *c = linear_to_srgb(*c);
        }
    }
    DynamicImage::ImageRgba32F(rgba).into_rgba8().into()
}

fn linear_to_srgb(v: f32) -> f32 {
    let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// A standard image by whichever decoder owns its format; files the `image` crate cannot read
/// (JPEG-in-TIFF, unusual BMP variants, ...) get a second chance through WIC.
fn decode_standard(path: &Path) -> Option<DynamicImage> {
    if crate::svg::handles(path) {
        return crate::svg::decode(path);
    }
    if crate::wic_decode::handles(path) {
        return crate::wic_decode::decode(path);
    }
    open(path)
        .and_then(decode)
        .or_else(|| crate::wic_decode::decode(path))
}

/// Decodes an in-memory image (format detected from content) under the shared limits.
pub fn decode_bytes(bytes: &[u8]) -> Option<DynamicImage> {
    decode(reader(Source::Memory(Cursor::new(bytes)), None)?)
}

/// Decodes any supported photo kind: standard formats directly, RAW via its embedded JPEG,
/// PSD via the merged composite (or its thumbnail).
pub fn decode_file(path: &Path, kind: MediaType) -> Option<DynamicImage> {
    match kind {
        MediaType::StandardImage => decode_standard(path),
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
        MediaType::StandardImage if crate::svg::handles(path) => header_dimensions(path).is_some(),
        MediaType::StandardImage => header_dimensions(path).is_some_and(|(w, h)| {
            w <= MAX_DIMENSION && h <= MAX_DIMENSION && (w as u64) * (h as u64) <= MAX_PIXELS
        }),
        MediaType::RawImage | MediaType::PsdImage => path.is_file(),
        _ => false,
    }
}

/// Whether the header of this standard image is read through Direct2D / WIC rather than parsed
/// directly: slower, so the WDX defers it like RAW.
pub fn header_needs_codec(path: &Path) -> bool {
    crate::svg::handles(path) || crate::wic_decode::handles(path)
}

/// Width and height of a standard image from its header, without decoding (EXIF rotation not
/// applied — the same sizes the analysis reports).
pub fn header_dimensions(path: &Path) -> Option<(u32, u32)> {
    if crate::svg::handles(path) {
        return crate::svg::dimensions(path);
    }
    if crate::wic_decode::handles(path) {
        return crate::wic_decode::dimensions(path);
    }
    let mut file = BufReader::new(File::open(path).ok()?);
    if let Some(size) = crate::jpeg::read_dimensions(&mut file) {
        return Some(size);
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    reader(Source::File(file), Some(path))?
        .into_dimensions()
        .ok()
        .or_else(|| crate::wic_decode::dimensions(path))
}

/// Applies an EXIF orientation code (1..=8); other values leave the image untouched.
pub fn apply_exif_orientation(img: &mut DynamicImage, orientation: u16) {
    if let Some(o) = u8::try_from(orientation)
        .ok()
        .and_then(Orientation::from_exif)
    {
        img.apply_orientation(o);
    }
}

/// Decodes a photo turned upright by its EXIF orientation (when `auto_rotate`); the EXIF block is
/// returned along with it, since reading the orientation needs it anyway.
pub fn decode_oriented(
    path: &Path,
    kind: MediaType,
    auto_rotate: bool,
) -> Option<(DynamicImage, Option<crate::exif::ExifInfo>)> {
    let mut img = decode_file(path, kind)?;
    let exif = crate::exif::read_exif(path);
    if let Some(orientation) = exif
        .as_ref()
        .and_then(|e| e.orientation)
        .filter(|_| auto_rotate)
    {
        apply_exif_orientation(&mut img, orientation);
    }
    Some((img, exif))
}
