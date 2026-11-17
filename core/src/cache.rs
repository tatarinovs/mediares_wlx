//! MediaCache: in-memory LRU cache of image, video and audio analysis results.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use crate::hashing::ImageAnalysis;
use crate::image_decode::decode_for_gray_analysis;
use crate::probe::{probe_file, MediaType};
use crate::video_frame::{analyze_video, probe_video_meta, VideoAnalysis, VideoError, VideoMeta};

/// Analysis results are small (hashes); a folder of thousands of files stays cached while TC
/// sorts and re-sorts it.
const CAPACITY: usize = 8192;
/// Metadata entries are a few hundred bytes; sorting a big folder by a column reads every file,
/// and a smaller cache would read them all again on the next sort.
const META_CAPACITY: usize = 8192;

#[derive(Debug, Clone)]
pub enum CachedMedia {
    Image(Arc<ImageAnalysis>),
    Video(Arc<VideoAnalysis>),
    Unsupported,
}

/// Spectral fingerprint; `None` for (nearly) silent files.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFingerprint {
    pub duration_sec: u32,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioPcmHash {
    pub duration_sec: u32,
    pub pcm_hash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioDuration(pub u32);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioField {
    Fingerprint,
    PcmHash,
    Duration,
}

/// Identifies a specific version of a file: edits change size or mtime and miss the cache.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct FileKey {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    pub(crate) modified: Option<SystemTime>,
}

thread_local! {
    /// The key of the file a TC call is about, while [`with_file_key`] runs.
    static SCOPED_KEY: RefCell<Option<FileKey>> = const { RefCell::new(None) };
    /// Background prefetch skips files already being decoded instead of blocking on Condvar.
    static NON_BLOCKING_FLIGHT: Cell<bool> = const { Cell::new(false) };
    /// Key whose stat/freshness was already verified inside `persistent_result`.
    static VERIFIED_KEY: RefCell<Option<FileKey>> = const { RefCell::new(None) };
}

impl FileKey {
    pub fn for_path(path: &Path) -> Option<Self> {
        let scoped = SCOPED_KEY.with_borrow(|k| k.as_ref().filter(|k| k.path == path).cloned());
        scoped.or_else(|| Self::stat(path))
    }

