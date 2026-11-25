//! Total Commander Content Plugin (WDX) implementation shared by the `wdx` and `combo` crates.
//!
//! Both crates expose it through [`export_content_plugin!`], so the FFI surface is defined once.

use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::cache::{get_cache, CachedMedia};
use crate::ffi::{pstr_to_path, pwstr_to_path, write_ansi, write_wide};
use crate::probe::{detect_extensions, probe_file, MediaType};
use crate::tc_api::*;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn is_stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::Relaxed)
}

#[inline]
pub fn reset_stop_flag() {
    STOP_REQUESTED.store(false, Ordering::Relaxed);
}

/// The work needed to obtain a field, used to keep slow reads off TC's UI thread.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Constant,
    Probe,
    PartialHash,
    Tags,
    Exif,
    ImageSize,
    ImageOrientation,
    ImageExtra,
    PixelFormat,
    Sharpness,
    VideoMeta,
    VideoTags,
    AudioAnalysis,
    AudioFastFingerprint,
    AudioDuration,
    Analysis,
}

#[derive(Clone, Copy)]
enum Scope {
    Any,
    Media,
    Image,
    StandardImage,
    Video,
    Audio,
}
impl Scope {
    fn accepts(self, kind: MediaType) -> bool {
        match self {
            Self::Any => true,
            Self::Media => kind.is_image_kind() || kind.is_playable(),
            Self::Image => kind.is_image_kind(),
            Self::StandardImage => kind == MediaType::StandardImage,
            Self::Video => kind == MediaType::Video,
            Self::Audio => kind == MediaType::Audio,
        }
    }
}

// Name, ABI type, units, data source and applicable media kinds have one definition.
macro_rules! fields {
    ($( $field:ident, $name:literal, $kind:ident, $units:literal, $source:ident, $scope:ident; )*) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub(crate) enum Field { $($field,)* }
        const FIELDS: &[(Field, &str, c_int, &str)] = &[$((Field::$field, $name, $kind, $units),)*];
        impl Field {
            pub(crate) fn source(self) -> Source { match self { $(Self::$field => Source::$source,)* } }
            pub(crate) fn applies_to(self, kind: MediaType) -> bool { match self { $(Self::$field => Scope::$scope.accepts(kind),)* } }
        }
    };
}

