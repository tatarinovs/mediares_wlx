//! Image decoding and rendering to BGRA DIB for GDI Double Buffering.

use std::path::Path;
use mediares_core::probe::MediaType;
use crate::wlx_state::DecodedImage;

pub fn load_image_for_display(path: &Path, media_type: MediaType) -> Option<DecodedImage> {
    let dyn_img = match media_type {
        MediaType::StandardImage => image::open(path).ok()?,
        MediaType::RawImage => {
            let bytes = mediares_core::raw_preview::extract_raw_preview(path)?;
            image::load_from_memory(&bytes).ok()?
        }
        MediaType::PsdImage => {
            mediares_core::psd_preview::load_psd_image(path)?
        }
        _ => return None,
    };

    let rgba = dyn_img.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    let raw = rgba.into_raw();

    // Dark sleek background color (matches Lister viewer background 0x181818)
    let bg_r = 0x18u32;
    let bg_g = 0x18u32;
    let bg_b = 0x18u32;

    // Convert RGBA to BGRA with software alpha pre-blending against background
    let mut bgra = vec![0u8; raw.len()];
    for (src, dst) in raw.chunks_exact(4).zip(bgra.chunks_exact_mut(4)) {
        let r = src[0] as u32;
        let g = src[1] as u32;
        let b = src[2] as u32;
        let a = src[3] as u32;

        if a == 255 {
            dst[0] = b as u8;
            dst[1] = g as u8;
            dst[2] = r as u8;
            dst[3] = 255;
        } else if a == 0 {
            dst[0] = bg_b as u8;
            dst[1] = bg_g as u8;
            dst[2] = bg_r as u8;
            dst[3] = 255;
        } else {
            let inv_a = 255 - a;
            dst[0] = ((b * a + bg_b * inv_a + 127) / 255) as u8;
            dst[1] = ((g * a + bg_g * inv_a + 127) / 255) as u8;
            dst[2] = ((r * a + bg_r * inv_a + 127) / 255) as u8;
            dst[3] = 255;
        }
    }

    Some(DecodedImage {
        width,
        height,
        bgra_pixels: bgra,
    })
}