    pub(crate) fn stat(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Some(FileKey {
            path: path.to_path_buf(),
            size: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    pub(crate) fn from_dir_entry(entry: &fs::DirEntry) -> Option<Self> {
        let meta = entry.metadata().ok()?;
        Some(FileKey {
            path: entry.path(),
            size: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// Runs `f` with a pre-resolved version key of the file, avoiding redundant filesystem stats.
pub fn with_known_file_key<R>(key: Option<FileKey>, f: impl FnOnce() -> R) -> R {
    /// Puts the previous key back, also when `f` panics (the FFI guard catches it, and a key
    /// left behind would make later lookups miss edits of the file).
    struct Restore(Option<FileKey>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = SCOPED_KEY.try_with(|k| k.replace(self.0.take()));
        }
    }
    let _restore = Restore(SCOPED_KEY.replace(key));
    f()
}

/// Runs `f` with the version of `path` looked up once: TC asks for each column separately, and
/// every cache lookup on the way would otherwise ask the file system again (slow on network
/// drives and under antivirus scanners).
pub fn with_file_key<R>(path: &Path, f: impl FnOnce() -> R) -> R {
    with_known_file_key(FileKey::stat(path), f)
}

pub(crate) fn with_non_blocking_flight<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = NON_BLOCKING_FLIGHT.try_with(|b| b.set(self.0));
        }
    }
    let _restore = Restore(NON_BLOCKING_FLIGHT.replace(true));
    f()
}

/// Least-recently-used map from file versions to values, shared between threads.
struct FileCache<V> {
    inner: Mutex<Lru<V>>,
    flights: Mutex<HashSet<FileKey>>,
    ready: Condvar,
    budget: usize,
    weigh: fn(&V) -> usize,
}

struct Lru<V> {
    /// Value and the tick it was last used at.
    map: HashMap<FileKey, (V, u64, usize)>,
    bytes: usize,
    clock: u64,
    capacity: usize,
}

impl<V: Clone> FileCache<V> {
    fn new(capacity: usize) -> Self {
        Self::weighted(capacity, usize::MAX, |_| 0)
    }

    fn weighted(capacity: usize, budget: usize, weigh: fn(&V) -> usize) -> Self {
        Self {
            flights: Mutex::new(HashSet::new()),
            ready: Condvar::new(),
            budget,
            weigh,
            inner: Mutex::new(Lru {
                map: HashMap::new(),
                clock: 0,
                bytes: 0,
                capacity,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Lru<V>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The cached value for the current version of `path`, or `compute`'s result (cached unless
    /// it is `None`). `None` also if the file is gone.
    fn get_or_compute(&self, path: &Path, compute: impl FnOnce(&Path) -> Option<V>) -> Option<V> {
        let key = FileKey::for_path(path)?;
        {
            let mut lru = self.lock();
            let next_tick = lru.clock + 1;
            if let Some((value, used, _)) = lru.map.get_mut(&key) {
                *used = next_tick;
                let value = value.clone();
                lru.clock = next_tick;
                return Some(value);
            }
        }
        // A foreground request may meet a prefetch already decoding this file. Serialize only
        // the same file/version in this cache family; unrelated files remain parallel.
        let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
        while flights.contains(&key) {
            if NON_BLOCKING_FLIGHT.get() {
                return None;
            }
            flights = self.ready.wait(flights).unwrap_or_else(|e| e.into_inner());
        }
        flights.insert(key.clone());
        drop(flights);
        struct Flight<'a>(&'a Mutex<HashSet<FileKey>>, &'a Condvar, FileKey);
        impl Drop for Flight<'_> {
            fn drop(&mut self) {
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.2);
                self.1.notify_all();
            }
        }
        let _flight = Flight(&self.flights, &self.ready, key.clone());
        {
            let mut lru = self.lock();
            let next_tick = lru.clock + 1;
            if let Some((value, used, _)) = lru.map.get_mut(&key) {
                *used = next_tick;
                let value = value.clone();
                lru.clock = next_tick;
                return Some(value);
            }
        }
        let value = compute(path)?;
        // An editor may have saved the file while a decoder was reading it. Do not attach
        // the result to either version; the next call must read the file again.
        let verified = VERIFIED_KEY.with_borrow_mut(|v| v.take());
        if verified.as_ref() != Some(&key) && FileKey::stat(path).as_ref() != Some(&key) {
            return None;
        }
        let bytes = (self.weigh)(&value)
            .saturating_add(key.path.as_os_str().len() * 2 + size_of::<FileKey>());
        if bytes > self.budget {
            return Some(value);
        }
        let mut lru = self.lock();
        lru.clock += 1;
        let now = lru.clock;
        if let Some((_, _, old)) = lru.map.insert(key, (value.clone(), now, bytes)) {
            lru.bytes -= old;
        }
        lru.bytes = lru.bytes.saturating_add(bytes);
        while lru.map.len() > lru.capacity || lru.bytes > self.budget {
            // The least recently used eighth at once: finding them scans the whole map, too slow
            // to repeat for every new file of a folder larger than the cache.
            let ticks = lru.map.values().map(|&(_, used, _)| used).collect();
            let newest_dropped = nth_smallest(ticks, lru.capacity.min(lru.map.len()) / 8);
            lru.map.retain(|_, (_, used, _)| *used > newest_dropped);
            lru.bytes = lru.map.values().map(|(_, _, bytes)| bytes).sum();
        }
        Some(value)
    }

    fn contains(&self, path: &Path) -> bool {
        FileKey::for_path(path).is_some_and(|key| self.lock().map.contains_key(&key))
    }

    #[cfg(feature = "audio-decode")]
    fn cached(&self, path: &Path) -> Option<V> {
        let key = FileKey::for_path(path)?;
        let mut lru = self.lock();
        lru.clock += 1;
        let now = lru.clock;
        let (value, used, _) = lru.map.get_mut(&key)?;
        *used = now;
        Some(value.clone())
    }

    fn clear(&self) {
        let mut lru = self.lock();
        lru.map.clear();
        lru.bytes = 0;
    }
}

/// The `n`-th smallest of `values` (0 = the smallest). Not generic and not inlined: each cached
/// value type would carry a copy of the selection otherwise.
#[inline(never)]
fn nth_smallest(mut values: Vec<u64>, n: usize) -> u64 {
    let n = n.min(values.len().saturating_sub(1));
    *values.select_nth_unstable(n).1
}

pub struct MediaCache(FileCache<CachedMedia>);

impl MediaCache {
    /// Returns the cached analysis or computes it. A cancelled analysis is returned as
    /// `Unsupported` but not cached, so the next request recomputes it.
    pub fn get_or_analyze(&self, path: &Path, cancelled: &dyn Fn() -> bool) -> CachedMedia {
        self.0
            .get_or_compute(path, |path| {
                persistent_result(path, cancelled, || analyze_file(path, cancelled))
                    .map(|(value, _)| value)
            })
            .unwrap_or(CachedMedia::Unsupported)
    }

    pub fn is_cached(&self, path: &Path) -> bool {
        self.0.contains(path)
    }

    pub fn clear(&self) {
        self.0.clear();
    }
}

/// Called only on a RAM miss, outside the RAM-cache mutex. SQLite access never holds a lock
/// while decoding; failures simply miss the disk cache. A fresh stat bypasses SCOPED_KEY
/// before saving, since it still describes the file at the beginning of the TC call.
fn persistent_result<T: crate::disk_cache::DiskValue>(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
    compute: impl FnOnce() -> Option<T>,
) -> Option<(T, usize)> {
    if cancelled() {
        return None;
    }
    let key = FileKey::for_path(path)?;
    if let Some(value) = crate::disk_cache::read(&key) {
        if cancelled() {
            return None;
        }
        VERIFIED_KEY.with_borrow_mut(|v| *v = Some(key));
        return Some(value);
    }
    let value = compute()?;
    if cancelled() || FileKey::stat(path).as_ref() != Some(&key) {
        return None;
    }
    let bytes = crate::disk_cache::write(&key, &value);
    VERIFIED_KEY.with_borrow_mut(|v| *v = Some(key));
    Some((value, bytes))
}

/// `None` if the host cancelled the analysis.
fn analyze_file(path: &Path, cancelled: &dyn Fn() -> bool) -> Option<CachedMedia> {
    Some(match probe_file(path) {
        kind if kind.is_image_kind() => {
            decode_for_gray_analysis(path, kind, crate::hashing::WORKING_SIZE)
                .map(|(gray, (w, h))| {
                    CachedMedia::Image(Arc::new(crate::hashing::analyze_gray(&gray, w, h)))
                })
                .unwrap_or(CachedMedia::Unsupported)
        }
        MediaType::Video => match analyze_video(path, cancelled) {
            Ok(v) => CachedMedia::Video(Arc::new(v)),
            Err(VideoError::Cancelled) => return None,
            Err(VideoError::Failed(_)) => CachedMedia::Unsupported,
        },
        _ => CachedMedia::Unsupported,
    })
}

fn audio_fingerprint_cache() -> &'static FileCache<AudioFingerprint> {
    static CACHE: OnceLock<FileCache<AudioFingerprint>> = OnceLock::new();
    CACHE.get_or_init(|| FileCache::new(CAPACITY))
}

fn audio_pcm_cache() -> &'static FileCache<AudioPcmHash> {
    static CACHE: OnceLock<FileCache<AudioPcmHash>> = OnceLock::new();
    CACHE.get_or_init(|| FileCache::new(CAPACITY))
}

fn audio_duration_cache() -> &'static FileCache<AudioDuration> {
    static CACHE: OnceLock<FileCache<AudioDuration>> = OnceLock::new();
    CACHE.get_or_init(|| FileCache::new(CAPACITY))
}

pub(crate) fn is_audio_cached(path: &Path, field: AudioField) -> bool {
    match field {
        AudioField::Fingerprint => audio_fingerprint_cache().contains(path),
        AudioField::PcmHash => audio_pcm_cache().contains(path),
        AudioField::Duration => {
            audio_duration_cache().contains(path)
                || audio_fingerprint_cache().contains(path)
                || audio_pcm_cache().contains(path)
        }
    }
}

#[cfg(feature = "audio-decode")]
fn audio_result<T: crate::disk_cache::DiskValue + Clone>(
    cache: &FileCache<T>,
    path: &Path,
    cancelled: &dyn Fn() -> bool,
    analyze: impl FnOnce() -> Result<T, crate::audio_fingerprint::AudioError>,
) -> Option<T> {
    cache.get_or_compute(path, |path| {
        persistent_result(path, cancelled, || analyze().ok()).map(|(value, _)| value)
    })
}

#[cfg(feature = "audio-decode")]
pub fn get_audio_fingerprint(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Option<AudioFingerprint> {
    audio_result(audio_fingerprint_cache(), path, cancelled, || {
        crate::audio_fingerprint::fingerprint_audio(path, cancelled)
    })
}

#[cfg(feature = "audio-decode")]
pub fn get_audio_pcm_hash(path: &Path, cancelled: &dyn Fn() -> bool) -> Option<AudioPcmHash> {
    audio_result(audio_pcm_cache(), path, cancelled, || {
        crate::audio_fingerprint::hash_audio_pcm(path, cancelled)
    })
}

#[cfg(feature = "audio-decode")]
pub fn get_audio_duration(path: &Path, cancelled: &dyn Fn() -> bool) -> Option<AudioDuration> {
    if cancelled() {
        return None;
    }
    // Both hash passes already count frames. Reuse that byproduct without starting the
    // other algorithm or doing SQLite work on a RAM hit.
    if let Some(duration) = audio_fingerprint_cache()
        .cached(path)
        .map(|a| a.duration_sec)
        .or_else(|| audio_pcm_cache().cached(path).map(|a| a.duration_sec))
    {
        return Some(AudioDuration(duration));
    }
    audio_result(audio_duration_cache(), path, cancelled, || {
        let key =
            FileKey::for_path(path).ok_or(crate::audio_fingerprint::AudioError::Unsupported)?;
        if let Some(duration) = crate::disk_cache::read::<AudioFingerprint>(&key)
            .map(|(a, _)| a.duration_sec)
            .or_else(|| crate::disk_cache::read::<AudioPcmHash>(&key).map(|(a, _)| a.duration_sec))
        {
            return Ok(AudioDuration(duration));
        }
        crate::audio_fingerprint::decoded_duration(path, cancelled)
    })
}

/// Cheap per-file metadata (tags, EXIF, stream properties) keyed by file version: TC asks for
/// every column separately, and each file is read once. Failed reads remain retryable.
struct MetaCache<T>(FileCache<(Arc<T>, usize)>);

impl<T: crate::disk_cache::DiskValue> MetaCache<T> {
    fn new() -> Self {
        // Estimate heap use conservatively from the disk representation; bound each family
        // independently so long comments or TIFF page lists cannot crowd out all metadata.
        MetaCache(FileCache::weighted(
            META_CAPACITY,
            8 * 1024 * 1024,
            |(_, bytes)| *bytes,
        ))
    }

    fn get_or_read(&self, path: &Path, read: impl FnOnce(&Path) -> Option<T>) -> Option<Arc<T>> {
        self.0
            .get_or_compute(path, |p| {
                persistent_result(p, &|| false, || read(p))
                    .map(|(value, bytes)| (Arc::new(value), bytes))
            })
            .map(|(value, _)| value)
    }

    fn contains(&self, path: &Path) -> bool {
        self.0.contains(path)
    }

    fn clear(&self) {
        self.0.clear();
    }
}

#[cfg(feature = "tags")]
fn tags_cache() -> &'static MetaCache<crate::audio_tags::AudioTags> {
    static TAGS: OnceLock<MetaCache<crate::audio_tags::AudioTags>> = OnceLock::new();
    TAGS.get_or_init(MetaCache::new)
}

/// Audio tags without pictures.
#[cfg(feature = "tags")]
pub fn get_tags(path: &Path) -> Option<Arc<crate::audio_tags::AudioTags>> {
    tags_cache().get_or_read(path, |p| crate::audio_tags::read_tags(p, false))
}

#[cfg(feature = "tags")]
pub fn is_tags_cached(path: &Path) -> bool {
    tags_cache().contains(path)
}

fn exif_cache() -> &'static MetaCache<crate::exif::ExifInfo> {
    static EXIF: OnceLock<MetaCache<crate::exif::ExifInfo>> = OnceLock::new();
    EXIF.get_or_init(MetaCache::new)
}

pub fn get_exif(path: &Path) -> Option<Arc<crate::exif::ExifInfo>> {
    exif_cache().get_or_read(path, crate::exif::read_exif_for_cache)
}

pub fn is_exif_cached(path: &Path) -> bool {
    exif_cache().contains(path)
}

fn tiff_pages_cache() -> &'static MetaCache<Vec<crate::wic_decode::TiffPage>> {
    static PAGES: OnceLock<MetaCache<Vec<crate::wic_decode::TiffPage>>> = OnceLock::new();
    PAGES.get_or_init(MetaCache::new)
}