fields! {
    ImageDHash, "Image_dHash", FT_STRINGW, "", Analysis, Image;
    ImagePHash, "Image_pHash", FT_STRINGW, "", Analysis, Image;
    ImageCoarseHash, "Image_CoarseHash", FT_STRINGW, "", Analysis, Image;
    VideoFingerprint, "Video_Fingerprint", FT_STRINGW, "", Analysis, Video;
    VideoDHashMid, "Video_dHash_Mid", FT_STRINGW, "", Analysis, Video;
    AudioFingerprint, "Audio_Fingerprint", FT_STRINGW, "", AudioAnalysis, Audio;
    AudioFastFingerprint, "Audio_Fast_Fingerprint", FT_STRINGW, "", AudioFastFingerprint, Audio;
    AudioPcmHash, "Audio_PCM_Hash", FT_STRINGW, "", AudioAnalysis, Audio;
    FilePartialHash, "File_Partial_Hash", FT_STRINGW, "", PartialHash, Media;
    MediaTypeName, "Media_Type", FT_STRINGW, "", Probe, Media;
    ImageWidth, "Image_Width", FT_NUMERIC_32, "", ImageSize, Image;
    ImageHeight, "Image_Height", FT_NUMERIC_32, "", ImageSize, Image;
    ImageMegapixels, "Image_Megapixels", FT_NUMERIC_FLOATING, "", ImageSize, Image;
    ImageDimensions, "Image_Dimensions", FT_STRINGW, "", ImageSize, Image;
    ImageAspectRatio, "Image_AspectRatio", FT_STRINGW, "Ratio|Decimal", ImageSize, Image;
    ImageOrientation, "Image_Orientation", FT_STRINGW, "Text|Short|Angle|EXIF", ImageOrientation, Image;
    ImageBitDepth, "Image_Bit_Depth", FT_NUMERIC_32, "", PixelFormat, StandardImage;
    ImageHasAlpha, "Image_Has_Alpha", FT_BOOLEAN, "", PixelFormat, StandardImage;
    ImageSharpness, "Image_Sharpness", FT_NUMERIC_32, "", Sharpness, Image;
    ImageDpi, "Image_DPI", FT_NUMERIC_32, "", ImageExtra, Image;
    ImageAiParameters, "Image_AI_Parameters", FT_STRINGW, "", ImageExtra, StandardImage;
    PhotoMake, "Photo_Make", FT_STRINGW, "", Exif, Image;
    PhotoModel, "Photo_Model", FT_STRINGW, "", Exif, Image;
    PhotoLens, "Photo_Lens", FT_STRINGW, "", Exif, Image;
    PhotoDateTaken, "Photo_Date_Taken", FT_DATETIME, "", Exif, Image;
    PhotoExposure, "Photo_Exposure", FT_STRINGW, "", Exif, Image;
    PhotoFNumber, "Photo_FNumber", FT_NUMERIC_FLOATING, "", Exif, Image;
    PhotoIso, "Photo_ISO", FT_NUMERIC_32, "", Exif, Image;
    PhotoFocalLength, "Photo_Focal_Length_mm", FT_NUMERIC_FLOATING, "", Exif, Image;
    PhotoFocalLength35, "Photo_Focal_Length_35mm", FT_NUMERIC_32, "", Exif, Image;
    PhotoFlash, "Photo_Flash", FT_BOOLEAN, "", Exif, Image;
    PhotoOrientation, "Photo_Orientation", FT_NUMERIC_32, "", Exif, Image;
    PhotoSoftware, "Photo_Software", FT_STRINGW, "", Exif, Image;
    PhotoGpsLatitude, "Photo_GPS_Latitude", FT_NUMERIC_FLOATING, "", Exif, Image;
    PhotoGpsLongitude, "Photo_GPS_Longitude", FT_NUMERIC_FLOATING, "", Exif, Image;
    PhotoGps, "Photo_GPS", FT_STRINGW, "Decimal|DMS|DMM", Exif, Image;
    PhotoHasGps, "Photo_Has_GPS", FT_BOOLEAN, "", Exif, Image;
    VideoDurationSec, "Video_Duration_Sec", FT_NUMERIC_32, "", VideoMeta, Video;
    VideoLength, "Video_Length", FT_TIME, "", VideoMeta, Video;
    VideoWidth, "Video_Width", FT_NUMERIC_32, "", VideoMeta, Video;
    VideoHeight, "Video_Height", FT_NUMERIC_32, "", VideoMeta, Video;
    VideoDimensions, "Video_Dimensions", FT_STRINGW, "", VideoMeta, Video;
    VideoOrientation, "Video_Orientation", FT_STRINGW, "Text|Short|Angle", VideoMeta, Video;
    VideoFrameRate, "Video_Frame_Rate", FT_NUMERIC_FLOATING, "", VideoMeta, Video;
    VideoCodec, "Video_Codec", FT_STRINGW, "", VideoMeta, Video;
    VideoContainer, "Video_Container", FT_STRINGW, "", VideoMeta, Video;
    VideoDynamicRange, "Video_HDR", FT_STRINGW, "", VideoMeta, Video;
    VideoRotation, "Video_Rotation", FT_NUMERIC_32, "", VideoMeta, Video;
    VideoBitrate, "Video_Bitrate_kbps", FT_NUMERIC_32, "", VideoMeta, Video;
    VideoAudioCodec, "Video_Audio_Codec", FT_STRINGW, "", VideoMeta, Video;
    VideoAudioChannels, "Video_Audio_Channels", FT_NUMERIC_32, "", VideoMeta, Video;
    VideoAudioSampleRate, "Video_Audio_Sample_Rate_Hz", FT_NUMERIC_32, "", VideoMeta, Video;
    VideoAudioStreams, "Video_Audio_Streams", FT_NUMERIC_32, "", VideoMeta, Video;
    VideoAudioLang, "Video_Audio_Lang", FT_STRINGW, "", VideoMeta, Video;
    VideoSummary, "Video_Summary", FT_STRINGW, "", VideoMeta, Video;
    VideoTitle, "Video_Title", FT_STRINGW, "", VideoTags, Video;
    VideoArtist, "Video_Artist", FT_STRINGW, "", VideoTags, Video;
    VideoDirector, "Video_Director", FT_STRINGW, "", VideoTags, Video;
    VideoDate, "Video_Date", FT_STRINGW, "", VideoTags, Video;
    VideoYear, "Video_Year", FT_NUMERIC_32, "", VideoTags, Video;
    VideoGenre, "Video_Genre", FT_STRINGW, "", VideoTags, Video;
    VideoComment, "Video_Comment", FT_STRINGW, "", VideoTags, Video;
    AudioDurationSec, "Audio_Duration_Sec", FT_NUMERIC_32, "", AudioDuration, Audio;
    AudioLength, "Audio_Length", FT_TIME, "", Tags, Audio;
    AudioArtistTitle, "Audio_Artist_Title", FT_STRINGW, "", Tags, Audio;
    AudioArtist, "Audio_Artist", FT_STRINGW, "", Tags, Audio;
    AudioTitle, "Audio_Title", FT_STRINGW, "", Tags, Audio;
    AudioAlbum, "Audio_Album", FT_STRINGW, "", Tags, Audio;
    AudioAlbumArtist, "Audio_Album_Artist", FT_STRINGW, "", Tags, Audio;
    AudioComposer, "Audio_Composer", FT_STRINGW, "", Tags, Audio;
    AudioGenre, "Audio_Genre", FT_STRINGW, "", Tags, Audio;
    AudioYear, "Audio_Year", FT_NUMERIC_32, "", Tags, Audio;
    AudioTrack, "Audio_Track", FT_NUMERIC_32, "", Tags, Audio;
    AudioTrackTotal, "Audio_Track_Total", FT_NUMERIC_32, "", Tags, Audio;
    AudioDisc, "Audio_Disc", FT_NUMERIC_32, "", Tags, Audio;
    AudioDiscTotal, "Audio_Disc_Total", FT_NUMERIC_32, "", Tags, Audio;
    AudioComment, "Audio_Comment", FT_STRINGW, "", Tags, Audio;
    AudioHasCover, "Audio_Has_Cover", FT_BOOLEAN, "", Tags, Audio;
    AudioCodec, "Audio_Codec", FT_STRINGW, "", Tags, Audio;
    AudioLossless, "Audio_Lossless", FT_BOOLEAN, "", Tags, Audio;
    AudioReplayGain, "Audio_ReplayGain_dB", FT_NUMERIC_FLOATING, "", Tags, Audio;
    AudioBitrate, "Audio_Bitrate_kbps", FT_NUMERIC_32, "", Tags, Audio;
    AudioSampleRate, "Audio_Sample_Rate_Hz", FT_NUMERIC_32, "", Tags, Audio;
    AudioChannels, "Audio_Channels", FT_NUMERIC_32, "", Tags, Audio;
    AudioBitDepth, "Audio_Bit_Depth", FT_NUMERIC_32, "", Tags, Audio;
    AudioSummary, "Audio_Summary", FT_STRINGW, "", Tags, Audio;
    PluginVersion, "Plugin_Version", FT_STRINGW, "", Constant, Any;
}

fn field_at(index: c_int) -> Option<&'static (Field, &'static str, c_int, &'static str)> {
    FIELDS.get(usize::try_from(index).ok()?)
}

/// Kinds the WDX analyzes (audio only when built with the decoder).
pub fn wdx_detect_string() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| {
        let mut kinds = vec![
            MediaType::StandardImage,
            MediaType::RawImage,
            MediaType::PsdImage,
            MediaType::Video,
        ];
        if cfg!(feature = "audio-decode") {
            kinds.push(MediaType::Audio);
        }
        detect_extensions(&kinds)
    })
}

