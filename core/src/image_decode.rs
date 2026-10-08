//! Photo decoding shared by WDX hashing and the WLX viewer: the `image` crate for standard
//! formats, WIC for TIFF/HEIC/AVIF/JXL/JXR/DDS (and whatever `image` fails on), Direct2D for SVG, plus
//! embedded previews for RAW/PSD, and an optional fallback decoder (libmpv in the viewer) for what
//! WIC lacks the codec for. All paths go through the same memory limits.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;

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

/// Decodes a picture WIC couldn't (no Store extension for its format), turned upright by the
/// container's own rotation as WIC does.
pub type FallbackDecoder = fn(&Path) -> Option<DynamicImage>;

struct Fallback {
    extensions: &'static [&'static str],
    decode: FallbackDecoder,
}

static FALLBACK: OnceLock<Fallback> = OnceLock::new();

/// Registers the decoder of last resort for files with `extensions` (the viewer plugs libmpv in
/// here when it is installed). Only the first registration counts.
pub fn set_fallback_decoder(extensions: &'static [&'static str], decode: FallbackDecoder) {
    let _ = FALLBACK.set(Fallback { extensions, decode });
}

fn fallback_for(path: &Path) -> Option<&'static Fallback> {
    FALLBACK
        .get()
        .filter(|f| crate::probe::has_extension(path, f.extensions))
}

fn decode_fallback(path: &Path) -> Option<DynamicImage> {
    (fallback_for(path)?.decode)(path)
        .filter(|img| (img.width() as u64) * (img.height() as u64) <= MAX_PIXELS)
}