/// The pages of a TIFF (see `wic_decode::tiff_pages`), looked up once per file version: every
/// page decoded (fitted, whole, ahead) needs them.
pub fn get_tiff_pages(path: &Path) -> Option<Arc<Vec<crate::wic_decode::TiffPage>>> {
    tiff_pages_cache().get_or_read(path, crate::wic_decode::tiff_pages)
}

fn image_size_cache() -> &'static MetaCache<(u32, u32)> {
    static SIZES: OnceLock<MetaCache<(u32, u32)>> = OnceLock::new();
    SIZES.get_or_init(MetaCache::new)
}

/// Size of a standard image from its header, read once for all the size columns. For formats a
/// system codec reads (see `header_needs_codec`) that is cheap once the codec is loaded, but not
/// free like a JPEG header.
pub fn get_image_size(path: &Path) -> Option<(u32, u32)> {
    image_size_cache()
        .get_or_read(path, crate::image_decode::header_dimensions)
        .map(|s| *s)
}

pub fn is_image_size_cached(path: &Path) -> bool {
    image_size_cache().contains(path)
}

fn pixel_format_cache() -> &'static MetaCache<crate::image_decode::PixelFormat> {
    static FORMATS: OnceLock<MetaCache<crate::image_decode::PixelFormat>> = OnceLock::new();
    FORMATS.get_or_init(MetaCache::new)
}

