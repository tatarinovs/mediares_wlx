//! Media type detection by file extension and signatures.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    StandardImage,
    RawImage,
    PsdImage,
    Video,
    Audio,
    Unsupported,
}

impl MediaType {
    pub fn is_image_kind(self) -> bool {
        matches!(self, MediaType::StandardImage | MediaType::RawImage | MediaType::PsdImage)
    }

    pub fn is_video_kind(self) -> bool {
        matches!(self, MediaType::Video)
    }

    pub fn is_audio_kind(self) -> bool {
        matches!(self, MediaType::Audio)
    }
}

pub fn probe_file(path: &Path) -> MediaType {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "ico" => {
            MediaType::StandardImage
        }
        "cr2" | "cr3" | "nef" | "arw" | "orf" | "rw2" | "dng" | "raf" | "pef" => {
            MediaType::RawImage
        }
        "psd" | "psb" => MediaType::PsdImage,
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "flv" | "ts" | "mts" => {
            MediaType::Video
        }
        "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" | "aac" | "wma" => MediaType::Audio,
        _ => MediaType::Unsupported,
    }
}
