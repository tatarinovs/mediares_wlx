//! Display-ready (BGRA) image cache with a small pool of background decoders.
//!
//! Decoding itself lives in `mediares_core::image_decode`; this module only adapts the result
//! for GDI, decodes off the UI thread and keeps recently used / neighbouring images around for
//! instant navigation.
//!
//! Photos are first decoded fitted to the screen, which is what the window shows unless zoomed
//! in: a 36 MP photo then takes about 8 MB instead of 146 MB and HEIC decodes about three times
//! faster. The whole picture of the photo on screen follows in the background, after the
//! neighbours, and is kept by its window only (never cached).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use mediares_core::cache::FileKey;
use mediares_core::exif::ExifInfo;
use mediares_core::image::DynamicImage;
use mediares_core::image_decode::{decode_bytes, decode_oriented_cancellable};
use mediares_core::probe::{probe_file, MediaType};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Upper bound for decoded pixels kept in memory (the image on screen is always kept).
const CACHE_BUDGET_BYTES: usize = 512 * 1024 * 1024;
const MAX_WORKERS: usize = 3;

/// Posted to the requesting window when its [`Ticket`] has a result.
pub const WM_IMAGE_READY: u32 = WM_APP + 0x12;

/// Default viewer background (COLORREF), also used to flatten transparency (GDI ignores alpha).
pub const BACKGROUND: u32 = 0x0018_1818;

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Size of the whole picture this one shows: larger than `width` x `height` when it was
    /// decoded fitted to the screen. Zoom, the OSD and the caption go by it.
    pub full: (u32, u32),
    /// Top-down 32bpp BGRA rows, alpha already composited over the background.
    pub bgra: Vec<u8>,
    /// True when the picture is an embedded preview rather than the full image (RAW files).
    pub is_preview: bool,
    /// Read along with the picture (it is needed for the orientation anyway); shown in the OSD.
    pub exif: Option<ExifInfo>,
}

/// How a photo is prepared for display; part of the cache key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodeOptions {
    pub auto_rotate: bool,
    /// COLORREF that transparency is flattened onto.
    pub background: u32,
    /// Longest side of the picture to decode (a square box, so turning the photo keeps it
    /// sharp); 0 = the whole picture.
    pub fit: u32,
}

impl DecodeOptions {
    /// The same, for the whole picture.
    pub fn whole(self) -> Self {
        DecodeOptions { fit: 0, ..self }
    }
}

