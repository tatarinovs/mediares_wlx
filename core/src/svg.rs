//! SVG / SVGZ rendered by Direct2D's own SVG engine (Windows 10 1703+), so no renderer lands in
//! our binary. Direct2D covers shapes, paths, gradients, clip paths, `use` and inline styles
//! (simple `<style>` sheets are inlined first, see `svg_css`); `<text>`, filters and masks are not
//! drawn.
//!
//! Vector art has no pixel size of its own, so small drawings (icons) are rendered larger to stay
//! sharp when shown fitted to the window.

use std::io::{Read, Seek};
use std::path::Path;

use image::{DynamicImage, RgbaImage};
use windows::core::{w, Interface};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_SIZE_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DeviceContext5, ID2D1Factory1, ID2D1SvgDocument, ID2D1SvgElement,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_SVG_ATTRIBUTE_POD_TYPE,
    D2D1_SVG_ATTRIBUTE_POD_TYPE_LENGTH, D2D1_SVG_ATTRIBUTE_POD_TYPE_VIEWBOX, D2D1_SVG_LENGTH,
    D2D1_SVG_LENGTH_UNITS_NUMBER, D2D1_SVG_VIEWBOX,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
    WICBitmapCacheOnLoad,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows_numerics::Matrix3x2;

use crate::mf_init::ComScope;

/// Drawings smaller than this (longer side, px) are scaled up to it.
const MIN_RENDER_SIDE: f32 = 1024.0;
/// Larger drawings are scaled down to this.
const MAX_RENDER_SIDE: f32 = 8192.0;
/// SVG files (after SVGZ unpacking) beyond this are not parsed: the XML tree costs far more
/// memory than the file.
const MAX_XML_BYTES: u64 = 64 * 1024 * 1024;
/// Direct2D's SVG engine crashes (access violation in d2d1.dll, about once in a hundred) when two
/// threads parse documents at once, even on separate single-threaded factories: TC's thumbnail
/// thread and the viewer do. One document at a time.
static D2D_SVG: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Size the SVG spec prescribes when neither `width`/`height` nor `viewBox` give one.
const DEFAULT_SIZE: (f32, f32) = (300.0, 150.0);

pub fn handles(path: &Path) -> bool {
    crate::probe::has_extension(path, crate::probe::SVG_EXTS)
}

/// The XML text; SVGZ (and gzipped `.svg`) is unpacked.
fn read_xml(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 2];
    file.read_exact(&mut head).ok()?;
    file.seek(std::io::SeekFrom::Start(0)).ok()?;
    let mut xml = Vec::new();
    let limit = MAX_XML_BYTES + 1;
    if head == [0x1F, 0x8B] {
        flate2::read::GzDecoder::new(file)
            .take(limit)
            .read_to_end(&mut xml)
            .ok()?;
    } else {
        file.take(limit).read_to_end(&mut xml).ok()?;
    }
    if xml.len() as u64 > MAX_XML_BYTES {
        return None;
    }
    Some(crate::svg_css::inline(&xml).unwrap_or(xml))
}

/// A parsed document on a device context drawing into a `w`×`h` WIC bitmap.
struct Canvas {
    dc: ID2D1DeviceContext5,
    bitmap: windows::Win32::Graphics::Imaging::IWICBitmap,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Option<Canvas> {
        unsafe {
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
            let bitmap = wic
                .CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)
                .ok()?;
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                ..Default::default()
            };
            let target = factory.CreateWicBitmapRenderTarget(&bitmap, &props).ok()?;
            let dc = target.cast::<ID2D1DeviceContext5>().ok()?;
            Some(Canvas { dc, bitmap })
        }
    }

    fn document(&self, xml: &[u8]) -> Option<ID2D1SvgDocument> {
        let stream = unsafe { SHCreateMemStream(Some(xml)) }?;
        let viewport = D2D_SIZE_F {
            width: DEFAULT_SIZE.0,
            height: DEFAULT_SIZE.1,
        };
        unsafe { self.dc.CreateSvgDocument(&stream, viewport) }.ok()
    }
}

fn pod<T: Default>(
    root: &ID2D1SvgElement,
    name: windows::core::PCWSTR,
    kind: D2D1_SVG_ATTRIBUTE_POD_TYPE,
) -> Option<T> {
    let mut value = T::default();
    unsafe {
        root.GetAttributeValue2(
            name,
            kind,
            (&mut value as *mut T).cast(),
            std::mem::size_of::<T>() as u32,
        )
    }
    .ok()?;
    Some(value)
}

