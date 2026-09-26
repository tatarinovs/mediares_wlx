//! MediaCache: in-memory LRU cache of image, video and audio analysis results.

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use lru::LruCache;

use crate::hashing::{analyze, ImageAnalysis};
use crate::image_decode::decode_file;
use crate::probe::{probe_file, MediaType};
use crate::video_frame::{analyze_video, probe_video_meta, VideoAnalysis, VideoError, VideoMeta};

const CAPACITY: usize = 1024;

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

pub struct MediaCache {
    cache: Mutex<LruCache<FileKey, CachedMedia>>,
}

impl MediaCache {
    fn new(capacity: NonZeroUsize) -> Self {
        Self {
            cache: Mutex::new(LruCache::new(capacity)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LruCache<FileKey, CachedMedia>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns the cached analysis or computes it. A cancelled analysis is returned as
    /// `Unsupported` but not cached, so the next request recomputes it.
    pub fn get_or_analyze(&self, path: &Path, cancelled: &dyn Fn() -> bool) -> CachedMedia {
        let Some(key) = FileKey::for_path(path) else {
            return CachedMedia::Unsupported;
        };
        if let Some(hit) = self.lock().get(&key) {
            return hit.clone();
        }

        let result = match probe_file(path) {
            kind if kind.is_image_kind() => decode_file(path, kind)
                .map(|img| CachedMedia::Image(Arc::new(analyze(&img))))
                .unwrap_or(CachedMedia::Unsupported),
            MediaType::Video => match analyze_video(path, cancelled) {
                Ok(v) => CachedMedia::Video(Arc::new(v)),
                Err(VideoError::Cancelled) => return CachedMedia::Unsupported,
                Err(VideoError::Failed(_)) => CachedMedia::Unsupported,
            },
            #[cfg(feature = "audio-decode")]
            MediaType::Audio => match crate::audio_fingerprint::analyze_audio(path, cancelled) {
                Ok(a) => CachedMedia::Audio(Arc::new(a)),
                Err(crate::audio_fingerprint::AudioError::Cancelled) => {
                    return CachedMedia::Unsupported
                }
                Err(crate::audio_fingerprint::AudioError::Unsupported) => CachedMedia::Unsupported,
            },
            _ => CachedMedia::Unsupported,
        };

        self.lock().put(key, result.clone());
        result
    }

    pub fn is_cached(&self, path: &Path) -> bool {
        FileKey::for_path(path).is_some_and(|key| self.lock().contains(&key))
    }

    pub fn clear(&self) {
        self.lock().clear();
    }
}

/// Cheap per-file metadata (tags, EXIF, stream properties) keyed by file version: TC asks for
/// every column separately, and each file is read once. Failed reads are cached too.
struct MetaCache<T>(Mutex<LruCache<FileKey, Option<Arc<T>>>>);

impl<T> MetaCache<T> {
    fn new() -> Self {
        MetaCache(Mutex::new(LruCache::new(
            NonZeroUsize::new(512).expect("non-zero capacity"),
        )))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LruCache<FileKey, Option<Arc<T>>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get_or_read(&self, path: &Path, read: impl FnOnce(&Path) -> Option<T>) -> Option<Arc<T>> {
        let key = FileKey::for_path(path)?;
        if let Some(hit) = self.lock().get(&key) {
            return hit.clone();
        }
        let value = read(path).map(Arc::new);
        self.lock().put(key, value.clone());
        value
    }

    fn contains(&self, path: &Path) -> bool {
        FileKey::for_path(path).is_some_and(|key| self.lock().contains(&key))
    }
}

/// Audio tags without pictures.
#[cfg(feature = "tags")]
pub fn get_tags(path: &Path) -> Option<Arc<crate::audio_tags::AudioTags>> {
    static TAGS: OnceLock<MetaCache<crate::audio_tags::AudioTags>> = OnceLock::new();
    TAGS.get_or_init(MetaCache::new)
        .get_or_read(path, |p| crate::audio_tags::read_tags(p, false))
}

pub fn get_exif(path: &Path) -> Option<Arc<crate::exif::ExifInfo>> {
    static EXIF: OnceLock<MetaCache<crate::exif::ExifInfo>> = OnceLock::new();
    EXIF.get_or_init(MetaCache::new)
        .get_or_read(path, crate::exif::read_exif)
}

fn video_meta_cache() -> &'static MetaCache<VideoMeta> {
    static VIDEO: OnceLock<MetaCache<VideoMeta>> = OnceLock::new();
    VIDEO.get_or_init(MetaCache::new)
}

/// Stream properties via Media Foundation: no decoding, but opening the source still takes a while.
pub fn get_video_meta(path: &Path) -> Option<Arc<VideoMeta>> {
    video_meta_cache().get_or_read(path, probe_video_meta)
}

pub fn is_video_meta_cached(path: &Path) -> bool {
    video_meta_cache().contains(path)
}

fn audio_meta_cache() -> &'static MetaCache<crate::mf_audio::AudioStreamMeta> {
    static AUDIO: OnceLock<MetaCache<crate::mf_audio::AudioStreamMeta>> = OnceLock::new();
    AUDIO.get_or_init(MetaCache::new)
}

/// Audio stream properties via Media Foundation, for files lofty can't parse (WMA, AC3...).
pub fn get_audio_meta(path: &Path) -> Option<Arc<crate::mf_audio::AudioStreamMeta>> {
    audio_meta_cache().get_or_read(path, crate::mf_audio::probe_audio_meta)
}

pub fn is_audio_meta_cached(path: &Path) -> bool {
    audio_meta_cache().contains(path)
}

/// Title, artist... of MKV/WebM and MP4/MOV: header reads only.
pub fn get_video_tags(path: &Path) -> Option<Arc<crate::video_tags::VideoTags>> {
    static VIDEO_TAGS: OnceLock<MetaCache<crate::video_tags::VideoTags>> = OnceLock::new();
    VIDEO_TAGS
        .get_or_init(MetaCache::new)
        .get_or_read(path, |p| {
            Some(crate::video_tags::read_video_tags(p)).filter(|t| !t.is_empty())
        })
}

pub fn get_cache() -> &'static MediaCache {
    static GLOBAL_CACHE: OnceLock<MediaCache> = OnceLock::new();
    GLOBAL_CACHE
        .get_or_init(|| MediaCache::new(NonZeroUsize::new(CAPACITY).expect("non-zero capacity")))
}