impl DecodedImage {
    /// Whether this is the whole picture rather than a copy fitted to the screen.
    pub fn is_whole(&self) -> bool {
        self.full == (self.width, self.height)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    file: FileKey,
    options: DecodeOptions,
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
    static CACHE: Mutex<Cache> = Mutex::new(Cache {
        entries: Vec::new(),
    });
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Returns the image for display, decoding it on the calling thread on a cache miss.
pub fn load(path: &Path, kind: MediaType, options: DecodeOptions) -> Option<Arc<DecodedImage>> {
    let key = Key {
        file: FileKey::for_path(path)?,
        options,
    };
    if let Some(img) = cache().get(&key) {
        return Some(img);
    }
    let img = Arc::new(decode(path, kind, options, &|| false)?);
    cache().insert(key, img.clone());
    Some(img)
}

/// Viewer windows alive; the cache lives only while there is one.
static VIEWERS: AtomicUsize = AtomicUsize::new(0);

/// Held by every viewer window. When the last one goes, the decoded images are freed along with
/// pending work: the Lister is mostly opened for one file, and TC keeps the plugin loaded.
pub struct ViewerHold(());

impl ViewerHold {
    pub fn new() -> Self {
        VIEWERS.fetch_add(1, Ordering::SeqCst);
        ViewerHold(())
    }
}

impl Drop for ViewerHold {
    fn drop(&mut self) {
        if VIEWERS.fetch_sub(1, Ordering::SeqCst) == 1 {
            release_all();
        }
    }
}

/// Drops the cached images and queued decodes; decodes already running are not cached.
fn release_all() {
    // No pool yet means nothing was decoded in the background (e.g. only audio was shown).
    let mut q = POOL.get().map(Pool::lock);
    if VIEWERS.load(Ordering::SeqCst) > 0 {
        return; // a new viewer opened meanwhile
    }
    if let Some(q) = q.as_mut() {
        q.generation += 1;
        q.foreground.clear();
        q.prefetch.clear();
        q.whole.clear();
    }
    cache().entries = Vec::new();
}

pub enum Request {
    Ready(Arc<DecodedImage>),
    /// Decoding in the background; `notify` receives [`WM_IMAGE_READY`] when it is done.
    Pending(Ticket),
}

/// Returns a cached image right away, otherwise schedules it ahead of any prefetching.
/// `None` if the file is gone.
pub fn request(
    path: &Path,
    kind: MediaType,
    options: DecodeOptions,
    notify: HWND,
) -> Option<Request> {
    let key = Key {
        file: FileKey::for_path(path)?,
        options,
    };
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
    let existing = q
        .in_flight
        .iter()
        .chain(&q.foreground)
        .find(|job| job.key == key)
        .cloned();
    let job = existing.unwrap_or_else(|| {
        let job = Job::new(key, path.to_path_buf(), kind, q.generation);
        q.foreground.push_back(job.clone());
        pool.wake.notify_one();
        job
    });
    job.lock_notify().push(hwnd);
    Some(Request::Pending(Ticket { job, hwnd }))
}

/// Schedules the whole picture of the photo on screen, after everything else (`options.fit` is
/// ignored). The result is not cached: only the window that shows the photo keeps it.
pub fn request_whole(
    path: &Path,
    kind: MediaType,
    options: DecodeOptions,
    notify: HWND,
) -> Option<Ticket> {
    let key = Key {
        file: FileKey::for_path(path)?,
        options: options.whole(),
    };
    let pool = pool();
    let mut q = pool.lock();
    let hwnd = notify.0 as isize;
    let job = Job::new(key, path.to_path_buf(), kind, q.generation);
    job.lock_notify().push(hwnd);
    q.whole.push_back(job.clone());
    pool.wake.notify_one();
    Some(Ticket { job, hwnd })
}

/// Decodes the whole picture on the calling thread (to print or copy it before the background
/// decode is done); not cached.
pub fn decode_whole(path: &Path, kind: MediaType, options: DecodeOptions) -> Option<DecodedImage> {
    decode(path, kind, options.whole(), &|| false)
}

/// Replaces the prefetch queue with `paths` (most wanted first); stale requests from earlier
/// navigation are dropped.
pub fn prefetch(paths: Vec<PathBuf>, options: DecodeOptions) {
    let pool = pool();
    let mut q = pool.lock();
    q.prefetch = paths.into_iter().map(|p| (p, options)).collect();
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

    /// The result now: waits for a decode already running, or takes a queued one out of the
    /// queue and decodes it on the calling thread (two decodes of one picture at a time would
    /// double the memory and both take longer).
    pub fn finish(self) -> Option<Arc<DecodedImage>> {
        let queued = {
            let mut guard = pool().lock();
            let q = &mut *guard;
            let queue = [&mut q.foreground, &mut q.whole]
                .into_iter()
                .find(|queue| queue.iter().any(|job| Arc::ptr_eq(job, &self.job)));
            match queue {
                Some(queue) => {
                    queue.retain(|job| !Arc::ptr_eq(job, &self.job));
                    true
                }
                None => false,
            }
        };
        if queued {
            let job = &self.job;
            return decode(&job.path, job.kind, job.key.options, &|| false).map(Arc::new);
        }
        // The worker sets the result before it takes the lock to announce it.
        let pool = pool();
        let mut q = pool.lock();
        loop {
            if let Some(result) = self.result() {
                return result;
            }
            q = pool.finished.wait(q).unwrap_or_else(|e| e.into_inner());
        }
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
    /// [`Queue::generation`] when the job was made; a result from an older one is not cached.
    generation: u64,
    /// Windows to post [`WM_IMAGE_READY`] to (raw handles: HWND is not `Send`).
    notify: Mutex<Vec<isize>>,
    result: OnceLock<Option<Arc<DecodedImage>>>,
}

impl Job {
    fn new(key: Key, path: PathBuf, kind: MediaType, generation: u64) -> Arc<Job> {
        Arc::new(Job {
            key,
            path,
            kind,
            generation,
            notify: Mutex::new(Vec::new()),
            result: OnceLock::new(),
        })
    }

    fn lock_notify(&self) -> MutexGuard<'_, Vec<isize>> {
        self.notify.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether a window waits for the result (a prefetch becomes wanted once requested).
    fn is_wanted(&self) -> bool {
        !self.lock_notify().is_empty()
    }

    /// A whole picture whose window moved on: decoding it further is wasted.
    fn is_abandoned(&self) -> bool {
        self.key.options.fit == 0 && !self.is_wanted()
    }
}

/// What a worker takes up next.
enum Next {
    Job(Arc<Job>),
    /// A neighbour to prefetch: its file is looked up outside the queue lock (slow on network
    /// drives), then it becomes a job unless it is done or under way by then.
    Prefetch(PathBuf, DecodeOptions),
}

#[derive(Default)]
struct Queue {
    foreground: VecDeque<Arc<Job>>,
    prefetch: VecDeque<(PathBuf, DecodeOptions)>,
    /// Whole pictures of the photos on screen: run alone, when nothing else is queued or running.
    whole: VecDeque<Arc<Job>>,
    in_flight: Vec<Arc<Job>>,
    /// Decoder threads; with more than one, prefetching leaves one free for the photo asked for.
    workers: usize,
    /// Bumped when the last viewer closes and the cache is emptied.
    generation: u64,
}

impl Queue {
    /// The next job to run: requested images first, then prefetches not already done or running,
    /// then the whole picture of the photo on screen.
    /// Neighbours wait until no decode that a window waits for is running: the viewer is often
    /// opened for a single file, and codecs like the HEIF one gain nothing from parallel decodes
    /// (three at once take as long as three in a row), so a prefetch would only delay the photo.
    fn next_job(&mut self) -> Option<Next> {
        if let Some(job) = self.foreground.pop_front() {
            return Some(Next::Job(job));
        }
        if self.in_flight.iter().any(|job| job.is_wanted()) {
            return None;
        }
        if self.prefetch.is_empty() && self.in_flight.is_empty() {
            // A window that moved on no longer waits for its whole picture.
            while let Some(job) = self.whole.pop_front() {
                if job.is_wanted() {
                    return Some(Next::Job(job));
                }
            }
        }
        if self.workers > 1 && self.in_flight.len() + 1 >= self.workers {
            return None;
        }
        self.prefetch
            .pop_front()
            .map(|(path, options)| Next::Prefetch(path, options))
    }

    /// A job for a prefetch (keyed outside the lock), unless it is already done or running.
    fn prefetch_job(&self, path: PathBuf, key: Key) -> Option<Arc<Job>> {
        if self.in_flight.iter().any(|job| job.key == key) || cache().contains(&key) {
            return None;
        }
        let kind = probe_file(&path);
        Some(Job::new(key, path, kind, self.generation))
    }
}

struct Pool {
    queue: Mutex<Queue>,
    wake: Condvar,
    /// A job has its result (see [`Ticket::finish`]).
    finished: Condvar,
}

impl Pool {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

static POOL: OnceLock<Pool> = OnceLock::new();

fn pool() -> &'static Pool {
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let workers = (0..cores.saturating_sub(1).clamp(1, MAX_WORKERS))
            .filter(|i| {
                std::thread::Builder::new()
                    .name(format!("mediares-decode-{i}"))
                    .spawn(worker_loop)
                    .is_ok()
            })
            .count();
        Pool {
            queue: Mutex::new(Queue {
                workers,
                ..Queue::default()
            }),
            wake: Condvar::new(),
            finished: Condvar::new(),
        }
    })
}

fn worker_loop() {
    let pool = pool();
    loop {
        let job = {
            let mut q = pool.lock();
            let job = loop {
                match q.next_job() {
                    Some(Next::Job(job)) => break job,
                    Some(Next::Prefetch(path, options)) => {
                        drop(q);
                        let file = FileKey::for_path(&path);
                        q = pool.lock();
                        let job = file.and_then(|file| q.prefetch_job(path, Key { file, options }));
                        if let Some(job) = job {
                            break job;
                        }
                    }
                    None => q = pool.wake.wait(q).unwrap_or_else(|e| e.into_inner()),
                }
            };
            q.in_flight.push(job.clone());
            job
        };

        // Whole pictures are kept by their window only.
        let cacheable = job.key.options.fit != 0;
        let cached = cacheable.then(|| cache().get(&job.key)).flatten();
        let result = cached.or_else(|| {
            let abandoned = || job.is_abandoned();
            let decoded = std::panic::catch_unwind(|| {
                decode(&job.path, job.kind, job.key.options, &abandoned)
            });
            let img = decoded.ok().flatten().map(Arc::new)?;
            // Under the queue lock, so the cache cannot be emptied between check and insert.
            let q = pool.lock();
            if cacheable && q.generation == job.generation {
                cache().insert(job.key.clone(), img.clone());
            }
            drop(q);
            Some(img)
        });
        let _ = job.result.set(result);

        let notify = {
            let mut q = pool.lock();
            q.in_flight.retain(|j| !Arc::ptr_eq(j, &job));
            let notify = std::mem::take(&mut *job.lock_notify());
            if !notify.is_empty() {
                // Prefetching held back for this job may start now, on every idle worker.
                pool.wake.notify_all();
            }
            pool.finished.notify_all();
            notify
        };
        for hwnd in notify {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut _)),
                    WM_IMAGE_READY,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

/// `cancelled`: give up a whole picture part way (see `decode_oriented_cancellable`).
fn decode(
    path: &Path,
    kind: MediaType,
    options: DecodeOptions,
    cancelled: &dyn Fn() -> bool,
) -> Option<DecodedImage> {
    let fit = (options.fit != 0).then_some((options.fit, options.fit));
    let (img, full, exif) =
        decode_oriented_cancellable(path, kind, options.auto_rotate, fit, cancelled)?;
    Some(DecodedImage {
        exif,
        full,
        ..to_bgra(img, options.background, kind == MediaType::RawImage)
    })
}

/// Album art never needs more pixels than this on a side, even fullscreen.
const PICTURE_SIDE: u32 = 2048;

/// Decodes an in-memory picture (e.g. embedded album art) for display; not cached. Scans of
/// 3000 px and more are reduced while decoding where the codec can.
pub fn decode_picture(bytes: &[u8]) -> Option<DecodedImage> {
    use mediares_core::wic_decode::{decode_turned, Input};
    let fit = (PICTURE_SIDE, PICTURE_SIDE);
    let img = match decode_turned(Input::Memory(bytes), Some(fit), 1) {
        Some((img, _)) => img,
        None => {
            let img = decode_bytes(bytes)?;
            let (w, h) = mediares_core::image_decode::fitted_size((img.width(), img.height()), fit);
            if (w, h) == (img.width(), img.height()) {
                img
            } else {
                mediares_core::image_decode::thumbnail(&img, w, h)
            }
        }
    };
    Some(to_bgra(img, BACKGROUND, false))
}

/// Converts to BGRA in place, compositing alpha over the COLORREF `background`.
pub fn to_bgra(img: DynamicImage, background: u32, is_preview: bool) -> DecodedImage {
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    let mut bgra = rgba.into_raw();
    let bg = [
        background & 0xFF,
        (background >> 8) & 0xFF,
        (background >> 16) & 0xFF,
    ];
    for px in bgra.chunks_exact_mut(4) {
        let a = px[3] as u32;
        let blend = |c: u8, bg: u32| ((c as u32 * a + bg * (255 - a) + 127) / 255) as u8;
        let (r, g, b) = if a == 255 {
            (px[0], px[1], px[2])
        } else {
            (
                blend(px[0], bg[0]),
                blend(px[1], bg[1]),
                blend(px[2], bg[2]),
            )
        };
        px.copy_from_slice(&[b, g, r, 255]);
    }
    DecodedImage {
        width,
        height,
        full: (width, height),
        bgra,
        is_preview,
        exif: None,
    }
}

/// The picture turned by a quarter, clockwise or counter-clockwise.
pub fn rotated(img: &DecodedImage, clockwise: bool) -> DecodedImage {
    let (w, h) = (img.width as usize, img.height as usize);
    let mut dst = vec![0u8; img.bgra.len()];
    // Whole pixels, in tiles that stay in the CPU cache: column reads across a large picture
    // would miss it on every pixel.
    const TILE: usize = 64;
    // SAFETY: every bit pattern is a valid `u32`; misaligned ends are checked below.
    let aligned = unsafe { (img.bgra.align_to::<u32>(), dst.align_to_mut::<u32>()) };
    let (src, dst32) = match aligned {
        (([], src, []), ([], dst32, [])) => (src, dst32),
        // Allocations are aligned in practice; a byte-wise turn would do otherwise.
        _ => return rotated_bytes(img, clockwise),
    };
    for ty in (0..h).step_by(TILE) {
        for tx in (0..w).step_by(TILE) {
            for y in ty..(ty + TILE).min(h) {
                let row = &src[y * w..(y + 1) * w];
                for (x, &px) in row.iter().enumerate().take((tx + TILE).min(w)).skip(tx) {
                    // The picture turned is `h` pixels wide.
                    let (nx, ny) = if clockwise {
                        (h - 1 - y, x)
                    } else {
                        (y, w - 1 - x)
                    };
                    dst32[ny * h + nx] = px;
                }
            }
        }
    }
    DecodedImage {
        width: img.height,
        height: img.width,
        full: (img.full.1, img.full.0),
        bgra: dst,
        is_preview: img.is_preview,
        exif: img.exif.clone(),
    }
}

/// [`rotated`] a byte at a time.
fn rotated_bytes(img: &DecodedImage, clockwise: bool) -> DecodedImage {
    let (w, h) = (img.width as usize, img.height as usize);
    let mut dst = Vec::with_capacity(img.bgra.len());
    // Output row `y` (of `w` rows, each `h` pixels wide) reads a source column.
    for y in 0..w {
        for x in 0..h {
            let (sx, sy) = if clockwise {
                (y, h - 1 - x)
            } else {
                (w - 1 - y, x)
            };
            let i = (sy * w + sx) * 4;
            dst.extend_from_slice(&img.bgra[i..i + 4]);
        }
    }
    DecodedImage {
        width: img.height,
        height: img.width,
        full: (img.full.1, img.full.0),
        bgra: dst,
        is_preview: img.is_preview,
        exif: img.exif.clone(),
    }
}

/// Back to an `image` buffer (e.g. to scale a cached picture).
pub fn to_dynamic(img: &DecodedImage) -> Option<DynamicImage> {
    let rgba: Vec<u8> = img
        .bgra
        .chunks_exact(4)
        .flat_map(|px| [px[2], px[1], px[0], px[3]])
        .collect();
    mediares_core::image::RgbaImage::from_raw(img.width, img.height, rgba)
        .map(DynamicImage::ImageRgba8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mediares_core::image::{Rgba, RgbaImage};

    /// The next job as a worker gets it (a prefetch keyed on the spot).
    fn take(q: &mut Queue) -> Option<Arc<Job>> {
        match q.next_job()? {
            Next::Job(job) => Some(job),
            Next::Prefetch(path, options) => {
                let file = FileKey::for_path(&path)?;
                q.prefetch_job(path, Key { file, options })
            }
        }
    }

    #[test]
    fn converts_to_bgra_over_background() {
        let img = RgbaImage::from_fn(2, 1, |x, _| {
            if x == 0 {
                Rgba([255, 0, 10, 255])
            } else {
                Rgba([255, 255, 255, 0])
            }
        });
        let out = to_bgra(DynamicImage::ImageRgba8(img), 0x0030_2010, false);
        assert_eq!(&out.bgra[0..4], &[10, 0, 255, 255]);
        assert_eq!(&out.bgra[4..8], &[0x30, 0x20, 0x10, 255]);
    }

    #[test]
    fn rotates_by_quarter_turns() {
        // 2x1: red, green  ->  clockwise 1x2: red above green; counter-clockwise: green above red.
        let img = DecodedImage {
            width: 2,
            height: 1,
            full: (2, 1),
            bgra: vec![0, 0, 255, 255, 0, 255, 0, 255],
            is_preview: false,
            exif: None,
        };
        let cw = rotated(&img, true);
        assert_eq!((cw.width, cw.height), (1, 2));
        assert_eq!(cw.bgra, vec![0, 0, 255, 255, 0, 255, 0, 255]);
        assert_eq!(
            rotated(&img, false).bgra,
            vec![0, 255, 0, 255, 0, 0, 255, 255]
        );
        // 2x2 turned four times is the original.
        let square = DecodedImage {
            width: 2,
            height: 2,
            full: (2, 2),
            bgra: (0..16).collect(),
            is_preview: false,
            exif: None,
        };
        let back = (0..4).fold(square, |img, _| rotated(&img, true));
        assert_eq!(back.bgra, (0..16).collect::<Vec<u8>>());
    }

    #[test]
    fn cache_evicts_least_recently_used_by_bytes() {
        let big = |n: usize| {
            Arc::new(DecodedImage {
                width: 1,
                height: 1,
                full: (1, 1),
                bgra: vec![0; n],
                is_preview: false,
                exif: None,
            })
        };
        let key = |name: &str| Key {
            file: FileKey::for_path(Path::new(env!("CARGO_MANIFEST_DIR")).join(name).as_path())
                .expect("file exists"),
            options: DecodeOptions {
                auto_rotate: true,
                background: BACKGROUND,
                fit: 0,
            },
        };
        let (a, b, c) = (key("Cargo.toml"), key("src/lib.rs"), key("src/config.rs"));
        let mut cache = Cache {
            entries: Vec::new(),
        };
        let third = CACHE_BUDGET_BYTES / 3 + 1;
        cache.insert(a.clone(), big(third));
        cache.insert(b.clone(), big(third));
        assert!(cache.get(&a).is_some()); // `a` becomes most recently used
        cache.insert(c.clone(), big(third));
        assert!(cache.contains(&a) && cache.contains(&c));
        assert!(!cache.contains(&b));
    }

    #[test]
    fn prefetch_waits_for_the_requested_photo() {
        let options = DecodeOptions {
            auto_rotate: true,
            background: BACKGROUND,
            fit: 0,
        };
        let file = |name: &str| Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
        let job = |name: &str| {
            let key = Key {
                file: FileKey::for_path(&file(name)).expect("file exists"),
                options,
            };
            Job::new(key, file(name), MediaType::StandardImage, 0)
        };
        let shown = job("Cargo.toml");
        shown.lock_notify().push(1);
        let mut q = Queue {
            in_flight: vec![shown.clone()],
            prefetch: [(file("src/lib.rs"), options)].into(),
            workers: 3,
            ..Queue::default()
        };
        assert!(q.next_job().is_none());
        // Done (or no longer wanted): the neighbour may go.
        shown.lock_notify().clear();
        assert_eq!(
            take(&mut q).map(|j| j.path.clone()),
            Some(file("src/lib.rs"))
        );
    }

    #[test]
    fn last_viewer_frees_the_cache() {
        let key = Key {
            file: FileKey::for_path(&Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
                .expect("exists"),
            options: DecodeOptions {
                auto_rotate: false,
                background: 0,
                fit: 0,
            },
        };
        let img = Arc::new(DecodedImage {
            width: 1,
            height: 1,
            full: (1, 1),
            bgra: vec![0; 4],
            is_preview: false,
            exif: None,
        });
        let (first, second) = (ViewerHold::new(), ViewerHold::new());
        cache().insert(key.clone(), img);
        drop(first);
        assert!(cache().contains(&key), "another viewer is still open");
        drop(second);
        assert!(!cache().contains(&key));
    }

    #[test]
    fn whole_picture_waits_for_the_neighbours() {
        let options = DecodeOptions {
            auto_rotate: true,
            background: BACKGROUND,
            fit: 1920,
        };
        let file = |name: &str| Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
        let job = |name: &str, options: DecodeOptions| {
            let key = Key {
                file: FileKey::for_path(&file(name)).expect("file exists"),
                options,
            };
            Job::new(key, file(name), MediaType::StandardImage, 0)
        };
        let whole = job("Cargo.toml", options.whole());
        whole.lock_notify().push(1);
        let neighbour = job("src/lib.rs", options);
        let mut q = Queue {
            whole: [whole.clone()].into(),
            prefetch: [(file("src/lib.rs"), options)].into(),
            workers: 3,
            ..Queue::default()
        };
        let next = take(&mut q).expect("the neighbour");
        assert_eq!(next.path, neighbour.path);
        q.in_flight.push(next);
        assert!(q.next_job().is_none(), "a neighbour is still decoding");
        q.in_flight.clear();
        assert!(Arc::ptr_eq(
            &take(&mut q).expect("the whole picture"),
            &whole
        ));
        // A window that moved on drops its ticket: nothing is decoded for it.
        let gone = job("Cargo.toml", options.whole());
        q.whole.push_back(gone);
        assert!(q.next_job().is_none());
    }
}