/// # Safety
/// `field_name` and `units` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_supported_field(
    field_index: c_int,
    field_name: *mut c_char,
    units: *mut c_char,
    max_len: c_int,
) -> c_int {
    let Some(&(_, name, field_type, field_units)) = field_at(field_index) else {
        return FT_NOMOREFIELDS;
    };
    let max_len = max_len.max(0) as usize;
    write_ansi(field_name, max_len, name);
    write_ansi(units, max_len, field_units);
    field_type
}

/// # Safety
/// `file_name` must be null or NUL-terminated; `field_value` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_value_w(
    file_name: *const u16,
    field_index: c_int,
    unit_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
) -> c_int {
    get_value(
        pwstr_to_path(file_name),
        field_index,
        unit_index,
        field_value,
        max_len,
        flags,
        true,
    )
}

/// # Safety
/// `file_name` must be null or NUL-terminated; `field_value` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_value_a(
    file_name: *const c_char,
    field_index: c_int,
    unit_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
) -> c_int {
    get_value(
        pstr_to_path(file_name),
        field_index,
        unit_index,
        field_value,
        max_len,
        flags,
        false,
    )
}

pub fn content_stop_get_value() {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
    crate::cache::clear_row_key();
    crate::prefetch::cancel();
}

/// # Safety
/// `detect_string` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_detect_string(detect_string: *mut c_char, max_len: c_int) {
    write_ansi(detect_string, max_len.max(0) as usize, wdx_detect_string());
}

pub fn content_plugin_unloading() {
    crate::prefetch::shutdown();
    crate::cache::clear_all();
}

unsafe fn get_value(
    path: Option<PathBuf>,
    field_index: c_int,
    unit_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
    unicode: bool,
) -> c_int {
    // A stop is meant for TC's background thread, which asks without `CONTENT_DELAYIFSLOW`. The
    // main thread's quick calls for the next folder must not clear it while that thread still
    // works on a file the user has left.
    if flags & CONTENT_DELAYIFSLOW == 0 {
        reset_stop_flag();
    }
    let Some(path) = path else {
        return FT_FILEERROR;
    };
    let Some(&(field, _, _, _)) = field_at(field_index) else {
        return FT_NOSUCHFIELD;
    };
    if field_value.is_null() {
        return FT_FIELDEMPTY;
    }

    let kind = probe_file(&path);
    if !field.applies_to(kind) {
        return FT_FIELDEMPTY;
    }
    // Constants and extension-only fields never need a filesystem identity.
    if matches!(field.source(), Source::Constant | Source::Probe) {
        return Output {
            dest: field_value,
            max_bytes: max_len.max(0) as usize,
            unicode,
        }
        .value(compute(&path, field, unit_index, kind));
    }
    let sample = crate::prefetch::request(&path, field, kind);
    let result = crate::disk_cache::with_maintenance(flags & CONTENT_DELAYIFSLOW == 0, || {
        crate::cache::with_row_file_key(&path, field as u32, flags, || {
            if (flags & CONTENT_DELAYIFSLOW) != 0 && is_slow(&path, field, kind) {
                return FT_DELAYED;
            }
            let out = Output {
                dest: field_value,
                max_bytes: max_len.max(0) as usize,
                unicode,
            };
            out.value(compute(&path, field, unit_index, kind))
        })
    });
    if result != FT_DELAYED && !is_stop_requested() {
        if let Some(sample) = sample {
            sample.complete_foreground();
        }
    }
    result
}

impl Output {
    unsafe fn value(&self, value: Option<Value>) -> c_int {
        match value {
            Some(Value::Text(text)) => self.text(&text),
            Some(Value::Int(n)) => self.int(n),
            Some(Value::Bool(b)) => self.boolean(b),
            Some(Value::Time(seconds)) => self.time(seconds),
            Some(Value::Float(x)) => self.float(x),
            Some(Value::DateTime(filetime)) => self.datetime(filetime),
            None => FT_FIELDEMPTY,
        }
    }
}

/// Whether computing the field now would stall TC's file list (not cached yet and needs decoding
/// or Media Foundation).
fn is_slow(path: &Path, field: Field, kind: MediaType) -> bool {
    let needs_codec =
        || kind == MediaType::StandardImage && crate::image_decode::header_needs_codec(path);
    let analysis_pending = || !get_cache().is_cached(path);
    match field.source() {
        Source::Constant | Source::Probe => false,
        Source::PartialHash => !crate::cache::is_partial_hash_cached(path),
        Source::Exif => {
            matches!(kind, MediaType::StandardImage | MediaType::RawImage)
                && !crate::cache::is_exif_cached(path)
        }
        Source::ImageExtra => kind.is_image_kind() && !crate::cache::is_image_extra_cached(path),
        Source::ImageOrientation => {
            let exif_pending = matches!(kind, MediaType::StandardImage | MediaType::RawImage)
                && !crate::cache::is_exif_cached(path);
            let size_pending = if needs_codec() {
                !crate::cache::is_image_size_cached(path)
            } else if kind == MediaType::RawImage {
                !crate::cache::is_exif_cached(path)
            } else if kind != MediaType::StandardImage {
                analysis_pending() && header_size(path, kind).is_none()
            } else {
                false
            };
            exif_pending || size_pending
        }
        Source::Tags => {
            kind == MediaType::Audio
                && if field.is_audio_stream_property() && crate::cache::is_audio_meta_cached(path) {
                    false
                } else {
                    !tags_cached(path) || needs_audio_meta(path, field, kind)
                }
        }
        Source::VideoTags => {
            kind == MediaType::Video
                && crate::video_tags::has_tag_reader(path)
                && !crate::cache::is_video_tags_cached(path)
        }
        Source::PixelFormat => {
            kind == MediaType::StandardImage
                && needs_codec()
                && !crate::cache::is_pixel_format_cached(path)
        }
        Source::Sharpness => kind.is_image_kind() && !crate::cache::is_sharpness_cached(path),
        Source::ImageSize if needs_codec() => !crate::cache::is_image_size_cached(path),
        Source::ImageSize if kind == MediaType::RawImage && !crate::cache::is_exif_cached(path) => {
            true
        }
        Source::ImageSize => {
            kind != MediaType::StandardImage
                && analysis_pending()
                && header_size(path, kind).is_none()
        }
        Source::VideoMeta
            if matches!(
                field,
                Field::VideoDynamicRange | Field::VideoAudioLang | Field::VideoAudioStreams
            ) && kind == MediaType::Video =>
        {
            crate::cache::cached_video_meta(path).is_none_or(|meta| {
                let needs_tags = match field {
                    Field::VideoDynamicRange => meta.dynamic_range.is_none(),
                    Field::VideoAudioLang => meta.audio_lang.is_none(),
                    Field::VideoAudioStreams => meta.audio_streams.is_none_or(|n| n == 0),
                    _ => false,
                };
                needs_tags
                    && crate::video_tags::has_tag_reader(path)
                    && !crate::cache::is_video_tags_cached(path)
            })
        }
        Source::VideoMeta => kind == MediaType::Video && !crate::cache::is_video_meta_cached(path),
        Source::AudioAnalysis | Source::AudioFastFingerprint | Source::AudioDuration => {
            !crate::cache::is_audio_cached(path, audio_field(field))
        }
        Source::Analysis => analysis_pending(),
    }
}

