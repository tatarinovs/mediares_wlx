//! Media type detection by file extension — the single source of truth for supported formats.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Decoded by the `image` crate.
const STANDARD_IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "jpe", "thm", "png", "gif", "webp", "bmp", "tiff", "tif", "ico", "tga", "hdr",
];
/// Decoded by Windows Imaging Component (HEIF/AV1/JPEG XL extensions, built-in JPEG XR and DDS).
pub const WIC_IMAGE_EXTS: &[&str] = &[
    "heic", "heif", "hif", "avif", "jxl", "jxr", "wdp", "hdp", "dds",
];
/// Rendered by Direct2D.
pub const SVG_EXTS: &[&str] = &["svg", "svgz"];
#[cfg(feature = "exr")]
const EXR_EXTS: &[&str] = &["exr"];
#[cfg(not(feature = "exr"))]
const EXR_EXTS: &[&str] = &[];

/// Every still-image extension shown as `StandardImage`, whatever decodes it.
fn standard_image_exts() -> &'static [&'static str] {
    static ALL: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| [STANDARD_IMAGE_EXTS, WIC_IMAGE_EXTS, SVG_EXTS, EXR_EXTS].concat())
}

/// Whether the extension of `path` is one of `exts` (ASCII case-insensitive).
pub fn has_extension(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| exts.iter().any(|e| e.eq_ignore_ascii_case(ext)))
}
const RAW_EXTS: &[&str] = &[
    "cr2", "cr3", "crw", "nef", "arw", "orf", "rw2", "dng", "raf", "pef", "raw",
];
const PSD_EXTS: &[&str] = &["psd", "psb"];
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "qt", "wmv", "asf", "webm", "m4v", "3gp", "3g2", "flv", "ts",
    "mts", "mpg", "mpeg", "vob",
];
const AUDIO_EXTS: &[&str] = &[
    "mp3", "mp2", "flac", "wav", "ogg", "oga", "opus", "m4a", "m4b", "aac", "wma", "aif", "aiff",
    "aifc", "caf", "mka", "ac3",
];
/// Video and audio only libmpv plays (FFmpeg demuxers and decoders); recognized once it is
/// found, see [`enable_mpv_formats`]. Fixed for a full build (shinchiro / zhongfly): asking the
/// DLL itself costs about a second at TC's start, and a build missing a decoder just reports
/// the file as unsupported.
const MPV_VIDEO_EXTS: &[&str] = &[
    "m2ts", "m2t", "rm", "rmvb", "ogv", "divx", "f4v", "mxf", "y4m", "dav", "nut", "dv",
];
const MPV_AUDIO_EXTS: &[&str] = &[
    "ape", "wv", "dsf", "dff", "tta", "mpc", "tak", "dts", "eac3", "thd", "mlp", "spx", "shn",
    "w64", "amr", "au", "ra", // tracker music through libopenmpt:
    "mod", "xm", "it", "s3m", "mptm",
];
const PLAYLIST_EXTS: &[&str] = &["m3u", "m3u8"];

static MPV_FORMATS: AtomicBool = AtomicBool::new(false);

/// libmpv is installed: the formats only it plays are recognized from now on.
pub fn enable_mpv_formats() {
    MPV_FORMATS.store(true, Ordering::Relaxed);
}

/// One of the formats recognized only because libmpv is installed: Media Foundation either
/// can't open it or (through a third-party source) reports the decoded PCM instead of the codec.
pub fn is_mpv_only(path: &Path) -> bool {
    has_extension(path, MPV_VIDEO_EXTS) || has_extension(path, MPV_AUDIO_EXTS)
}

fn video_exts() -> &'static [&'static str] {
    static WITH_MPV: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    if MPV_FORMATS.load(Ordering::Relaxed) {
        WITH_MPV.get_or_init(|| [VIDEO_EXTS, MPV_VIDEO_EXTS].concat())
    } else {
        VIDEO_EXTS
    }
}

fn audio_exts() -> &'static [&'static str] {
    static WITH_MPV: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    if MPV_FORMATS.load(Ordering::Relaxed) {
        WITH_MPV.get_or_init(|| [AUDIO_EXTS, MPV_AUDIO_EXTS].concat())
    } else {
        AUDIO_EXTS
    }
}

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
            MediaType::StandardImage => standard_image_exts(),
            MediaType::RawImage => RAW_EXTS,
            MediaType::PsdImage => PSD_EXTS,
            MediaType::Video => video_exts(),
            MediaType::Audio => audio_exts(),
            MediaType::Playlist => PLAYLIST_EXTS,
            MediaType::Unsupported => &[],
        }
    }

    pub fn is_image_kind(self) -> bool {
        matches!(
            self,
            MediaType::StandardImage | MediaType::RawImage | MediaType::PsdImage
        )
    }

    /// Audio or video: something with a timeline that can be played through.
    pub fn is_playable(self) -> bool {
        matches!(self, MediaType::Audio | MediaType::Video)
    }

    /// Kinds that are expensive to analyze and should be deferred with `FT_DELAYED`.
    pub fn is_slow_kind(self) -> bool {
        matches!(
            self,
            MediaType::Video | MediaType::Audio | MediaType::RawImage | MediaType::PsdImage
        )
    }
}

pub fn probe_file(path: &Path) -> MediaType {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return MediaType::Unsupported;
    };
    MediaType::ALL
        .into_iter()
        .find(|kind| {
            kind.extensions()
                .iter()
                .any(|e| e.eq_ignore_ascii_case(ext))
        })
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
        assert_eq!(
            probe_file(Path::new("IMG_1.HEIC")),
            MediaType::StandardImage
        );
        assert_eq!(probe_file(Path::new("t.tga")), MediaType::StandardImage);
    }

    #[test]
    fn detect_string_lists_every_extension() {
        let s = detect_extensions(&[MediaType::RawImage, MediaType::PsdImage]);
        for ext in RAW_EXTS.iter().chain(PSD_EXTS) {
            assert!(s.contains(&format!("EXT=\"{}\"", ext.to_ascii_uppercase())));
        }
    }
}
