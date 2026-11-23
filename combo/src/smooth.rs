//! Bicubic enlarging through Direct2D, drawn straight into the GDI paint buffer: GDI itself only
//! replicates pixels when stretching up.

use std::cell::RefCell;

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap, ID2D1DCRenderTarget, ID2D1DeviceContext, ID2D1Factory,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::HDC;

use crate::image_cache::DecodedImage;

/// Pictures are opaque BGRA (transparency is already flattened onto the background).
const FORMAT: D2D1_PIXEL_FORMAT = D2D1_PIXEL_FORMAT {
    format: DXGI_FORMAT_B8G8R8A8_UNORM,
    alphaMode: D2D1_ALPHA_MODE_IGNORE,
};
/// 96 DPI: one Direct2D unit is one pixel of the buffer.
const DPI: f32 = 96.0;
const MAX_CACHED_SIDE: u32 = 4096;

thread_local! {
    /// Painting happens on the window's thread; the target is reused between paints.
    static TARGET: RefCell<Target> = const { RefCell::new(Target(None)) };
}

struct CachedBitmap {
    image_id: u64,
    bitmap: ID2D1Bitmap,
}

struct CachedTarget {
    bitmap: Option<CachedBitmap>,
    context: Option<ID2D1DeviceContext>,
    rt: ID2D1DCRenderTarget,
}

struct Target(Option<CachedTarget>);

impl Drop for Target {
    /// Thread-local destructors run when TC unloads the DLL, under the loader lock: releasing
    /// Direct2D there calls into the graphics driver, which waits for its own threads, which wait
    /// for the loader lock — TC hangs on exit. [`release`] frees the target before that; whatever
    /// is left at unload is leaked.
    fn drop(&mut self) {
        if let Some(cached) = self.0.take() {
            std::mem::forget(cached.bitmap);
            std::mem::forget(cached.context);
            std::mem::forget(cached.rt);
        }
    }
}

/// Frees this thread's render target; called when a viewer window closes.
pub fn release() {
    let _ = TARGET.try_with(|target| target.borrow_mut().0 = None);
}

fn create_target() -> Option<CachedTarget> {
    unsafe {
        let factory: ID2D1Factory =
            D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
        let rt = factory
            .CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: FORMAT,
                dpiX: DPI,
                dpiY: DPI,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            })
            .ok()?;
        let context = rt.cast::<ID2D1DeviceContext>().ok();
        Some(CachedTarget {
            bitmap: None,
            context,
            rt,
        })
    }
}

/// Draws source columns / rows `(start, len)` of `img` onto destination `(start, len)` spans of
/// `dc`, clipped to `bounds`. Returns false if Direct2D failed; the caller then draws with GDI.
pub unsafe fn draw(
    dc: HDC,
    img: &DecodedImage,
    (sx, sw, dx, dw): (i32, i32, i32, i32),
    (sy, sh, dy, dh): (i32, i32, i32, i32),
    bounds: RECT,
) -> bool {
    // Bind only the part the picture covers: Direct2D writes back the whole bound rectangle.
    let bound = RECT {
        left: dx.max(bounds.left),
        top: dy.max(bounds.top),
        right: (dx + dw).min(bounds.right),
        bottom: (dy + dh).min(bounds.bottom),
    };
    if bound.right <= bound.left || bound.bottom <= bound.top || sw <= 0 || sh <= 0 {
        return true;
    }
    let ok = TARGET.with_borrow_mut(|Target(target)| {
        if target.is_none() {
            *target = create_target();
        }
        let Some(cached) = target.as_mut() else {
            return false;
        };
        let ok = render(cached, dc, img, (sx, sy, sw, sh), bound, (dx, dy, dw, dh));
        if !ok {
            // A lost device (or anything else): start afresh next time.
            *target = None;
        }
        ok
    });
    ok
}

unsafe fn render(
    cached: &mut CachedTarget,
    dc: HDC,
    img: &DecodedImage,
    (sx, sy, sw, sh): (i32, i32, i32, i32),
    bound: RECT,
    (dx, dy, dw, dh): (i32, i32, i32, i32),
) -> bool {
    if cached.rt.BindDC(dc, &bound).is_err() {
        return false;
    }
    let pitch = img.width as usize * 4;
    let start = sy as usize * pitch + sx as usize * 4;
    let needed = start + (sh as usize - 1) * pitch + sw as usize * 4;
    if needed > img.bgra.len() {
        return false;
    }
    let props = D2D1_BITMAP_PROPERTIES {
        pixelFormat: FORMAT,
        dpiX: DPI,
        dpiY: DPI,
    };
    let can_cache = img.width <= MAX_CACHED_SIDE
        && img.height <= MAX_CACHED_SIDE
        && img.bgra.len() >= img.height as usize * pitch;
    cached.rt.BeginDraw();
    let temp;
    let (bitmap, src_rect): (Option<&ID2D1Bitmap>, Option<D2D_RECT_F>) = if can_cache {
        let hit = cached.bitmap.as_ref().is_some_and(|b| b.image_id == img.id);
        if !hit {
            cached.bitmap = cached
                .rt
                .CreateBitmap(
                    D2D_SIZE_U {
                        width: img.width,
                        height: img.height,
                    },
                    Some(img.bgra.as_ptr() as *const _),
                    pitch as u32,
                    &props,
                )
                .ok()
                .map(|bitmap| CachedBitmap {
                    image_id: img.id,
                    bitmap,
                });
        }
        let src = D2D_RECT_F {
            left: sx as f32,
            top: sy as f32,
            right: (sx + sw) as f32,
            bottom: (sy + sh) as f32,
        };
        (cached.bitmap.as_ref().map(|b| &b.bitmap), Some(src))
    } else {
        temp = cached
            .rt
            .CreateBitmap(
                D2D_SIZE_U {
                    width: sw as u32,
                    height: sh as u32,
                },
                Some(img.bgra[start..].as_ptr() as *const _),
                pitch as u32,
                &props,
            )
            .ok();
        (temp.as_ref(), None)
    };
    let had_bitmap = bitmap.is_some();
    if let Some(bitmap) = bitmap {
        // Relative to the bound rectangle, which is the target's origin.
        let dest = D2D_RECT_F {
            left: (dx - bound.left) as f32,
            top: (dy - bound.top) as f32,
            right: (dx + dw - bound.left) as f32,
            bottom: (dy + dh - bound.top) as f32,
        };
        let src = src_rect.as_ref().map(|r| r as *const D2D_RECT_F);
        match cached.context.as_ref() {
            Some(context) => context.DrawBitmap(
                bitmap,
                Some(&dest),
                1.0,
                D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                src,
                None,
            ),
            // Windows 7 without the platform update: bilinear.
            None => cached.rt.DrawBitmap(
                bitmap,
                Some(&dest),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                src,
            ),
        }
    }
    cached.rt.EndDraw(None, None).is_ok() && had_bitmap
}