/// The drawing's own size: absolute `width` / `height`, else the `viewBox` (the missing side
/// following its aspect ratio), else the SVG default.
fn intrinsic_size(doc: &ID2D1SvgDocument) -> (f32, f32) {
    let Ok(root) = (unsafe { doc.GetRoot() }) else {
        return DEFAULT_SIZE;
    };
    let length = |name| {
        pod::<D2D1_SVG_LENGTH>(&root, name, D2D1_SVG_ATTRIBUTE_POD_TYPE_LENGTH)
            .filter(|l| l.units == D2D1_SVG_LENGTH_UNITS_NUMBER && l.value > 0.0)
            .map(|l| l.value)
    };
    let view = pod::<D2D1_SVG_VIEWBOX>(&root, w!("viewBox"), D2D1_SVG_ATTRIBUTE_POD_TYPE_VIEWBOX)
        .filter(|v| v.width > 0.0 && v.height > 0.0);
    match (length(w!("width")), length(w!("height")), view) {
        (Some(w), Some(h), _) => (w, h),
        (Some(w), None, Some(v)) => (w, w * v.height / v.width),
        (None, Some(h), Some(v)) => (h * v.width / v.height, h),
        (None, None, Some(v)) => (v.width, v.height),
        (w, h, None) => (w.unwrap_or(DEFAULT_SIZE.0), h.unwrap_or(DEFAULT_SIZE.1)),
    }
}

/// Pixel size the drawing is rendered at and the scale from its own units.
fn render_size((w, h): (f32, f32)) -> Option<(u32, u32, f32)> {
    let longer = w.max(h);
    if !(longer > 0.0 && longer.is_finite()) {
        return None;
    }
    let scale = longer.clamp(MIN_RENDER_SIDE, MAX_RENDER_SIDE) / longer;
    let px = |v: f32| ((v * scale).round() as u32).max(1);
    Some((px(w), px(h), scale))
}

/// Parses the document on a 1×1 canvas: enough to read its size.
fn parsed_size(xml: &[u8]) -> Option<(f32, f32)> {
    let canvas = Canvas::new(1, 1)?;
    Some(intrinsic_size(&canvas.document(xml)?))
}

/// The drawing's own size, rounded.
pub fn dimensions(path: &Path) -> Option<(u32, u32)> {
    let _com = ComScope::new();
    let xml = read_xml(path)?;
    let _d2d = D2D_SVG.lock().unwrap_or_else(|e| e.into_inner());
    let (w, h) = parsed_size(&xml)?;
    Some(((w.round() as u32).max(1), (h.round() as u32).max(1)))
}

pub fn decode(path: &Path) -> Option<DynamicImage> {
    let _com = ComScope::new();
    let xml = read_xml(path)?;
    let _d2d = D2D_SVG.lock().unwrap_or_else(|e| e.into_inner());
    let size = parsed_size(&xml)?;
    let (w, h, scale) = render_size(size)?;
    let canvas = Canvas::new(w, h)?;
    let doc = canvas.document(&xml)?;
    unsafe {
        doc.SetViewportSize(D2D_SIZE_F {
            width: size.0,
            height: size.1,
        })
        .ok()?;
        let dc = &canvas.dc;
        dc.BeginDraw();
        dc.Clear(Some(&D2D1_COLOR_F::default()));
        dc.SetTransform(&Matrix3x2::scale(scale, scale));
        dc.DrawSvgDocument(&doc);
        dc.EndDraw(None, None).ok()?;
    }
    let stride = w * 4;
    let mut pixels = vec![0u8; stride as usize * h as usize];
    unsafe {
        canvas
            .bitmap
            .CopyPixels(std::ptr::null(), stride, &mut pixels)
    }
    .ok()?;
    // Premultiplied BGRA → straight RGBA.
    for px in pixels.chunks_exact_mut(4) {
        let a = px[3];
        px.swap(0, 2);
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u16::from(*c) * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8;
            }
        }
    }
    RgbaImage::from_raw(w, h, pixels).map(DynamicImage::ImageRgba8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_scaled_up_and_posters_down() {
        assert_eq!(render_size((16.0, 8.0)), Some((1024, 512, 64.0)));
        assert_eq!(
            render_size((20_000.0, 10_000.0)).map(|s| (s.0, s.1)),
            Some((8192, 4096))
        );
        assert_eq!(render_size((0.0, 0.0)), None);
    }

    fn temp_svg(name: &str, xml: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("mediares_{}_{name}", std::process::id()));
        std::fs::write(&path, xml).unwrap();
        path
    }

    #[test]
    fn renders_shapes_at_scale() {
        let path = temp_svg(
            "t.svg",
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="16" viewBox="0 0 2 1">
                 <rect x="0" y="0" width="1" height="1" fill="#ff0000"/>
               </svg>"##,
        );
        assert_eq!(dimensions(&path), Some((32, 16)));
        let img = decode(&path).unwrap().into_rgba8();
        std::fs::remove_file(&path).ok();
        assert_eq!(img.dimensions(), (1024, 512));
        assert_eq!(img.get_pixel(100, 100).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(900, 100).0[3], 0);
    }

    #[test]
    fn size_from_viewbox_only() {
        let path = temp_svg(
            "v.svg",
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 100"/>"#,
        );
        assert_eq!(dimensions(&path), Some((200, 100)));
        std::fs::remove_file(&path).ok();
    }
}