/// Bit depth and alpha of a standard image, from its header.
pub fn get_pixel_format(path: &Path) -> Option<crate::image_decode::PixelFormat> {
    pixel_format_cache()
        .get_or_read(path, crate::image_decode::header_pixel_format)
        .map(|f| *f)
}

pub fn is_pixel_format_cached(path: &Path) -> bool {
    pixel_format_cache().contains(path)
}

fn sharpness_cache() -> &'static MetaCache<u32> {
    static SHARPNESS: OnceLock<MetaCache<u32>> = OnceLock::new();
    SHARPNESS.get_or_init(MetaCache::new)
}

/// Sharpness of a photo (see `hashing::sharpness`): decoded at the measuring size only, so
/// JPEG and RAW previews shrink while decoding and nothing else pays for it.
pub fn get_sharpness(path: &Path) -> Option<u32> {
    sharpness_cache()
        .get_or_read(path, |p| {
            let side = crate::hashing::SHARPNESS_SIZE;
            let (gray, _) = decode_for_gray_analysis(p, probe_file(p), side)?;
            Some(crate::hashing::sharpness_gray(&gray))
        })
        .map(|s| *s)
}

pub fn is_sharpness_cached(path: &Path) -> bool {
    sharpness_cache().contains(path)
}

fn video_meta_cache() -> &'static MetaCache<VideoMeta> {
    static VIDEO: OnceLock<MetaCache<VideoMeta>> = OnceLock::new();
    VIDEO.get_or_init(MetaCache::new)
}

