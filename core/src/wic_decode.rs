//! Formats decoded by Windows Imaging Component: HEIC/HEIF and AVIF (the Store's HEIF and AV1
//! extensions), JPEG XL (its extension), TIFF, JPEG XR and DDS (built into Windows). Nothing of the
//! codecs lands in our binary; a missing extension simply makes decoding fail. JPEG goes through
//! WIC too when a smaller copy is wanted: its decoder scales while decoding.
//!
//! WIC applies the container's own transforms (HEIF `irot` / `imir`), so the pixels come out
//! upright and no EXIF orientation must be applied on top.

use std::path::Path;

use image::{DynamicImage, RgbaImage};
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapDecoder,
    IWICBitmapFrameDecode, IWICBitmapSource, IWICImagingFactory, IWICPixelFormatInfo2,
    WICBitmapCacheOnLoad, WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICBitmapTransformFlipHorizontal, WICBitmapTransformFlipVertical,
    WICBitmapTransformOptions, WICBitmapTransformRotate0, WICBitmapTransformRotate180,
    WICBitmapTransformRotate270, WICBitmapTransformRotate90, WICDecodeMetadataCacheOnDemand,
    WICRect,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use crate::image_decode::{MAX_DIMENSION, MAX_PIXELS};
use crate::mf_init::ComScope;

/// Whether the file is of a kind only WIC decodes (the `image` crate has no decoder for it).
pub fn handles(path: &Path) -> bool {
    crate::probe::has_extension(path, crate::probe::WIC_IMAGE_EXTS)
}

/// What a picture is decoded from.
#[derive(Clone, Copy)]
pub enum Input<'a> {
    File(&'a Path),
    /// An encoded picture in memory (a RAW file's embedded JPEG preview, ...).
    Memory(&'a [u8]),
}

/// The first frame of the input with the factory and decoder that opened it. COM must be
/// initialized. The decoder has to outlive the frame: the built-in DDS decoder's frames do not
/// keep it alive and crash once it is gone.
struct Frame {
    frame: IWICBitmapFrameDecode,
    _decoder: IWICBitmapDecoder,
    factory: IWICImagingFactory,
}

fn first_frame(input: Input<'_>) -> Option<Frame> {
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let decoder = match input {
            Input::File(path) => {
                let name = HSTRING::from(path.as_os_str());
                factory
                    .CreateDecoderFromFilename(
                        PCWSTR(name.as_ptr()),
                        None,
                        GENERIC_READ,
                        WICDecodeMetadataCacheOnDemand,
                    )
                    .ok()?
            }
            Input::Memory(bytes) => {
                // The stream reads `bytes` in place: they are borrowed for the whole decode.
                let stream = factory.CreateStream().ok()?;
                stream.InitializeFromMemory(bytes).ok()?;
                factory
                    .CreateDecoderFromStream(
                        &stream,
                        std::ptr::null(),
                        WICDecodeMetadataCacheOnDemand,
                    )
                    .ok()?
            }
        };
        let frame = decoder.GetFrame(0).ok()?;
        Some(Frame {
            frame,
            _decoder: decoder,
            factory,
        })
    }
}

