//! Formats decoded by Windows Imaging Component: HEIC/HEIF and AVIF (the Store's HEIF and AV1
//! extensions), JPEG XL (its extension), TIFF, JPEG XR and DDS (built into Windows). Nothing of the
//! codecs lands in our binary; a missing extension simply makes decoding fail.
//!
//! WIC applies the container's own transforms (HEIF `irot` / `imir`), so the pixels come out
//! upright and no EXIF orientation must be applied on top.

use std::path::Path;

use image::{DynamicImage, RgbaImage};
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapDecoder,
    IWICBitmapFrameDecode, IWICBitmapSource, IWICImagingFactory, WICBitmapDitherTypeNone,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use crate::image_decode::{MAX_DIMENSION, MAX_PIXELS};
use crate::mf_init::ComScope;

/// Whether the file is of a kind only WIC decodes (the `image` crate has no decoder for it).
pub fn handles(path: &Path) -> bool {
    crate::probe::has_extension(path, crate::probe::WIC_IMAGE_EXTS)
}

/// The first frame of `path` with the factory and decoder that opened it. COM must be
/// initialized. The decoder has to outlive the frame: the built-in DDS decoder's frames do not
/// keep it alive and crash once it is gone.
struct Frame {
    frame: IWICBitmapFrameDecode,
    _decoder: IWICBitmapDecoder,
    factory: IWICImagingFactory,
}

fn first_frame(path: &Path) -> Option<Frame> {
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let name = HSTRING::from(path.as_os_str());
        let decoder = factory
            .CreateDecoderFromFilename(
                PCWSTR(name.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .ok()?;
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
    let frame = first_frame(path)?;
    frame_size(&frame.frame)
}

/// Decodes the first frame to RGBA under the shared size limits.
pub fn decode(path: &Path) -> Option<DynamicImage> {
    decode_fitted(path, None).map(|(img, _)| img)
}

/// Decodes the first frame to RGBA, fitted into `fit` (aspect kept, never enlarged) when given,
/// along with the frame's full size. Decoders that can (HEIF, JPEG) produce the smaller picture
/// directly, which is several times faster than decoding it whole. The size limits apply to the
/// full frame, so a picture is either shown at every size or not at all.
pub fn decode_fitted(path: &Path, fit: Option<(u32, u32)>) -> Option<(DynamicImage, (u32, u32))> {
    let _com = ComScope::new();
    let Frame { frame, factory, .. } = &first_frame(path)?;
    let (w, h) = frame_size(frame)?;
    if w > MAX_DIMENSION || h > MAX_DIMENSION || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let (tw, th) = fit.map_or((w, h), |(fw, fh)| {
        crate::image_decode::fitted_size((w, h), (fw, fh))
    });
    let stride = tw * 4;
    let mut rgba = vec![0u8; stride as usize * th as usize];
    unsafe {
        let source: IWICBitmapSource = if (tw, th) == (w, h) {
            frame.cast().ok()?
        } else {
            let scaler = factory.CreateBitmapScaler().ok()?;
            scaler
                .Initialize(frame, tw, th, WICBitmapInterpolationModeFant)
                .ok()?;
            scaler.cast().ok()?
        };
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
    let img = RgbaImage::from_raw(tw, th, rgba).map(DynamicImage::ImageRgba8)?;
    Some((img, (w, h)))
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
}