/// Stream properties of files Media Foundation can't open (the viewer plugs libmpv in here when
/// it is installed). Asked first for the formats only libmpv plays, see `probe::is_mpv_only`.
pub struct MetaFallback {
    pub video: fn(&Path) -> Option<VideoMeta>,
    pub audio: fn(&Path) -> Option<crate::mf_audio::AudioStreamMeta>,
}

static META_FALLBACK: OnceLock<MetaFallback> = OnceLock::new();

/// Only the first registration counts.
pub fn set_meta_fallback(fallback: MetaFallback) {
    let _ = META_FALLBACK.set(fallback);
}

type MetaReader<T> = fn(&Path) -> Option<T>;

fn with_fallback<T>(
    path: &Path,
    mf: MetaReader<T>,
    pick: fn(&MetaFallback) -> MetaReader<T>,
) -> Option<T> {
    let Some(fallback) = META_FALLBACK.get().map(pick) else {
        return mf(path);
    };
    if crate::probe::is_mpv_only(path) {
        fallback(path).or_else(|| mf(path))
    } else {
        mf(path).or_else(|| fallback(path))
    }
}

/// Stream properties via Media Foundation: no decoding, but opening the source still takes a while.
pub fn get_video_meta(path: &Path) -> Option<Arc<VideoMeta>> {
    video_meta_cache().get_or_read(path, |p| with_fallback(p, probe_video_meta, |f| f.video))
}

