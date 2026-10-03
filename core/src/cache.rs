//! MediaCache: in-memory LRU cache of image, video and audio analysis results.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use crate::hashing::{analyze, ImageAnalysis};
use crate::image_decode::decode_file;
use crate::probe::{probe_file, MediaType};
use crate::video_frame::{analyze_video, probe_video_meta, VideoAnalysis, VideoError, VideoMeta};

const CAPACITY: usize = 1024;
const META_CAPACITY: usize = 512;

#[derive(Debug, Clone)]
pub enum CachedMedia {
    Image(Arc<ImageAnalysis>),
    Video(Arc<VideoAnalysis>),
    Audio(Arc<AudioAnalysis>),
    Unsupported,
}

/// Duplicate-detection fields of an audio file (see `audio_fingerprint`).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioAnalysis {
    pub duration_sec: u32,
    /// Hash of the decoded samples (hex).
    pub pcm_hash: String,
    /// `None` for (nearly) silent files.
    pub fingerprint: Option<String>,
}

/// Identifies a specific version of a file: edits change size or mtime and miss the cache.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct FileKey {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

impl FileKey {
    pub fn for_path(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Some(FileKey {
            path: path.to_path_buf(),
            size: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// Least-recently-used map from file versions to values, shared between threads.
struct FileCache<V> {
    inner: Mutex<Lru<V>>,
}

struct Lru<V> {
    /// Value and the tick it was last used at.
    map: HashMap<FileKey, (V, u64)>,
    clock: u64,
    capacity: usize,
}

impl<V: Clone> FileCache<V> {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Lru {
                map: HashMap::new(),
                clock: 0,
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
            lru.clock += 1;
            let now = lru.clock;
            if let Some((value, used)) = lru.map.get_mut(&key) {
                *used = now;
                return Some(value.clone());
            }
        }
        let value = compute(path)?;
        let mut lru = self.lock();
        lru.clock += 1;
        let now = lru.clock;
        lru.map.insert(key, (value.clone(), now));
        if lru.map.len() > lru.capacity {
            // Only when full: a scan over at most `capacity` entries.
            let oldest = lru
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                lru.map.remove(&oldest);
            }
        }
        Some(value)
    }

    fn contains(&self, path: &Path) -> bool {
        FileKey::for_path(path).is_some_and(|key| self.lock().map.contains_key(&key))
    }

    fn clear(&self) {
        self.lock().map.clear();
    }
}

pub struct MediaCache(FileCache<CachedMedia>);

impl MediaCache {
    /// Returns the cached analysis or computes it. A cancelled analysis is returned as
    /// `Unsupported` but not cached, so the next request recomputes it.
    pub fn get_or_analyze(&self, path: &Path, cancelled: &dyn Fn() -> bool) -> CachedMedia {
        self.0
            .get_or_compute(path, |path| analyze_file(path, cancelled))
            .unwrap_or(CachedMedia::Unsupported)
    }

    pub fn is_cached(&self, path: &Path) -> bool {
        self.0.contains(path)
    }

    pub fn clear(&self) {
        self.0.clear();
    }
}

/// `None` if the host cancelled the analysis.
fn analyze_file(path: &Path, cancelled: &dyn Fn() -> bool) -> Option<CachedMedia> {
    Some(match probe_file(path) {
        kind if kind.is_image_kind() => decode_file(path, kind)
            .map(|img| CachedMedia::Image(Arc::new(analyze(&img))))
            .unwrap_or(CachedMedia::Unsupported),
        MediaType::Video => match analyze_video(path, cancelled) {
            Ok(v) => CachedMedia::Video(Arc::new(v)),
            Err(VideoError::Cancelled) => return None,
            Err(VideoError::Failed(_)) => CachedMedia::Unsupported,
        },
        #[cfg(feature = "audio-decode")]
        MediaType::Audio => match crate::audio_fingerprint::analyze_audio(path, cancelled) {
            Ok(a) => CachedMedia::Audio(Arc::new(a)),
            Err(crate::audio_fingerprint::AudioError::Cancelled) => return None,
            Err(crate::audio_fingerprint::AudioError::Unsupported) => CachedMedia::Unsupported,
        },
        _ => CachedMedia::Unsupported,
    })
}

/// Cheap per-file metadata (tags, EXIF, stream properties) keyed by file version: TC asks for
/// every column separately, and each file is read once. Failed reads are cached too.
struct MetaCache<T>(FileCache<Option<Arc<T>>>);

impl<T> MetaCache<T> {
    fn new() -> Self {
        MetaCache(FileCache::new(META_CAPACITY))
    }

    fn get_or_read(&self, path: &Path, read: impl FnOnce(&Path) -> Option<T>) -> Option<Arc<T>> {
        self.0
            .get_or_compute(path, |p| Some(read(p).map(Arc::new)))
            .flatten()
    }

    fn contains(&self, path: &Path) -> bool {
        self.0.contains(path)
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

pub fn get_exif(path: &Path) -> Option<Arc<crate::exif::ExifInfo>> {
    static EXIF: OnceLock<MetaCache<crate::exif::ExifInfo>> = OnceLock::new();
    EXIF.get_or_init(MetaCache::new)
        .get_or_read(path, crate::exif::read_exif)
}

fn codec_size_cache() -> &'static MetaCache<(u32, u32)> {
    static SIZES: OnceLock<MetaCache<(u32, u32)>> = OnceLock::new();
    SIZES.get_or_init(MetaCache::new)
}

/// Size of a picture whose header is read by a system codec (see `header_needs_codec`): cheap
/// once the codec is loaded, but not free like a JPEG header.
pub fn get_codec_image_size(path: &Path) -> Option<(u32, u32)> {
    codec_size_cache()
        .get_or_read(path, crate::image_decode::header_dimensions)
        .map(|s| *s)
}

pub fn is_codec_image_size_cached(path: &Path) -> bool {
    codec_size_cache().contains(path)
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
    video_tags_cache().get_or_read(path, |p| {
        Some(crate::video_tags::read_video_tags(p)).filter(|t| !t.is_empty())
    })
}

pub fn is_video_tags_cached(path: &Path) -> bool {
    video_tags_cache().contains(path)
}

pub fn get_cache() -> &'static MediaCache {
    static GLOBAL_CACHE: OnceLock<MediaCache> = OnceLock::new();
    GLOBAL_CACHE.get_or_init(|| MediaCache(FileCache::new(CAPACITY)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