enum Value {
    Text(String),
    Int(i32),
    Bool(bool),
    /// Duration in whole seconds, shown by TC as h:mm:ss.
    Time(u32),
    Float(f64),
    /// FILETIME, UTC.
    DateTime(u64),
}

fn int(n: u32) -> Value {
    Value::Int(n.min(i32::MAX as u32) as i32)
}

fn compute(path: &Path, field: Field, unit_index: c_int, kind: MediaType) -> Option<Value> {
    compute_cancelled(path, field, unit_index, kind, &is_stop_requested)
}

pub(crate) fn prefetch_value(
    path: &Path,
    key: Option<crate::cache::FileKey>,
    field: Field,
    cancelled: &dyn Fn() -> bool,
) {
    if !cancelled() {
        crate::disk_cache::with_maintenance(true, || {
            crate::cache::with_non_blocking_flight(|| {
                let key = key.or_else(|| crate::cache::FileKey::stat(path));
                crate::cache::with_known_file_key(key, || {
                    let _ = compute_cancelled(path, field, 0, probe_file(path), cancelled);
                });
            });
        });
    }
}

fn compute_cancelled(
    path: &Path,
    field: Field,
    unit_index: c_int,
    kind: MediaType,
    cancelled: &dyn Fn() -> bool,
) -> Option<Value> {
    use Field::*;
    let text = |s: String| Some(Value::Text(s));
    match field.source() {
        Source::Constant => return text(env!("CARGO_PKG_VERSION").to_string()),
        Source::Probe => {
            let name = match kind {
                k if k.is_image_kind() => "Image",
                MediaType::Video => "Video",
                MediaType::Audio => "Audio",
                _ => return None,
            };
            return text(name.into());
        }
        Source::PartialHash => {
            return crate::cache::get_partial_hash(path).map(|h| Value::Text(format!("{h:016x}")));
        }
        Source::Tags => return tag_value(path, field, kind),
        Source::Exif => return exif_value(path, field, unit_index, kind),
        Source::ImageExtra => return image_extra_value(path, field),
        Source::ImageOrientation => {
            return image_orientation_value(path, unit_index, kind, cancelled);
        }
        Source::VideoMeta => return video_meta_value(path, field, unit_index, kind),
        Source::VideoTags => return video_tag_value(path, field, kind),
        Source::AudioAnalysis | Source::AudioFastFingerprint | Source::AudioDuration => {
            return audio_analysis_value(path, field, cancelled);
        }
        Source::PixelFormat => {
            let format = (kind == MediaType::StandardImage)
                .then(|| crate::cache::get_pixel_format(path))
                .flatten()?;
            return Some(match field {
                ImageBitDepth => int(format.bits_per_channel),
                _ => Value::Bool(format.alpha),
            });
        }
        Source::Sharpness => {
            return kind
                .is_image_kind()
                .then(|| crate::cache::get_sharpness(path))
                .flatten()
                .map(int)
        }
        Source::ImageSize => {
            if let Some((w, h)) = header_size(path, kind) {
                return match field {
                    ImageWidth => Some(int(w)),
                    ImageHeight => Some(int(h)),
                    ImageMegapixels => Some(Value::Float(w as f64 * h as f64 / 1_000_000.0)),
                    ImageDimensions => text(format!("{w}x{h}")),
                    _ => format_aspect_ratio(w, h, unit_index).map(Value::Text),
                };
            }
            if kind == MediaType::StandardImage {
                return None;
            }
        }
        Source::Analysis => {}
    }
    match (field, get_cache().get_or_analyze(path, cancelled)) {
        (ImageWidth, CachedMedia::Image(img)) => Some(int(img.width)),
        (ImageHeight, CachedMedia::Image(img)) => Some(int(img.height)),
        (ImageMegapixels, CachedMedia::Image(img)) => Some(Value::Float(
            img.width as f64 * img.height as f64 / 1_000_000.0,
        )),
        (ImageDHash, CachedMedia::Image(img)) => text(img.dhash_hex()),
        (ImagePHash, CachedMedia::Image(img)) => text(img.phash_hex()),
        (ImageCoarseHash, CachedMedia::Image(img)) => text(img.coarse_hash_hex()),
        (ImageDimensions, CachedMedia::Image(img)) => text(img.dimensions_str()),
        (ImageAspectRatio, CachedMedia::Image(img)) => {
            format_aspect_ratio(img.width, img.height, unit_index).map(Value::Text)
        }
        (VideoFingerprint, CachedMedia::Video(vid)) => vid.fingerprint.clone().map(Value::Text),
        (VideoDHashMid, CachedMedia::Video(vid)) => vid.dhash_mid_hex().map(Value::Text),
        _ => None,
    }
}

fn format_aspect_ratio(w: u32, h: u32, unit_index: c_int) -> Option<String> {
    if unit_index == 1 {
        (w > 0 && h > 0).then(|| format!("{:.3}", f64::from(w) / f64::from(h)))
    } else {
        Some(crate::hashing::compute_aspect_ratio(w, h))
    }
}

