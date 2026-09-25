//! Display-ready (BGRA) image cache with a small pool of background decoders.
//!
//! Decoding itself lives in `mediares_core::image_decode`; this module only adapts the result
//! for GDI, decodes off the UI thread and keeps recently used / neighbouring images around for
//! instant navigation.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use mediares_core::cache::FileKey;
use mediares_core::exif::read_orientation;
use mediares_core::image::DynamicImage;
use mediares_core::image_decode::{apply_exif_orientation, decode_bytes, decode_file};
use mediares_core::probe::{probe_file, MediaType};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Upper bound for decoded pixels kept in memory (the image on screen is always kept).
const CACHE_BUDGET_BYTES: usize = 512 * 1024 * 1024;
const MAX_WORKERS: usize = 3;

/// Posted to the requesting window when its [`Ticket`] has a result.
pub const WM_IMAGE_READY: u32 = WM_APP + 0x12;

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

pub enum Request {
    Ready(Arc<DecodedImage>),
    /// Decoding in the background; `notify` receives [`WM_IMAGE_READY`] when it is done.
    Pending(Ticket),
}

/// Returns a cached image right away, otherwise schedules it ahead of any prefetching.
/// `None` if the file is gone.
pub fn request(path: &Path, kind: MediaType, auto_rotate: bool, notify: HWND) -> Option<Request> {
    let key = Key { file: FileKey::for_path(path)?, auto_rotate };
    if let Some(img) = cache().get(&key) {
        return Some(Request::Ready(img));
    }
    let pool = pool();
    let mut q = pool.lock();
    // Re-checked under the queue lock: a worker caches its result before leaving `in_flight`.
    if let Some(img) = cache().get(&key) {
        return Some(Request::Ready(img));
    }
    let hwnd = notify.0 as isize;
    // A newer request from the same window supersedes one that has not started yet.
    q.foreground.retain(|job| {
        let mut notify = job.lock_notify();
        notify.retain(|&h| h != hwnd);
        !notify.is_empty()
    });
    let existing = q.in_flight.iter().chain(&q.foreground).find(|job| job.key == key).cloned();
    let job = existing.unwrap_or_else(|| {
        let job = Job::new(key, path.to_path_buf(), kind);
        q.foreground.push_back(job.clone());
        pool.wake.notify_one();
        job
    });
    job.lock_notify().push(hwnd);
    Some(Request::Pending(Ticket { job, hwnd }))
}

/// Replaces the prefetch queue with `paths` (most wanted first); stale requests from earlier
/// navigation are dropped.
pub fn prefetch(paths: Vec<PathBuf>, auto_rotate: bool) {
    let pool = pool();
    let mut q = pool.lock();
    q.prefetch = paths.into_iter().map(|p| (p, auto_rotate)).collect();
    pool.wake.notify_all();
}

/// A background decode the viewer is waiting for. Dropping it stops the notification.
pub struct Ticket {
    job: Arc<Job>,
    hwnd: isize,
}

impl Ticket {
    /// `None` while decoding; then the image, or `None` inside if it could not be decoded.
    pub fn result(&self) -> Option<Option<Arc<DecodedImage>>> {
        self.job.result.get().cloned()
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut notify = self.job.lock_notify();
        if let Some(pos) = notify.iter().position(|&h| h == self.hwnd) {
            notify.swap_remove(pos);
        }
    }
}

struct Job {
    key: Key,
    path: PathBuf,
    kind: MediaType,
    /// Windows to post [`WM_IMAGE_READY`] to (raw handles: HWND is not `Send`).
    notify: Mutex<Vec<isize>>,
    result: OnceLock<Option<Arc<DecodedImage>>>,
}

impl Job {
    fn new(key: Key, path: PathBuf, kind: MediaType) -> Arc<Job> {
        Arc::new(Job { key, path, kind, notify: Mutex::new(Vec::new()), result: OnceLock::new() })
    }

    fn lock_notify(&self) -> MutexGuard<'_, Vec<isize>> {
        self.notify.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Default)]
struct Queue {
    foreground: VecDeque<Arc<Job>>,
    prefetch: VecDeque<(PathBuf, bool)>,
    in_flight: Vec<Arc<Job>>,
}

impl Queue {
    /// The next job to run: requested images first, then prefetches not already done or running.
    fn next_job(&mut self) -> Option<Arc<Job>> {
        if let Some(job) = self.foreground.pop_front() {
            return Some(job);
        }
        while let Some((path, auto_rotate)) = self.prefetch.pop_front() {
            let Some(file) = FileKey::for_path(&path) else { continue };
            let key = Key { file, auto_rotate };
            if self.in_flight.iter().any(|job| job.key == key) || cache().contains(&key) {
                continue;
            }
            let kind = probe_file(&path);
            return Some(Job::new(key, path, kind));
        }
        None
    }
}

struct Pool {
    queue: Mutex<Queue>,
    wake: Condvar,
}

impl Pool {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        for i in 0..cores.saturating_sub(1).clamp(1, MAX_WORKERS) {
            let _ = std::thread::Builder::new().name(format!("mediares-decode-{i}")).spawn(worker_loop);
        }
        Pool { queue: Mutex::new(Queue::default()), wake: Condvar::new() }
    })
}

fn worker_loop() {
    let pool = pool();
    loop {
        let job = {
            let mut q = pool.lock();
            let job = loop {
                match q.next_job() {
                    Some(job) => break job,
                    None => q = pool.wake.wait(q).unwrap_or_else(|e| e.into_inner()),
                }
            };
            q.in_flight.push(job.clone());
            job
        };

        let cached = cache().get(&job.key);
        let result = cached.or_else(|| {
            let decoded = std::panic::catch_unwind(|| decode(&job.path, job.kind, job.key.auto_rotate));
            let img = decoded.ok().flatten().map(Arc::new)?;
            cache().insert(job.key.clone(), img.clone());
            Some(img)
        });
        let _ = job.result.set(result);

        let notify = {
            let mut q = pool.lock();
            q.in_flight.retain(|j| !Arc::ptr_eq(j, &job));
            std::mem::take(&mut *job.lock_notify())
        };
        for hwnd in notify {
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_IMAGE_READY, WPARAM(0), LPARAM(0));
            }
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
    to_bgra(img, BACKGROUND_GRAY, is_preview)
}

/// Converts to BGRA in place, compositing alpha over a gray level `background`.
pub fn to_bgra(img: DynamicImage, background: u8, is_preview: bool) -> DecodedImage {
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    let mut bgra = rgba.into_raw();
    let bg = background as u32;
    for px in bgra.chunks_exact_mut(4) {
        let a = px[3] as u32;
        let blend = |c: u8| ((c as u32 * a + bg * (255 - a) + 127) / 255) as u8;
        let (r, g, b) = if a == 255 { (px[0], px[1], px[2]) } else { (blend(px[0]), blend(px[1]), blend(px[2])) };
        px.copy_from_slice(&[b, g, r, 255]);
    }
    DecodedImage { width, height, bgra, is_preview }
}

/// Back to an `image` buffer (e.g. to scale a cached picture).
pub fn to_dynamic(img: &DecodedImage) -> Option<DynamicImage> {
    let rgba: Vec<u8> = img.bgra.chunks_exact(4).flat_map(|px| [px[2], px[1], px[0], px[3]]).collect();
    mediares_core::image::RgbaImage::from_raw(img.width, img.height, rgba).map(DynamicImage::ImageRgba8)
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
