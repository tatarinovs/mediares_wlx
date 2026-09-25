//! MediaCache: in-memory LRU cache of image and video analysis results.

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
    Unsupported,
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

pub fn get_cache() -> &'static MediaCache {
    static GLOBAL_CACHE: OnceLock<MediaCache> = OnceLock::new();
    GLOBAL_CACHE.get_or_init(|| MediaCache::new(NonZeroUsize::new(CAPACITY).expect("non-zero capacity")))
}