/// A standard image by whichever decoder owns its format; files the `image` crate cannot read
/// (unusual BMP variants, ...) get a second chance through WIC.
fn decode_standard(path: &Path) -> Option<DynamicImage> {
    if crate::svg::handles(path) {
        return crate::svg::decode(path);
    }
    if crate::wic_decode::handles(path) {
        return crate::wic_decode::decode(path).or_else(|| decode_fallback(path));
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

/// `size` scaled down to fit into `fit` (aspect kept); never enlarged.
pub fn fitted_size((w, h): (u32, u32), (fit_w, fit_h): (u32, u32)) -> (u32, u32) {
    let ratio = (fit_w as f64 / w.max(1) as f64).min(fit_h as f64 / h.max(1) as f64);
    if ratio >= 1.0 {
        return (w, h);
    }
    (
        ((w as f64 * ratio).round() as u32).max(1),
        ((h as f64 * ratio).round() as u32).max(1),
    )
}

/// Like [`decode_file`], but fitted into `fit` (never enlarged), along with the size of the whole
/// picture. JPEG, RAW previews and the WIC formats are reduced while decoding (HEIC about three
/// times faster at a quarter of the size); the rest are decoded whole and then shrunk.
pub fn decode_file_fitted(
    path: &Path,
    kind: MediaType,
    fit: (u32, u32),
) -> Option<(DynamicImage, (u32, u32))> {
    decode_turned_fitted(path, kind, fit, 1).map(|(img, full, _)| (img, full))
}

/// [`decode_file_fitted`] that also tries to turn the picture by the EXIF `orientation` while
/// decoding. Returns the picture, the size of the whole picture before any turn, and whether
/// the turn was made.
fn decode_turned_fitted(
    path: &Path,
    kind: MediaType,
    fit: (u32, u32),
    orientation: u16,
) -> Option<(DynamicImage, (u32, u32), bool)> {
    use crate::wic_decode::{decode_turned, Input};
    if by_wic(path, kind) {
        if let Some((img, full)) = decode_turned(Input::File(path), Some(fit), orientation) {
            return Some((img, full, true));
        }
    }
    #[cfg(feature = "raw-preview")]
    if kind == MediaType::RawImage {
        let preview = crate::raw_preview::extract_raw_preview_for(path, Some(fit.0.max(fit.1)))?;
        if let Some((img, full)) = decode_turned(Input::Memory(&preview), Some(fit), orientation) {
            return Some((img, full, true));
        }
        let img = decode_bytes(&preview)?;
        let full = (img.width(), img.height());
        return Some((shrunk(img, fit), full, false));
    }
    let img = decode_file(path, kind)?;
    let full = (img.width(), img.height());
    Some((shrunk(img, fit), full, false))
}

/// `img` fitted into `fit` (never enlarged).
fn shrunk(img: DynamicImage, fit: (u32, u32)) -> DynamicImage {
    let full = (img.width(), img.height());
    let (w, h) = fitted_size(full, fit);
    if (w, h) == full {
        img
    } else {
        thumbnail(&img, w, h)
    }
}

/// The whole picture for display: through WIC where it can stop part way (as fast as the `image`
/// crate on JPEG), else as [`decode_file`] does.
fn decode_whole(
    path: &Path,
    kind: MediaType,
    cancelled: &dyn Fn() -> bool,
) -> Option<DynamicImage> {
    use crate::wic_decode::{decode_whole_cancellable, Input};
    // A decode given up is not tried again another way.
    let tried = |img: Option<DynamicImage>| match img {
        Some(img) => Some(Some(img)),
        None if cancelled() => Some(None),
        None => None,
    };
    if by_wic(path, kind) {
        if let Some(result) = tried(decode_whole_cancellable(Input::File(path), cancelled)) {
            return result;
        }
    }
    #[cfg(feature = "raw-preview")]
    if kind == MediaType::RawImage {
        let preview = crate::raw_preview::extract_raw_preview(path)?;
        if let Some(result) = tried(decode_whole_cancellable(Input::Memory(&preview), cancelled)) {
            return result;
        }
        return decode_bytes(&preview);
    }
    decode_file(path, kind)
}

/// Decoded through WIC for display: its formats, and JPEG, whose decoder there scales while
/// decoding and can stop part way.
fn by_wic(path: &Path, kind: MediaType) -> bool {
    kind == MediaType::StandardImage
        && !crate::svg::handles(path)
        && (crate::wic_decode::handles(path) || is_jpeg(path))
}

/// The file starts like a JPEG stream, whatever its extension.
pub fn is_jpeg(path: &Path) -> bool {
    let mut magic = [0u8; 3];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok_and(|_| magic == [0xFF, 0xD8, 0xFF])
}

/// Cheap pre-check reading only the header: whether a standard image is likely decodable within
/// the limits. Formats whose header only a codec reads (SVG is parsed whole, under a lock the
/// decoder holds while rendering), RAW and PSD are accepted as is: the decode itself tells.
pub fn header_looks_decodable(path: &Path, kind: MediaType) -> bool {
    match kind {
        MediaType::StandardImage if header_needs_codec(path) => path.is_file(),
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

/// How a standard image stores its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    /// 8 for most pictures; 16 for 16-bit PNG/TIFF, 10-12 for HDR HEIC/AVIF, 32 for float.
    pub bits_per_channel: u32,
    pub channels: u32,
    /// Has an alpha channel (it may still be fully opaque).
    pub alpha: bool,
}

/// [`PixelFormat`] of a standard image from its header, without decoding. SVG has none.
pub fn header_pixel_format(path: &Path) -> Option<PixelFormat> {
    use image::ImageDecoder;
    if crate::svg::handles(path) {
        return None;
    }
    if crate::wic_decode::handles(path) {
        return crate::wic_decode::pixel_format(path);
    }
    let mut file = BufReader::new(File::open(path).ok()?);
    if let Some(sof) = crate::jpeg::read_frame_header(&mut file) {
        return Some(PixelFormat {
            bits_per_channel: sof.precision.into(),
            channels: sof.components.into(),
            alpha: false,
        });
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    let decoder = reader(Source::File(file), Some(path))?
        .into_decoder()
        .ok()?;
    let original = decoder.original_color_type();
    let channels = u32::from(original.channel_count());
    (channels > 0).then(|| PixelFormat {
        bits_per_channel: u32::from(original.bits_per_pixel()) / channels,
        channels,
        alpha: decoder.color_type().has_alpha(),
    })
}

/// `img` fitted into `max_w` x `max_h` (aspect kept) by averaging, like
/// [`DynamicImage::thumbnail`], but only for the buffer types the decoders produce: that method
/// compiles its filter for every pixel type (about 45 KB). Rarer types go through RGBA8.
pub fn thumbnail(img: &DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    use image::imageops;
    let (w, h) = (img.width().max(1) as f64, img.height().max(1) as f64);
    let ratio = (max_w as f64 / w).min(max_h as f64 / h);
    let (tw, th) = (
        ((w * ratio).round() as u32).max(1),
        ((h * ratio).round() as u32).max(1),
    );
    match img {
        DynamicImage::ImageRgb8(b) => imageops::thumbnail(b, tw, th).into(),
        DynamicImage::ImageRgba8(b) => imageops::thumbnail(b, tw, th).into(),
        DynamicImage::ImageLuma8(b) => imageops::thumbnail(b, tw, th).into(),
        other => imageops::thumbnail(&other.to_rgba8(), tw, th).into(),
    }
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
    decode_oriented_fitted(path, kind, auto_rotate, None).map(|(img, _, exif)| (img, exif))
}

/// [`decode_oriented`] fitted into `fit` when given (see [`decode_file_fitted`]); also returns
/// the size of the whole upright picture.
pub fn decode_oriented_fitted(
    path: &Path,
    kind: MediaType,
    auto_rotate: bool,
    fit: Option<(u32, u32)>,
) -> Option<(DynamicImage, (u32, u32), Option<crate::exif::ExifInfo>)> {
    decode_oriented_cancellable(path, kind, auto_rotate, fit, &|| false)
}

/// [`decode_oriented_fitted`]; a whole picture (`fit` = `None`) of JPEG, RAW and the WIC formats
/// is decoded a strip at a time and given up (`None`) as soon as `cancelled` says so.
pub fn decode_oriented_cancellable(
    path: &Path,
    kind: MediaType,
    auto_rotate: bool,
    fit: Option<(u32, u32)>,
    cancelled: &dyn Fn() -> bool,
) -> Option<(DynamicImage, (u32, u32), Option<crate::exif::ExifInfo>)> {
    // Read first: the decoders that can turn the picture on the way get the orientation.
    let exif = crate::exif::read_exif(path);
    let orientation = exif
        .as_ref()
        .and_then(|e| e.orientation)
        .filter(|o| auto_rotate && (1..=8).contains(o))
        .unwrap_or(1);
    let (mut img, (mut w, mut h), turned) = match fit {
        Some(fit) => decode_turned_fitted(path, kind, fit, orientation)?,
        None => decode_whole(path, kind, cancelled).map(|img| {
            let size = (img.width(), img.height());
            (img, size, false)
        })?,
    };
    if !turned {
        apply_exif_orientation(&mut img, orientation);
    }
    if (5..=8).contains(&orientation) {
        (w, h) = (h, w);
    }
    Some((img, (w, h), exif))
}