fn frame_size(frame: &IWICBitmapFrameDecode) -> Option<(u32, u32)> {
    let (mut w, mut h) = (0, 0);
    unsafe { frame.GetSize(&mut w, &mut h) }.ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// Width and height as shown (after the container's rotation), without decoding pixels.
pub fn dimensions(path: &Path) -> Option<(u32, u32)> {
    let _com = ComScope::new();
    // A local, not a temporary of the tail expression: those are dropped after `_com`, i.e.
    // released after `CoUninitialize`, which crashes the built-in DDS and JPEG XR codecs.
    let frame = first_frame(Input::File(path))?;
    frame_size(&frame.frame)
}

/// Bits per channel, channel count and transparency of the first frame's stored pixels.
pub fn pixel_format(path: &Path) -> Option<crate::image_decode::PixelFormat> {
    let _com = ComScope::new();
    // Locals, released before `_com` (see `dimensions`).
    let frame = first_frame(Input::File(path))?;
    let info = unsafe {
        let guid = frame.frame.GetPixelFormat().ok()?;
        frame
            .factory
            .CreateComponentInfo(&guid)
            .ok()?
            .cast::<IWICPixelFormatInfo2>()
            .ok()?
    };
    let (bits, channels, alpha) = unsafe {
        (
            info.GetBitsPerPixel().ok()?,
            info.GetChannelCount().ok()?,
            info.SupportsTransparency().ok()?.as_bool(),
        )
    };
    (channels > 0).then_some(crate::image_decode::PixelFormat {
        bits_per_channel: bits / channels,
        channels,
        alpha,
    })
}

/// Decodes the first frame to RGBA under the shared size limits.
pub fn decode(path: &Path) -> Option<DynamicImage> {
    decode_turned(Input::File(path), None, 1).map(|(img, _)| img)
}

/// Decodes the first frame to RGBA, fitted into `fit` (aspect kept, never enlarged) when given,
/// along with the frame's full size (before the turn). Decoders that can (HEIF, JPEG) produce the
/// smaller picture directly, which is several times faster than decoding it whole. The size
/// limits apply to the full frame, so a picture is either shown at every size or not at all.
/// The picture is turned by the EXIF `orientation` (1..=8) on the way: the turn is a step of the
/// decoding pipeline, so the picture is not copied once more to turn it.
pub fn decode_turned(
    input: Input<'_>,
    fit: Option<(u32, u32)>,
    orientation: u16,
) -> Option<(DynamicImage, (u32, u32))> {
    let _com = ComScope::new();
    let Frame { frame, factory, .. } = &first_frame(input)?;
    let (w, h) = frame_size(frame)?;
    if w > MAX_DIMENSION || h > MAX_DIMENSION || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let (tw, th) = fit.map_or((w, h), |(fw, fh)| {
        crate::image_decode::fitted_size((w, h), (fw, fh))
    });
    let transform = transform_for(orientation);
    let (ow, oh) = if (5..=8).contains(&orientation) {
        (th, tw)
    } else {
        (tw, th)
    };
    let stride = ow * 4;
    let mut rgba = vec![0u8; stride as usize * oh as usize];
    unsafe {
        let mut source: IWICBitmapSource = frame.cast().ok()?;
        if (tw, th) != (w, h) {
            let scaler = factory.CreateBitmapScaler().ok()?;
            scaler
                .Initialize(&source, tw, th, WICBitmapInterpolationModeFant)
                .ok()?;
            source = scaler.cast().ok()?;
        }
        if transform != WICBitmapTransformRotate0 {
            // Turned straight from the decoder, the rotator would read it a column at a time,
            // decoding (and scaling) the whole picture again for every column: 48 s instead
            // of 50 ms on a 33 MP RAW preview. The picture at its final size is held first.
            let held = factory
                .CreateBitmapFromSource(&source, WICBitmapCacheOnLoad)
                .ok()?;
            let rotator = factory.CreateBitmapFlipRotator().ok()?;
            rotator.Initialize(&held, transform).ok()?;
            source = rotator.cast().ok()?;
        }
        let converter = factory.CreateFormatConverter().ok()?;
        converter
            .Initialize(
                &source,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;
        converter
            .CopyPixels(std::ptr::null(), stride, &mut rgba)
            .ok()?;
    }
    let img = RgbaImage::from_raw(ow, oh, rgba).map(DynamicImage::ImageRgba8)?;
    Some((img, (w, h)))
}

/// Rows copied at a time by [`decode_whole_cancellable`]; the decoders that work row by row
/// (JPEG, TIFF, PNG) decode no further than asked.
const STRIP_ROWS: u32 = 256;

/// The whole first frame as RGBA, decoded a strip of rows at a time: when `cancelled` says so
/// between strips, the decode stops (`None`). The picture is not turned.
pub fn decode_whole_cancellable(
    input: Input<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Option<DynamicImage> {
    let _com = ComScope::new();
    let Frame { frame, factory, .. } = &first_frame(input)?;
    let (w, h) = frame_size(frame)?;
    if w > MAX_DIMENSION || h > MAX_DIMENSION || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let stride = w as usize * 4;
    let mut rgba = vec![0u8; stride * h as usize];
    unsafe {
        let converter = factory.CreateFormatConverter().ok()?;
        converter
            .Initialize(
                frame,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;
        for (i, strip) in rgba.chunks_mut(stride * STRIP_ROWS as usize).enumerate() {
            if cancelled() {
                return None;
            }
            let rect = WICRect {
                X: 0,
                Y: (i as u32 * STRIP_ROWS) as i32,
                Width: w as i32,
                Height: (strip.len() / stride) as i32,
            };
            converter.CopyPixels(&rect, stride as u32, strip).ok()?;
        }
    }
    RgbaImage::from_raw(w, h, rgba).map(DynamicImage::ImageRgba8)
}

/// The WIC turn that shows a picture with the EXIF `orientation` upright (WIC flips, then turns
/// clockwise; checked against the `image` crate by a test).
fn transform_for(orientation: u16) -> WICBitmapTransformOptions {
    let flipped = |o: WICBitmapTransformOptions| {
        WICBitmapTransformOptions(o.0 | WICBitmapTransformFlipHorizontal.0)
    };
    match orientation {
        2 => WICBitmapTransformFlipHorizontal,
        3 => WICBitmapTransformRotate180,
        4 => WICBitmapTransformFlipVertical,
        5 => flipped(WICBitmapTransformRotate270),
        6 => WICBitmapTransformRotate90,
        7 => flipped(WICBitmapTransformRotate90),
        8 => WICBitmapTransformRotate270,
        _ => WICBitmapTransformRotate0,
    }
}

#[cfg(test)]
mod tests {
    use crate::image_decode::{decode_file, header_dimensions, header_needs_codec};
    use crate::probe::{probe_file, MediaType};

    /// Uncompressed little-endian RGB TIFF, 2x1: red, green.
    fn rgb_tiff() -> Vec<u8> {
        let short = |tag: u16, v: u16| {
            [
                &tag.to_le_bytes()[..],
                &[3, 0, 1, 0, 0, 0],
                &v.to_le_bytes(),
                &[0, 0],
            ]
            .concat()
        };
        let long = |tag: u16, v: u32| {
            [
                &tag.to_le_bytes()[..],
                &[4, 0, 1, 0, 0, 0],
                &v.to_le_bytes(),
            ]
            .concat()
        };
        // Header, IFD (9 entries) at 8, BitsPerSample values at 122, pixels at 128.
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&9u16.to_le_bytes());
        t.extend(short(256, 2));
        t.extend(short(257, 1));
        t.extend(
            [
                &258u16.to_le_bytes()[..],
                &[3, 0, 3, 0, 0, 0],
                &122u32.to_le_bytes(),
            ]
            .concat(),
        );
        t.extend(short(259, 1));
        t.extend(short(262, 2));
        t.extend(long(273, 128));
        t.extend(short(277, 3));
        t.extend(short(278, 1));
        t.extend(long(279, 6));
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend([8u16, 8, 8].iter().flat_map(|v| v.to_le_bytes()));
        t.extend_from_slice(&[255, 0, 0, 0, 255, 0]);
        t
    }

    #[test]
    fn tiff_goes_through_wic() {
        let path = std::env::temp_dir().join(format!("mediares_{}_rgb.tif", std::process::id()));
        std::fs::write(&path, rgb_tiff()).unwrap();
        let (kind, codec, size) = (
            probe_file(&path),
            header_needs_codec(&path),
            header_dimensions(&path),
        );
        let img = decode_file(&path, MediaType::StandardImage).map(|i| i.into_rgba8());
        std::fs::remove_file(&path).ok();
        assert_eq!(
            (kind, codec, size),
            (MediaType::StandardImage, true, Some((2, 1)))
        );
        let img = img.expect("decoded");
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0, 255]);
    }

    /// A whole picture taller than one strip comes out complete, or not at all once cancelled.
    #[test]
    fn whole_picture_decodes_in_strips_and_stops_when_cancelled() {
        use image::codecs::png::PngEncoder;
        use image::{ExtendedColorType, ImageEncoder, Luma, Rgba, RgbaImage};
        let src = image::GrayImage::from_fn(5, 600, |x, y| Luma([((x * 7 + y) % 256) as u8]));
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(src.as_raw(), 5, 600, ExtendedColorType::L8)
            .unwrap();
        let input = super::Input::Memory(&png);
        let whole = super::decode_whole_cancellable(input, &|| false).expect("decoded");
        let expected = RgbaImage::from_fn(5, 600, |x, y| {
            let v = src.get_pixel(x, y).0[0];
            Rgba([v, v, v, 255])
        });
        assert_eq!(whole.to_rgba8(), expected);
        // Cancelled after the first strip.
        let strips = std::cell::Cell::new(0);
        let cancel = || {
            strips.set(strips.get() + 1);
            strips.get() > 1
        };
        assert!(super::decode_whole_cancellable(input, &cancel).is_none());
        assert_eq!(strips.get(), 2);
    }

    /// Every EXIF orientation turned by WIC matches the `image` crate's turn.
    #[test]
    fn wic_turns_match_exif_orientations() {
        use image::codecs::png::PngEncoder;
        use image::{DynamicImage, ExtendedColorType, ImageEncoder, Rgba, RgbaImage};
        // 3x2 with distinct pixels: any wrong turn or flip shows.
        let src = RgbaImage::from_fn(3, 2, |x, y| Rgba([(x * 80) as u8, (y * 200) as u8, 7, 255]));
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(src.as_raw(), 3, 2, ExtendedColorType::Rgba8)
            .unwrap();
        for orientation in 1..=8u16 {
            let (wic, full) = super::decode_turned(super::Input::Memory(&png), None, orientation)
                .expect("decoded");
            let mut expected = DynamicImage::ImageRgba8(src.clone());
            crate::image_decode::apply_exif_orientation(&mut expected, orientation);
            assert_eq!(full, (3, 2));
            assert_eq!(
                wic.to_rgba8(),
                expected.to_rgba8(),
                "orientation {orientation}"
            );
        }
    }
}
