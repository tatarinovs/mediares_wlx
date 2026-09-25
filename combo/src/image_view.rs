//! Image decoding and rendering to BGRA DIB for GDI Double Buffering.

use std::path::Path;
use mediares_core::probe::MediaType;
use crate::wlx_state::DecodedImage;

pub fn load_image_for_display(path: &Path, media_type: MediaType) -> Option<DecodedImage> {
    match media_type {
        MediaType::StandardImage => {
            let img = image::open(path).ok()?;
            let rgba = img.to_rgba8();
            let (width, height) = (rgba.width(), rgba.height());
            let mut bgra = rgba.into_raw();
            // Convert RGBA to BGRA in-place for fast GDI StretchDIBits
            for chunk in bgra.chunks_exact_mut(4) {
                chunk.swap(0, 2);
            }
            Some(DecodedImage {
                width,
                height,
                bgra_pixels: bgra,
            })
        }
        MediaType::RawImage => {
            let bytes = mediares_core::raw_preview::extract_raw_preview(path)?;
            let img = image::load_from_memory(&bytes).ok()?;
            let rgba = img.to_rgba8();
            let (width, height) = (rgba.width(), rgba.height());
            let mut bgra = rgba.into_raw();
            for chunk in bgra.chunks_exact_mut(4) {
                chunk.swap(0, 2);
            }
            Some(DecodedImage {
                width,
                height,
                bgra_pixels: bgra,
            })
        }
        MediaType::PsdImage => {
            let bytes = mediares_core::psd_preview::extract_psd_preview(path)?;
            let img = image::load_from_memory(&bytes).ok()?;
            let rgba = img.to_rgba8();
            let (width, height) = (rgba.width(), rgba.height());
            let mut bgra = rgba.into_raw();
            for chunk in bgra.chunks_exact_mut(4) {
                chunk.swap(0, 2);
            }
            Some(DecodedImage {
                width,
                height,
                bgra_pixels: bgra,
            })
        }
        _ => None,
    }
}
