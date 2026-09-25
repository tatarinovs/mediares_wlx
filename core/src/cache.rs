//! MediaCache: in-memory LRU cache for image and video analysis results.

use lru::LruCache;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use crate::hashing::{analyze_dynamic_image, analyze_image, analyze_image_from_memory, ImageAnalysis};
use crate::probe::{probe_file, MediaType};
use crate::video::{analyze_video, VideoAnalysis};

#[derive(Debug, Clone)]
pub enum CachedMedia {
    Image(Arc<ImageAnalysis>),
    Video(Arc<VideoAnalysis>),
    Unsupported,
}

#[derive(Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    path: PathBuf,
    file_size: u64,
    mtime_nanos: u128,
}

pub struct MediaCache {
    cache: Mutex<LruCache<CacheKey, CachedMedia>>,
}

impl MediaCache {
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(512).unwrap());
        Self {
            cache: Mutex::new(LruCache::new(cap)),
        }
    }

    pub fn get_or_analyze(&self, path: &Path) -> CachedMedia {
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return CachedMedia::Unsupported,
        };

        let file_size = meta.len();
        let mtime_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let key = CacheKey {
            path: path.to_path_buf(),
            file_size,
            mtime_nanos,
        };

        {
            let mut lock = self.cache.lock().unwrap();
            if let Some(val) = lock.get(&key) {
                return val.clone();
            }
        }

        let media_type = probe_file(path);
        let result = match media_type {
            MediaType::StandardImage => match analyze_image(path) {
                Ok(a) => CachedMedia::Image(Arc::new(a)),
                Err(_) => CachedMedia::Unsupported,
            },
            MediaType::RawImage => {
                #[cfg(feature = "raw-preview")]
                {
                    if let Some(bytes) = crate::raw_preview::extract_raw_preview(path) {
                        if let Ok(a) = analyze_image_from_memory(&bytes) {
                            CachedMedia::Image(Arc::new(a))
                        } else {
                            CachedMedia::Unsupported
                        }
                    } else {
                        CachedMedia::Unsupported
                    }
                }
                #[cfg(not(feature = "raw-preview"))]
                CachedMedia::Unsupported
            }
            MediaType::PsdImage => {
                #[cfg(feature = "psd-preview")]
                {
                    if let Some(img) = crate::psd_preview::load_psd_image(path) {
                        let a = analyze_dynamic_image(&img);
                        CachedMedia::Image(Arc::new(a))
                    } else {
                        CachedMedia::Unsupported
                    }
                }
                #[cfg(not(feature = "psd-preview"))]
                CachedMedia::Unsupported
            }
            MediaType::Video => match analyze_video(path) {
                Ok(a) => CachedMedia::Video(Arc::new(a)),
                Err(_) => CachedMedia::Unsupported,
            },
            MediaType::Audio | MediaType::Unsupported => CachedMedia::Unsupported,
        };

        let mut lock = self.cache.lock().unwrap();
        lock.put(key, result.clone());
        result
    }

    pub fn is_cached(&self, path: &Path) -> bool {
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };

        let file_size = meta.len();
        let mtime_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let key = CacheKey {
            path: path.to_path_buf(),
            file_size,
            mtime_nanos,
        };

        let lock = self.cache.lock().unwrap();
        lock.contains(&key)
    }

    pub fn clear(&self) {
        if let Ok(mut lock) = self.cache.lock() {
            lock.clear();
        }
    }
}

static GLOBAL_CACHE: OnceLock<MediaCache> = OnceLock::new();

pub fn get_cache() -> &'static MediaCache {
    GLOBAL_CACHE.get_or_init(|| MediaCache::new(1024))
}
