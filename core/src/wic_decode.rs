//! Formats decoded by Windows Imaging Component: HEIC/HEIF and AVIF (the Store's HEIF and AV1
//! extensions), JPEG XL (its extension), JPEG XR and DDS (built into Windows). Nothing of the
//! codecs lands in our binary; a missing extension simply makes decoding fail.
//!
//! WIC applies the container's own transforms (HEIF `irot` / `imir`), so the pixels come out
//! upright and no EXIF orientation must be applied on top.

use std::path::Path;

use image::{DynamicImage, RgbaImage};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapDecoder,
    IWICBitmapFrameDecode, IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
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
    let _com = ComScope::new();
    let Frame { frame, factory, .. } = &first_frame(path)?;
    let (w, h) = frame_size(frame)?;
    if w > MAX_DIMENSION || h > MAX_DIMENSION || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let stride = w * 4;
    let mut rgba = vec![0u8; stride as usize * h as usize];
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
        converter
            .CopyPixels(std::ptr::null(), stride, &mut rgba)
            .ok()?;
    }
    RgbaImage::from_raw(w, h, rgba).map(DynamicImage::ImageRgba8)
}