fn format_orientation_by_size(w: u32, h: u32, unit_index: c_int) -> &'static str {
    if unit_index == 1 {
        if w > h {
            "H"
        } else if h > w {
            "V"
        } else {
            "S"
        }
    } else if w > h {
        "Landscape"
    } else if h > w {
        "Portrait"
    } else {
        "Square"
    }
}

fn image_extra_value(path: &Path, field: Field) -> Option<Value> {
    let extra = crate::cache::get_image_extra(path)?;
    match field {
        Field::ImageDpi => extra.dpi.map(int),
        Field::ImageAiParameters => extra.ai_parameters.clone().map(Value::Text),
        _ => None,
    }
}

fn image_orientation_value(
    path: &Path,
    unit_index: c_int,
    kind: MediaType,
    cancelled: &dyn Fn() -> bool,
) -> Option<Value> {
    let exif_orient = matches!(kind, MediaType::StandardImage | MediaType::RawImage)
        .then(|| {
            crate::cache::get_exif(path)
                .and_then(|e| e.orientation)
                .filter(|o| (1..=8).contains(o))
        })
        .flatten();
    if unit_index == 3 {
        return exif_orient.map(|o| Value::Text(o.to_string()));
    }
    let (raw_w, raw_h) = header_size(path, kind).or_else(|| {
        match get_cache().get_or_analyze(path, cancelled) {
            CachedMedia::Image(img) => Some((img.width, img.height)),
            _ => None,
        }
    })?;
    if raw_w == 0 || raw_h == 0 {
        return None;
    }
    if unit_index == 2 {
        let angle = match exif_orient {
            Some(3 | 4) => "180",
            Some(5 | 6) => "+90",
            Some(7 | 8) => "-90",
            _ => "0",
        };
        return Some(Value::Text(angle.into()));
    }
    let (w, h) = if matches!(exif_orient, Some(5..=8)) {
        (raw_h, raw_w)
    } else {
        (raw_w, raw_h)
    };
    Some(Value::Text(
        format_orientation_by_size(w, h, unit_index).into(),
    ))
}

fn audio_field(field: Field) -> crate::cache::AudioField {
    use crate::cache::AudioField;
    match field {
        Field::AudioFingerprint | Field::AudioPcmHash => AudioField::Analysis,
        Field::AudioFastFingerprint => AudioField::FastFingerprint,
        Field::AudioDurationSec => AudioField::Duration,
        _ => unreachable!("audio analysis field"),
    }
}

fn audio_analysis_value(path: &Path, field: Field, cancelled: &dyn Fn() -> bool) -> Option<Value> {
    #[cfg(feature = "audio-decode")]
    match field {
        Field::AudioFingerprint => crate::cache::get_audio_analysis(path, cancelled)?
            .fingerprint
            .map(Value::Text),
        Field::AudioFastFingerprint => crate::cache::get_audio_fast_fingerprint(path, cancelled)?
            .fingerprint
            .map(Value::Text),
        Field::AudioPcmHash => {
            let hash = crate::cache::get_audio_analysis(path, cancelled)?.pcm_hash;
            (!hash.is_empty()).then_some(Value::Text(hash))
        }
        Field::AudioDurationSec => {
            let sec = crate::cache::get_audio_duration(path, cancelled)?.0;
            (sec != u32::MAX).then_some(int(sec))
        }
        _ => None,
    }
    #[cfg(not(feature = "audio-decode"))]
    {
        let _ = (path, field, cancelled);
        None
    }
}

/// Image size without decoding: the header of a standard image, the EXIF of a RAW (its embedded
/// preview is often downscaled, e.g. 1616x1080 for a 6000x4000 Sony ARW).
fn header_size(path: &Path, kind: MediaType) -> Option<(u32, u32)> {
    match kind {
        MediaType::StandardImage => crate::cache::get_image_size(path),
        MediaType::RawImage => {
            let exif = crate::cache::get_exif(path)?;
            exif.width.zip(exif.height).filter(|&(w, h)| w > 0 && h > 0)
        }
        _ => None,
    }
}

fn exif_value(path: &Path, field: Field, unit_index: c_int, kind: MediaType) -> Option<Value> {
    use Field::*;
    if !matches!(kind, MediaType::StandardImage | MediaType::RawImage) {
        // PSD keeps EXIF in an image resource we don't parse; still answer "no GPS" for photos.
        return (field == PhotoHasGps && kind.is_image_kind()).then_some(Value::Bool(false));
    }
    let Some(exif) = crate::cache::get_exif(path) else {
        return (field == PhotoHasGps).then_some(Value::Bool(false));
    };
    let text = |s: &Option<String>| s.clone().map(Value::Text);
    let positive = |x: Option<f64>| x.filter(|&x| x > 0.0).map(Value::Float);
    match field {
        PhotoMake => text(&exif.make),
        PhotoModel => text(&exif.model),
        PhotoLens => text(&exif.lens_model),
        PhotoDateTaken => exif
            .taken()
            .and_then(crate::exif::parse_exif_datetime)
            .and_then(local_to_filetime)
            .map(Value::DateTime),
        PhotoExposure => text(&exif.exposure_time),
        PhotoFNumber => positive(exif.f_number),
        PhotoIso => exif.iso.filter(|&v| v > 0).map(int),
        PhotoFocalLength => positive(exif.focal_length),
        PhotoFocalLength35 => exif.focal_length_35mm.filter(|&v| v > 0).map(int),
        PhotoFlash => exif.flash_fired.map(Value::Bool),
        PhotoOrientation => exif
            .orientation
            .filter(|o| (1..=8).contains(o))
            .map(|o| int(o.into())),
        PhotoSoftware => text(&exif.software),
        PhotoGpsLatitude => exif.gps_latitude.map(Value::Float),
        PhotoGpsLongitude => exif.gps_longitude.map(Value::Float),
        PhotoGps => exif.gps().map(|(lat, lon)| Value::Text(format_gps(lat, lon, unit_index))),
        PhotoHasGps => Some(Value::Bool(exif.gps().is_some())),
        _ => None,
    }
}