pub fn is_video_meta_cached(path: &Path) -> bool {
    video_meta_cache().contains(path)
}

fn audio_meta_cache() -> &'static MetaCache<crate::mf_audio::AudioStreamMeta> {
    static AUDIO: OnceLock<MetaCache<crate::mf_audio::AudioStreamMeta>> = OnceLock::new();
    AUDIO.get_or_init(MetaCache::new)
}

/// Audio stream properties via Media Foundation, for files symphonia can't parse (WMA, AC3...).
pub fn get_audio_meta(path: &Path) -> Option<Arc<crate::mf_audio::AudioStreamMeta>> {
    audio_meta_cache().get_or_read(path, |p| {
        with_fallback(p, crate::mf_audio::probe_audio_meta, |f| f.audio)
    })
}

pub fn is_audio_meta_cached(path: &Path) -> bool {
    audio_meta_cache().contains(path)
}

fn video_tags_cache() -> &'static MetaCache<crate::video_tags::VideoTags> {
    static VIDEO_TAGS: OnceLock<MetaCache<crate::video_tags::VideoTags>> = OnceLock::new();
    VIDEO_TAGS.get_or_init(MetaCache::new)
}

/// Title, artist... of MKV/WebM and MP4/MOV: header reads only.
pub fn get_video_tags(path: &Path) -> Option<Arc<crate::video_tags::VideoTags>> {
    video_tags_cache().get_or_read(path, crate::video_tags::try_read_video_tags)
}

pub fn is_video_tags_cached(path: &Path) -> bool {
    video_tags_cache().contains(path)
}

/// Empties every cache (the plugin is being unloaded).
pub fn clear_all() {
    get_cache().clear();
    audio_fingerprint_cache().clear();
    audio_pcm_cache().clear();
    audio_duration_cache().clear();
    #[cfg(feature = "tags")]
    tags_cache().clear();
    exif_cache().clear();
    tiff_pages_cache().clear();
    image_size_cache().clear();
    pixel_format_cache().clear();
    sharpness_cache().clear();
    video_meta_cache().clear();
    audio_meta_cache().clear();
    video_tags_cache().clear();
    crate::disk_cache::close();
}

