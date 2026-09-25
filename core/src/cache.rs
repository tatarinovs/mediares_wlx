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
use crate::video_frame::{analyze_video, VideoAnalysis, VideoError};

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
        Some(FileKey { path: path.to_path_buf(), size: meta.len(), modified: meta.modified().ok() })
    }
}

pub struct MediaCache {
    cache: Mutex<LruCache<FileKey, CachedMedia>>,
}

impl MediaCache {
    fn new(capacity: NonZeroUsize) -> Self {
        Self { cache: Mutex::new(LruCache::new(capacity)) }
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
                Err(crate::audio_fingerprint::AudioError::Cancelled) => return CachedMedia::Unsupported,
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

/// Tags of recently queried audio files: TC asks for every column separately, and the tags are
/// read once per file version. Pictures are not kept.
#[cfg(feature = "tags")]
pub fn get_tags(path: &Path) -> Option<Arc<crate::audio_tags::AudioTags>> {
    type TagCache = Mutex<LruCache<FileKey, Option<Arc<crate::audio_tags::AudioTags>>>>;
    static TAGS: OnceLock<TagCache> = OnceLock::new();
    let cache = TAGS.get_or_init(|| Mutex::new(LruCache::new(NonZeroUsize::new(512).expect("non-zero capacity"))));
    let key = FileKey::for_path(path)?;
    if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return hit.clone();
    }
    let tags = crate::audio_tags::read_tags(path, false).map(Arc::new);
    cache.lock().unwrap_or_else(|e| e.into_inner()).put(key, tags.clone());
    tags
}

pub fn get_cache() -> &'static MediaCache {
    static GLOBAL_CACHE: OnceLock<MediaCache> = OnceLock::new();
    GLOBAL_CACHE.get_or_init(|| MediaCache::new(NonZeroUsize::new(CAPACITY).expect("non-zero capacity")))
}