fn format_gps(lat: f64, lon: f64, unit_index: c_int) -> String {
    match unit_index {
        1 => format!("{}, {}", format_dms(lat, 'N', 'S'), format_dms(lon, 'E', 'W')),
        2 => format!("{}, {}", format_dmm(lat, 'N', 'S'), format_dmm(lon, 'E', 'W')),
        _ => format!("{lat:.6}, {lon:.6}"),
    }
}

fn format_dms(val: f64, pos: char, neg: char) -> String {
    let hemi = if val < 0.0 { neg } else { pos };
    let total_sec = (val.abs() * 3600.0).round() as u32;
    let d = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = (total_sec % 60) as f64;
    format!("{d}°{m:02}'{s:04.1}\"{hemi}")
}

fn format_dmm(val: f64, pos: char, neg: char) -> String {
    let hemi = if val < 0.0 { neg } else { pos };
    let abs = val.abs();
    let d = abs.floor() as u32;
    let m = (abs - f64::from(d)) * 60.0;
    format!("{d}°{m:06.3}'{hemi}")
}

fn video_meta_value(path: &Path, field: Field, unit_index: c_int, kind: MediaType) -> Option<Value> {
    use Field::*;
    if kind != MediaType::Video {
        return None;
    }
    let meta = crate::cache::get_video_meta(path)?;
    let positive = |n: u32| (n > 0).then(|| int(n));
    match field {
        VideoDimensions => (meta.width > 0 && meta.height > 0)
            .then(|| Value::Text(format!("{}x{}", meta.width, meta.height))),
        VideoOrientation => {
            if meta.width == 0 || meta.height == 0 {
                return None;
            }
            let rot = meta.rotation.unwrap_or(0) % 360;
            if unit_index == 2 {
                let angle = match rot {
                    90 => "+90",
                    180 => "180",
                    270 => "-90",
                    _ => "0",
                };
                return Some(Value::Text(angle.into()));
            }
            let (w, h) = if rot == 90 || rot == 270 {
                (meta.height, meta.width)
            } else {
                (meta.width, meta.height)
            };
            Some(Value::Text(
                format_orientation_by_size(w, h, unit_index).into(),
            ))
        }
        VideoDurationSec => {
            (meta.duration_sec > 0.0).then(|| int(meta.duration_sec.round() as u32))
        }
        VideoWidth => positive(meta.width),
        VideoHeight => positive(meta.height),
        VideoLength => {
            (meta.duration_sec > 0.0).then(|| Value::Time(meta.duration_sec.round() as u32))
        }
        // Two decimals: 29.97, 23.98.
        VideoFrameRate => {
            (meta.frame_rate > 0.0).then(|| Value::Float((meta.frame_rate * 100.0).round() / 100.0))
        }
        VideoCodec => meta.codec.clone().map(Value::Text),
        VideoContainer => meta.container.clone().map(Value::Text),
        VideoBitrate => meta.bitrate_kbps.and_then(positive),
        VideoAudioCodec => meta.audio_codec.clone().map(Value::Text),
        VideoAudioChannels => meta.audio_channels.map(int),
        VideoAudioSampleRate => meta.audio_sample_rate.map(int),
        VideoAudioStreams => meta
            .audio_streams
            .filter(|&n| n > 0)
            .or_else(|| crate::cache::get_video_tags(path)?.audio_streams)
            .or(meta.audio_streams)
            .map(int),
        VideoAudioLang => meta
            .audio_lang
            .clone()
            .or_else(|| crate::cache::get_video_tags(path)?.audio_lang.clone())
            .map(Value::Text),
        VideoRotation => meta.rotation.map(int),
        // Media Foundation's Matroska source doesn't pass the colour description on.
        VideoDynamicRange => meta
            .dynamic_range
            .or_else(|| crate::cache::get_video_tags(path)?.dynamic_range())
            .map(|s| Value::Text(s.into())),
        VideoSummary => format_video_summary(&meta).map(Value::Text),
        _ => None,
    }
}

fn format_video_summary(meta: &crate::video_frame::VideoMeta) -> Option<String> {
    let mut video_parts = Vec::new();
    if let Some(codec) = meta.codec.as_deref().filter(|s| !s.is_empty()) {
        video_parts.push(codec.to_string());
    }
    if meta.width > 0 && meta.height > 0 {
        video_parts.push(format!("{}x{}", meta.width, meta.height));
    }
    if meta.frame_rate > 0.0 {
        let fps = (meta.frame_rate * 100.0).round() / 100.0;
        let s = if (fps - fps.round()).abs() < 0.005 {
            format!("{}fps", fps.round() as u32)
        } else {
            format!("{fps:.2}fps")
        };
        video_parts.push(s);
    }
    if let Some(kbps) = meta.bitrate_kbps.filter(|&b| b > 0) {
        video_parts.push(format!("{kbps} kbps"));
    }

    let mut audio_parts = Vec::new();
    if let Some(ac) = meta.audio_codec.as_deref().filter(|s| !s.is_empty()) {
        audio_parts.push(ac.to_string());
    }
    if let Some(ch) = meta.audio_channels.filter(|&c| c > 0) {
        audio_parts.push(format!("{ch}ch"));
    }

    match (video_parts.is_empty(), audio_parts.is_empty()) {
        (true, true) => None,
        (false, true) => Some(video_parts.join(" ")),
        (true, false) => Some(audio_parts.join(" ")),
        (false, false) => Some(format!("{} / {}", video_parts.join(" "), audio_parts.join(" "))),
    }
}