pub fn get_cache() -> &'static MediaCache {
    static GLOBAL_CACHE: OnceLock<MediaCache> = OnceLock::new();
    GLOBAL_CACHE.get_or_init(|| MediaCache(FileCache::new(CAPACITY)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_requests_share_one_computation_and_retry_after_panic() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let path = std::env::temp_dir().join(format!("mediares-flight-{}", std::process::id()));
        fs::write(&path, b"test").unwrap();
        let cache = FileCache::new(8);
        let calls = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    assert_eq!(
                        cache.get_or_compute(&path, |_| {
                            calls.fetch_add(1, Ordering::Relaxed);
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            Some(42)
                        }),
                        Some(42)
                    );
                });
            }
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        cache.clear();
        let _ = std::panic::catch_unwind(|| {
            cache.get_or_compute(&path, |_| -> Option<i32> { panic!("decoder") })
        });
        assert_eq!(cache.get_or_compute(&path, |_| Some(7)), Some(7));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn metadata_prepares_its_disk_representation_only_once_on_a_ram_miss() {
        use crate::disk_cache::{Columns, DiskValue};
        use std::sync::atomic::{AtomicUsize, Ordering};

        static PREPARED: AtomicUsize = AtomicUsize::new(0);
        struct Counted;
        impl DiskValue for Counted {
            const SLOT: i32 = 99;

            fn from_row(_: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
                unreachable!("relative paths never enter SQLite")
            }

            fn columns(&self) -> Option<Columns<'_>> {
                PREPARED.fetch_add(1, Ordering::Relaxed);
                Some(Columns::default())
            }
        }

        let path = PathBuf::from(format!("cache-weight-{}.tmp", std::process::id()));
        fs::write(&path, b"fixture").unwrap();
        let cache = MetaCache::<Counted>::new();
        assert!(cache.get_or_read(&path, |_| Some(Counted)).is_some());
        assert!(cache
            .get_or_read(&path, |_| panic!("RAM hit must not read metadata"))
            .is_some());
        assert_eq!(PREPARED.load(Ordering::Relaxed), 1);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn metadata_retries_failed_reads_but_keeps_successful_empty_records() {
        let path = PathBuf::from(format!("cache-retry-{}.tmp", std::process::id()));
        std::fs::write(&path, b"fixture").unwrap();
        let cache = MetaCache::<crate::video_tags::VideoTags>::new();
        assert!(cache.get_or_read(&path, |_| None).is_none());
        assert!(!cache.contains(&path));
        assert!(cache
            .get_or_read(&path, |_| Some(Default::default()))
            .is_some());
        assert!(cache.contains(&path));
        assert!(cache
            .get_or_read(&path, |_| panic!(
                "successful empty metadata must be cached"
            ))
            .is_some());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn byte_budget_evicts_old_metadata_and_does_not_cache_oversized_values() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let a = dir.join("Cargo.toml");
        let b = dir.join("src/lib.rs");
        let cache = FileCache::weighted(8192, 4000, String::capacity);
        cache.get_or_compute(&a, |_| Some("a".repeat(2400)));
        cache.get_or_compute(&b, |_| Some("b".repeat(2400)));
        assert!(!cache.contains(&a));
        assert!(cache.contains(&b));
        assert!(cache.lock().bytes <= 4000);
        assert_eq!(
            cache
                .get_or_compute(&a, |_| Some("x".repeat(5000)))
                .unwrap()
                .len(),
            5000
        );
        assert!(!cache.contains(&a));
        assert!(cache.contains(&b));
    }

    #[test]
    fn least_recently_used_goes_first() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let (a, b, c) = (
            dir.join("Cargo.toml"),
            dir.join("src/lib.rs"),
            dir.join("src/cache.rs"),
        );
        let cache = FileCache::new(2);
        let computed = std::cell::Cell::new(0);
        let get = |p: &Path| {
            cache.get_or_compute(p, |_| {
                computed.set(computed.get() + 1);
                Some(computed.get())
            })
        };
        assert_eq!(get(&a), Some(1));
        assert_eq!(get(&b), Some(2));
        assert_eq!(get(&a), Some(1)); // hit: `a` is now the most recent
        assert_eq!(get(&c), Some(3)); // evicts `b`
        assert!(cache.contains(&a) && cache.contains(&c) && !cache.contains(&b));
        assert_eq!(cache.get_or_compute(&b, |_| None), None);
        assert!(!cache.contains(&b));
    }

    #[test]
    fn a_full_cache_drops_its_oldest_eighth() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .take(17)
            .collect();
        assert_eq!(files.len(), 17, "enough source files");
        let cache = FileCache::new(16);
        for (i, f) in files.iter().enumerate() {
            assert_eq!(cache.get_or_compute(f, |_| Some(i)), Some(i));
        }
        // 17 > 16: the three oldest (16 / 8 + 1) went, the rest stayed.
        assert!(files[..3].iter().all(|f| !cache.contains(f)));
        assert!(files[3..].iter().all(|f| cache.contains(f)));
    }

    #[test]
    fn scoped_key_is_used_for_its_file_only() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let (a, b) = (dir.join("Cargo.toml"), dir.join("src/lib.rs"));
        let real = FileKey::for_path(&a).expect("exists");
        with_file_key(&a, || {
            assert_eq!(FileKey::for_path(&a), Some(real.clone()));
            assert_eq!(FileKey::for_path(&b), FileKey::stat(&b));
        });
        assert!(SCOPED_KEY.with_borrow(Option::is_none));
        // A file that doesn't exist has no key, scoped or not.
        let gone = dir.join("no_such_file");
        with_file_key(&gone, || assert_eq!(FileKey::for_path(&gone), None));
    }

    #[test]
    fn changes_during_computation_are_not_cached_even_inside_a_scoped_key() {
        let path =
            std::env::temp_dir().join(format!("mediares-cache-change-{}", std::process::id()));
        fs::write(&path, b"old").unwrap();
        let cache = FileCache::new(2);
        with_file_key(&path, || {
            assert_eq!(
                cache.get_or_compute(&path, |_| {
                    fs::write(&path, b"changed size").unwrap();
                    Some(42)
                }),
                None
            );
        });
        assert!(!cache.contains(&path));
        assert_eq!(cache.get_or_compute(&path, |_| Some(43)), Some(43));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancelled_analysis_does_not_compute_or_enter_the_ram_cache() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let cache = MediaCache(FileCache::new(2));
        assert!(matches!(
            cache.get_or_analyze(&path, &|| true),
            CachedMedia::Unsupported
        ));
        assert!(!cache.is_cached(&path));
    }
}
