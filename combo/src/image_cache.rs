//! Display-ready (BGRA) image cache with a single background prefetch worker.
//!
//! Decoding itself lives in `mediares_core::image_decode`; this module only adapts the result
//! for GDI and keeps recently used / neighbouring images around for instant navigation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use mediares_core::cache::FileKey;
use mediares_core::exif::read_orientation;
use mediares_core::image::DynamicImage;
use mediares_core::image_decode::{apply_exif_orientation, decode_bytes, decode_file};
use mediares_core::probe::{probe_file, MediaType};

/// Upper bound for decoded pixels kept in memory (the image on screen is always kept).
const CACHE_BUDGET_BYTES: usize = 384 * 1024 * 1024;

/// Viewer background, used to flatten transparency (GDI ignores alpha): 0x181818.
pub const BACKGROUND_GRAY: u8 = 0x18;

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Top-down 32bpp BGRA rows, alpha already composited over the background.
    pub bgra: Vec<u8>,
    /// True when the picture is an embedded preview rather than the full image (RAW files).
    pub is_preview: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    file: FileKey,
    auto_rotate: bool,
}

struct Cache {
    /// Least recently used first.
    entries: Vec<(Key, Arc<DecodedImage>)>,
}

impl Cache {
    fn get(&mut self, key: &Key) -> Option<Arc<DecodedImage>> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(pos);
        let img = entry.1.clone();
        self.entries.push(entry);
        Some(img)
    }

    fn contains(&self, key: &Key) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    fn insert(&mut self, key: Key, img: Arc<DecodedImage>) {
        self.entries.retain(|(k, _)| *k != key);
        self.entries.push((key, img));
        let mut total: usize = self.entries.iter().map(|(_, i)| i.bgra.len()).sum();
        while total > CACHE_BUDGET_BYTES && self.entries.len() > 1 {
            total -= self.entries.remove(0).1.bgra.len();
        }
    }
}

fn cache() -> MutexGuard<'static, Cache> {
    static CACHE: Mutex<Cache> = Mutex::new(Cache { entries: Vec::new() });
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Returns the image for display, decoding it on the calling thread on a cache miss.
pub fn load(path: &Path, kind: MediaType, auto_rotate: bool) -> Option<Arc<DecodedImage>> {
    let key = Key { file: FileKey::for_path(path)?, auto_rotate };
    if let Some(img) = cache().get(&key) {
        return Some(img);
    }
    let img = Arc::new(decode(path, kind, auto_rotate)?);
    cache().insert(key, img.clone());
    Some(img)
}

/// Replaces the prefetch queue with `paths`; stale requests from earlier navigation are dropped.
pub fn prefetch(paths: Vec<PathBuf>, auto_rotate: bool) {
    let worker = prefetcher();
    *worker.queue.lock().unwrap_or_else(|e| e.into_inner()) = (paths, auto_rotate);
    worker.wake.notify_one();
}

struct Prefetcher {
    queue: Mutex<(Vec<PathBuf>, bool)>,
    wake: Condvar,
}

fn prefetcher() -> &'static Prefetcher {
    static WORKER: OnceLock<Prefetcher> = OnceLock::new();
    WORKER.get_or_init(|| {
        std::thread::Builder::new()
            .name("mediares-prefetch".into())
            .spawn(prefetch_loop)
            .ok();
        Prefetcher { queue: Mutex::new((Vec::new(), true)), wake: Condvar::new() }
    })
}

fn prefetch_loop() {
    let worker = prefetcher();
    loop {
        let (path, auto_rotate) = {
            let mut q = worker.queue.lock().unwrap_or_else(|e| e.into_inner());
            while q.0.is_empty() {
                q = worker.wake.wait(q).unwrap_or_else(|e| e.into_inner());
            }
            (q.0.remove(0), q.1)
        };
        let Some(file) = FileKey::for_path(&path) else { continue };
        let key = Key { file, auto_rotate };
        if cache().contains(&key) {
            continue;
        }
        let decoded = std::panic::catch_unwind(|| decode(&path, probe_file(&path), auto_rotate));
        if let Ok(Some(img)) = decoded {
            cache().insert(key, Arc::new(img));
        }
    }
}

fn decode(path: &Path, kind: MediaType, auto_rotate: bool) -> Option<DecodedImage> {
    let mut img = decode_file(path, kind)?;
    if auto_rotate {
        if let Some(orientation) = read_orientation(path) {
            apply_exif_orientation(&mut img, orientation);
        }
    }
    Some(to_display(img, kind == MediaType::RawImage))
}

/// Decodes an in-memory picture (e.g. embedded album art) for display; not cached.
pub fn decode_picture(bytes: &[u8]) -> Option<DecodedImage> {
    Some(to_display(decode_bytes(bytes)?, false))
}

/// Converts to BGRA in place, compositing alpha over the viewer background.
fn to_display(img: DynamicImage, is_preview: bool) -> DecodedImage {
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    let mut bgra = rgba.into_raw();
    let bg = BACKGROUND_GRAY as u32;
    for px in bgra.chunks_exact_mut(4) {
        let a = px[3] as u32;
        let blend = |c: u8| ((c as u32 * a + bg * (255 - a) + 127) / 255) as u8;
        let (r, g, b) = if a == 255 { (px[0], px[1], px[2]) } else { (blend(px[0]), blend(px[1]), blend(px[2])) };
        px.copy_from_slice(&[b, g, r, 255]);
    }
    DecodedImage { width, height, bgra, is_preview }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mediares_core::image::{Rgba, RgbaImage};

    #[test]
    fn converts_to_bgra_over_background() {
        let img = RgbaImage::from_fn(2, 1, |x, _| if x == 0 { Rgba([255, 0, 10, 255]) } else { Rgba([255, 255, 255, 0]) });
        let out = to_display(DynamicImage::ImageRgba8(img), false);
        assert_eq!(&out.bgra[0..4], &[10, 0, 255, 255]);
        assert_eq!(&out.bgra[4..8], &[BACKGROUND_GRAY, BACKGROUND_GRAY, BACKGROUND_GRAY, 255]);
    }

    #[test]
    fn cache_evicts_least_recently_used_by_bytes() {
        let big = |n: usize| Arc::new(DecodedImage { width: 1, height: 1, bgra: vec![0; n], is_preview: false });
        let key = |name: &str| Key {
            file: FileKey::for_path(Path::new(env!("CARGO_MANIFEST_DIR")).join(name).as_path()).expect("file exists"),
            auto_rotate: true,
        };
        let (a, b, c) = (key("Cargo.toml"), key("src/lib.rs"), key("src/config.rs"));
        let mut cache = Cache { entries: Vec::new() };
        let third = CACHE_BUDGET_BYTES / 3 + 1;
        cache.insert(a.clone(), big(third));
        cache.insert(b.clone(), big(third));
        assert!(cache.get(&a).is_some()); // `a` becomes most recently used
        cache.insert(c.clone(), big(third));
        assert!(cache.contains(&a) && cache.contains(&c));
        assert!(!cache.contains(&b));
    }
}