fn format_audio_summary(
    codec: Option<&str>,
    bitrate_kbps: Option<u32>,
    sample_rate: Option<u32>,
    channels: Option<u32>,
    bit_depth: Option<u32>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(c) = codec.filter(|s| !s.is_empty()) {
        parts.push(c.to_string());
    }
    if let Some(kbps) = bitrate_kbps.filter(|&b| b > 0) {
        parts.push(format!("{kbps} kbps"));
    }
    if let Some(sr) = sample_rate.filter(|&s| s > 0) {
        if sr % 1000 == 0 {
            parts.push(format!("{} kHz", sr / 1000));
        } else {
            parts.push(format!("{:.1} kHz", f64::from(sr) / 1000.0));
        }
    }
    if let Some(ch) = channels.filter(|&c| c > 0) {
        parts.push(format!("{ch}ch"));
    }
    if let Some(bits) = bit_depth.filter(|&b| b > 0) {
        parts.push(format!("{bits}-bit"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn video_tag_value(path: &Path, field: Field, kind: MediaType) -> Option<Value> {
    use Field::*;
    if kind != MediaType::Video {
        return None;
    }
    let tags = crate::cache::get_video_tags(path)?;
    let text = |s: &Option<String>| s.clone().map(Value::Text);
    match field {
        VideoTitle => text(&tags.title),
        VideoArtist => text(&tags.artist),
        VideoDirector => text(&tags.director),
        VideoDate => text(&tags.date),
        VideoYear => tags.year().map(int),
        VideoGenre => text(&tags.genre),
        VideoComment => text(&tags.comment),
        _ => None,
    }
}

/// EXIF times are the camera's local clock; TC expects UTC and shows it back in local time.
fn local_to_filetime(t: crate::exif::ExifDateTime) -> Option<u64> {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime};
    let local = SYSTEMTIME {
        wYear: t.year,
        wMonth: t.month.into(),
        wDayOfWeek: 0,
        wDay: t.day.into(),
        wHour: t.hour.into(),
        wMinute: t.minute.into(),
        wSecond: t.second.into(),
        wMilliseconds: 0,
    };
    let (mut utc, mut ft) = (SYSTEMTIME::default(), FILETIME::default());
    unsafe {
        // Fails on impossible dates such as February 30.
        TzSpecificLocalTimeToSystemTime(None, &local, &mut utc).ok()?;
        SystemTimeToFileTime(&utc, &mut ft).ok()?;
    }
    Some((ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64)
}

impl Field {
    /// Stream properties, which Media Foundation can supply when symphonia can't parse the file.
    fn is_audio_stream_property(self) -> bool {
        use Field::*;
        matches!(
            self,
            AudioLength
                | AudioBitrate
                | AudioSampleRate
                | AudioChannels
                | AudioBitDepth
                | AudioCodec
                | AudioLossless
                | AudioSummary
        )
    }
}

/// The field has to come from Media Foundation: an audio file symphonia doesn't recognize (WMA, AC3...).
fn needs_audio_meta(path: &Path, field: Field, kind: MediaType) -> bool {
    kind == MediaType::Audio && field.is_audio_stream_property() && !has_tags(path)
}

#[cfg(feature = "tags")]
fn has_tags(path: &Path) -> bool {
    crate::cache::get_tags(path).is_some()
}

#[cfg(not(feature = "tags"))]
fn has_tags(_path: &Path) -> bool {
    false
}

#[cfg(feature = "tags")]
fn tags_cached(path: &Path) -> bool {
    crate::cache::is_tags_cached(path)
}

/// Without tag support there is nothing to read.
#[cfg(not(feature = "tags"))]
fn tags_cached(_path: &Path) -> bool {
    true
}

fn tag_value(path: &Path, field: Field, kind: MediaType) -> Option<Value> {
    if kind != MediaType::Audio {
        return None;
    }
    if field.is_audio_stream_property()
        && ((!tags_cached(path) && crate::cache::is_audio_meta_cached(path))
            || needs_audio_meta(path, field, MediaType::Audio))
    {
        return audio_meta_value(path, field);
    }
    tags_value(path, field)
}

fn audio_meta_value(path: &Path, field: Field) -> Option<Value> {
    use Field::*;
    let meta = crate::cache::get_audio_meta(path)?;
    let num = |n: Option<u32>| n.filter(|&n| n > 0).map(int);
    match field {
        AudioLength => {
            (meta.duration_sec > 0.0).then(|| Value::Time(meta.duration_sec.round() as u32))
        }
        AudioBitrate => num(meta.bitrate_kbps),
        AudioSampleRate => num(meta.sample_rate),
        AudioChannels => num(meta.channels),
        AudioBitDepth => num(meta.bit_depth),
        AudioCodec => meta.codec.clone().map(Value::Text),
        AudioLossless => meta.lossless.map(Value::Bool),
        AudioSummary => format_audio_summary(
            meta.codec.as_deref(),
            meta.bitrate_kbps,
            meta.sample_rate,
            meta.channels,
            meta.bit_depth,
        )
        .map(Value::Text),
        _ => None,
    }
}

#[cfg(feature = "tags")]
fn tags_value(path: &Path, field: Field) -> Option<Value> {
    use Field::*;
    let tags = crate::cache::get_tags(path)?;
    let text = |s: &Option<String>| s.clone().map(Value::Text);
    let num = |n: Option<u32>| n.filter(|&n| n > 0).map(int);
    match field {
        AudioArtistTitle => tags.normalized_artist_title().map(Value::Text),
        AudioReplayGain => tags
            .replay_gain_db
            .filter(|v| v.is_finite())
            .map(|v| Value::Float(f64::from(v))),
        AudioArtist => text(&tags.artist),
        AudioTitle => text(&tags.title),
        AudioAlbum => text(&tags.album),
        AudioAlbumArtist => text(&tags.album_artist),
        AudioYear => num(tags.year),
        AudioTrack => num(tags.track),
        AudioDisc => num(tags.disc),
        AudioGenre => text(&tags.genre),
        AudioComment => text(&tags.comment),
        AudioLength => {
            (tags.duration_sec > 0.0).then(|| Value::Time(tags.duration_sec.round() as u32))
        }
        AudioBitrate => num(tags.bitrate_kbps),
        AudioSampleRate => num(tags.sample_rate),
        AudioChannels => num(tags.channels.map(u32::from)),
        AudioBitDepth => num(tags.bit_depth.map(u32::from)),
        AudioHasCover => Some(Value::Bool(tags.has_cover)),
        AudioCodec => tags.codec.map(|c| Value::Text(c.into())),
        AudioLossless => tags.lossless.map(Value::Bool),
        AudioComposer => text(&tags.composer),
        AudioTrackTotal => num(tags.track_total),
        AudioDiscTotal => num(tags.disc_total),
        AudioSummary => format_audio_summary(
            tags.codec,
            tags.bitrate_kbps,
            tags.sample_rate,
            tags.channels.map(u32::from),
            tags.bit_depth.map(u32::from),
        )
        .map(Value::Text),
        _ => None,
    }
}

#[cfg(not(feature = "tags"))]
fn tags_value(_path: &Path, _field: Field) -> Option<Value> {
    None
}

struct Output {
    dest: *mut c_void,
    max_bytes: usize,
    unicode: bool,
}

impl Output {
    unsafe fn text(&self, text: &str) -> c_int {
        if self.unicode {
            if write_wide(self.dest as *mut u16, self.max_bytes, text) {
                FT_STRINGW
            } else {
                FT_FIELDEMPTY
            }
        } else if write_ansi(self.dest as *mut c_char, self.max_bytes, text) {
            FT_STRING
        } else {
            FT_FIELDEMPTY
        }
    }

    unsafe fn int(&self, n: i32) -> c_int {
        if self.max_bytes < 4 {
            return FT_FIELDEMPTY;
        }
        (self.dest as *mut i32).write_unaligned(n);
        FT_NUMERIC_32
    }

    unsafe fn boolean(&self, b: bool) -> c_int {
        if self.max_bytes < 4 {
            return FT_FIELDEMPTY;
        }
        (self.dest as *mut i32).write_unaligned(b as i32);
        FT_BOOLEAN
    }

    /// A double; the optional display string after it is left empty so TC formats the number.
    unsafe fn float(&self, x: f64) -> c_int {
        if self.max_bytes < 10 {
            return FT_FIELDEMPTY;
        }
        let dest = self.dest as *mut u8;
        std::ptr::copy_nonoverlapping(x.to_le_bytes().as_ptr(), dest, 8);
        // Empty for both the ANSI and the wide call.
        dest.add(8).write_bytes(0, 2);
        FT_NUMERIC_FLOATING
    }

    unsafe fn datetime(&self, filetime: u64) -> c_int {
        if self.max_bytes < 8 {
            return FT_FIELDEMPTY;
        }
        (self.dest as *mut u64).write_unaligned(filetime);
        FT_DATETIME
    }

    /// `ttimeformat { WORD wHour, wMinute, wSecond }`.
    unsafe fn time(&self, seconds: u32) -> c_int {
        if self.max_bytes < 6 {
            return FT_FIELDEMPTY;
        }
        let hours = (seconds / 3600).min(u16::MAX as u32) as u16;
        let parts = [hours, (seconds / 60 % 60) as u16, (seconds % 60) as u16];
        std::ptr::copy_nonoverlapping(parts.as_ptr() as *const u8, self.dest as *mut u8, 6);
        FT_TIME
    }
}

/// Defines the `#[no_mangle]` WDX exports in the calling cdylib crate, each wrapped in a panic guard.
#[macro_export]
macro_rules! export_content_plugin {
    () => {
        $crate::export_content_plugin!(init = || {});
    };
    // `init` runs before files are looked at (the combo plugin enables libmpv's formats there).
    (init = $init:expr) => {
        #[no_mangle]
        pub unsafe extern "system" fn ContentGetSupportedField(
            field_index: ::std::os::raw::c_int,
            field_name: *mut ::std::os::raw::c_char,
            units: *mut ::std::os::raw::c_char,
            max_len: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            $crate::ffi::guard($crate::tc_api::FT_NOMOREFIELDS, || unsafe {
                $crate::wdx_api::content_get_supported_field(
                    field_index,
                    field_name,
                    units,
                    max_len,
                )
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentGetValueW(
            file_name: *const u16,
            field_index: ::std::os::raw::c_int,
            unit_index: ::std::os::raw::c_int,
            field_value: *mut ::std::os::raw::c_void,
            max_len: ::std::os::raw::c_int,
            flags: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            $crate::ffi::guard($crate::tc_api::FT_FILEERROR, || unsafe {
                ($init)();
                $crate::wdx_api::content_get_value_w(
                    file_name,
                    field_index,
                    unit_index,
                    field_value,
                    max_len,
                    flags,
                )
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentGetValue(
            file_name: *const ::std::os::raw::c_char,
            field_index: ::std::os::raw::c_int,
            unit_index: ::std::os::raw::c_int,
            field_value: *mut ::std::os::raw::c_void,
            max_len: ::std::os::raw::c_int,
            flags: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            $crate::ffi::guard($crate::tc_api::FT_FILEERROR, || unsafe {
                ($init)();
                $crate::wdx_api::content_get_value_a(
                    file_name,
                    field_index,
                    unit_index,
                    field_value,
                    max_len,
                    flags,
                )
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentStopGetValueW(_file_name: *const u16) {
            $crate::ffi::guard((), $crate::wdx_api::content_stop_get_value)
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentStopGetValue(
            _file_name: *const ::std::os::raw::c_char,
        ) {
            $crate::ffi::guard((), $crate::wdx_api::content_stop_get_value)
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentGetDetectString(
            detect_string: *mut ::std::os::raw::c_char,
            max_len: ::std::os::raw::c_int,
        ) {
            $crate::ffi::guard((), || unsafe {
                ($init)();
                $crate::wdx_api::content_get_detect_string(detect_string, max_len)
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentSetDefaultParams(
            dps: *mut $crate::tc_api::ContentDefaultParamStruct,
        ) {
            $crate::ffi::guard((), || unsafe {
                if let Some(dps) = dps.as_ref() {
                    // Bound the C string even if a host omits its trailing NUL.
                    if dps.default_ini_name.contains(&0) {
                        if let Some(ini) = $crate::ffi::pstr_to_path(dps.default_ini_name.as_ptr())
                        {
                            $crate::config::set_tc_ini_path(&ini);
                        }
                    }
                }
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentPluginUnloading() {
            $crate::ffi::guard((), $crate::wdx_api::content_plugin_unloading)
        }
    };
}
