//! Media type detection by file extension — the single source of truth for supported formats.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    StandardImage,
    RawImage,
    PsdImage,
    Video,
    Audio,
    /// M3U / M3U8 list of media files (opened by the viewer as a play queue).
    Playlist,
    Unsupported,
}

const STANDARD_IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff", "tif", "ico"];
const RAW_EXTS: &[&str] = &["cr2", "cr3", "nef", "arw", "orf", "rw2", "dng", "raf", "pef"];
const PSD_EXTS: &[&str] = &["psd", "psb"];
const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "avi", "mov", "wmv", "webm", "m4v", "flv", "ts", "mts"];
const AUDIO_EXTS: &[&str] = &[
    "mp3", "mp2", "flac", "wav", "ogg", "oga", "opus", "m4a", "m4b", "aac", "wma", "aif", "aiff", "caf", "mka",
];
const PLAYLIST_EXTS: &[&str] = &["m3u", "m3u8"];

impl MediaType {
    const ALL: [MediaType; 6] = [
        MediaType::StandardImage,
        MediaType::RawImage,
        MediaType::PsdImage,
        MediaType::Video,
        MediaType::Audio,
        MediaType::Playlist,
    ];

    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            MediaType::StandardImage => STANDARD_IMAGE_EXTS,
            MediaType::RawImage => RAW_EXTS,
            MediaType::PsdImage => PSD_EXTS,
            MediaType::Video => VIDEO_EXTS,
            MediaType::Audio => AUDIO_EXTS,
            MediaType::Playlist => PLAYLIST_EXTS,
            MediaType::Unsupported => &[],
        }
    }

    pub fn is_image_kind(self) -> bool {
        matches!(self, MediaType::StandardImage | MediaType::RawImage | MediaType::PsdImage)
    }

    /// Audio or video: something with a timeline that can be played through.
    pub fn is_playable(self) -> bool {
        matches!(self, MediaType::Audio | MediaType::Video)
    }

    /// Kinds that are expensive to analyze and should be deferred with `FT_DELAYED`.
    pub fn is_slow_kind(self) -> bool {
        matches!(self, MediaType::Video | MediaType::Audio | MediaType::RawImage | MediaType::PsdImage)
    }
}

pub fn probe_file(path: &Path) -> MediaType {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return MediaType::Unsupported;
    };
    MediaType::ALL
        .into_iter()
        .find(|kind| kind.extensions().iter().any(|e| e.eq_ignore_ascii_case(ext)))
        .unwrap_or(MediaType::Unsupported)
}

/// Builds a TC detect-string fragment `EXT="JPG" | EXT="JPEG" | ...` for the given kinds.
pub fn detect_extensions(kinds: &[MediaType]) -> String {
    kinds
        .iter()
        .flat_map(|k| k.extensions())
        .map(|e| format!("EXT=\"{}\"", e.to_ascii_uppercase()))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_is_case_insensitive() {
        assert_eq!(probe_file(Path::new("a/B.JpG")), MediaType::StandardImage);
        assert_eq!(probe_file(Path::new("x.PSB")), MediaType::PsdImage);
        assert_eq!(probe_file(Path::new("x.cr3")), MediaType::RawImage);
        assert_eq!(probe_file(Path::new("noext")), MediaType::Unsupported);
        assert_eq!(probe_file(Path::new("x.FLAC")), MediaType::Audio);
        assert_eq!(probe_file(Path::new("list.m3u8")), MediaType::Playlist);
    }

    #[test]
    fn detect_string_lists_every_extension() {
        let s = detect_extensions(&[MediaType::RawImage, MediaType::PsdImage]);
        for ext in RAW_EXTS.iter().chain(PSD_EXTS) {
            assert!(s.contains(&format!("EXT=\"{}\"", ext.to_ascii_uppercase())));
        }
    }
}
