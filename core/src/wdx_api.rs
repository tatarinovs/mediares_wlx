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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    ImageDHash,
    ImagePHash,
    ImageCoarseHash,
    ImageDimensions,
    ImageAspectRatio,
    VideoFingerprint,
    VideoDHashMid,
    VideoDurationSec,
    VideoDimensions,
    MediaTypeName,
    PluginVersion,
    AudioFingerprint,
    AudioPcmHash,
    AudioDurationSec,
    AudioArtistTitle,
    AudioArtist,
    AudioTitle,
    AudioAlbum,
    AudioAlbumArtist,
    AudioYear,
    AudioTrack,
    AudioDisc,
    AudioGenre,
    AudioComment,
    AudioLength,
    AudioBitrate,
    AudioSampleRate,
    AudioChannels,
    AudioBitDepth,
    AudioHasCover,
    ImageWidth,
    ImageHeight,
    PhotoMake,
    PhotoModel,
    PhotoLens,
    PhotoDateTaken,
    PhotoExposure,
    PhotoFNumber,
    PhotoIso,
    PhotoFocalLength,
    PhotoFocalLength35,
    PhotoFlash,
    PhotoOrientation,
    PhotoSoftware,
    PhotoGpsLatitude,
    PhotoGpsLongitude,
    PhotoHasGps,
    VideoWidth,
    VideoHeight,
    VideoLength,
    VideoFrameRate,
    VideoCodec,
    VideoBitrate,
    VideoAudioCodec,
    VideoAudioChannels,
    VideoAudioSampleRate,
    AudioCodec,
    AudioLossless,
    AudioComposer,
    AudioTrackTotal,
    AudioDiscTotal,
    VideoTitle,
    VideoArtist,
    VideoDirector,
    VideoDate,
    VideoYear,
    VideoGenre,
    VideoComment,
}

/// Where a field's value comes from; decides whether it is worth delaying.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Constant,
    /// The file extension.
    Probe,
    /// Audio tags: headers only. Delayed only for stream properties of files lofty can't parse,
    /// which come from Media Foundation.
    Tags,
    /// EXIF block: a small read, never delayed.
    Exif,
    /// Image header for standard formats, the full analysis for RAW/PSD.
    ImageSize,
    /// Media Foundation stream properties: no decoding, but slow to open.
    VideoMeta,
    /// Video container tags: headers only, never delayed.
    VideoTags,
    /// Decoding and hashing.
    Analysis,
}

impl Field {
    fn source(self) -> Source {
        use Field::*;
        match self {
            PluginVersion => Source::Constant,
            MediaTypeName => Source::Probe,
            ImageWidth | ImageHeight => Source::ImageSize,
            PhotoMake | PhotoModel | PhotoLens | PhotoDateTaken | PhotoExposure | PhotoFNumber | PhotoIso
            | PhotoFocalLength | PhotoFocalLength35 | PhotoFlash | PhotoOrientation | PhotoSoftware
            | PhotoGpsLatitude | PhotoGpsLongitude | PhotoHasGps => Source::Exif,
            VideoWidth | VideoHeight | VideoLength | VideoFrameRate | VideoCodec | VideoBitrate | VideoAudioCodec
            | VideoAudioChannels | VideoAudioSampleRate => Source::VideoMeta,
            VideoTitle | VideoArtist | VideoDirector | VideoDate | VideoYear | VideoGenre | VideoComment => {
                Source::VideoTags
            }
            _ if self.is_tag() => Source::Tags,
            _ => Source::Analysis,
        }
    }

    fn is_tag(self) -> bool {
        use Field::*;
        matches!(
            self,
            AudioArtistTitle
                | AudioArtist
                | AudioTitle
                | AudioAlbum
                | AudioAlbumArtist
                | AudioYear
                | AudioTrack
                | AudioDisc
                | AudioGenre
                | AudioComment
                | AudioLength
                | AudioBitrate
                | AudioSampleRate
                | AudioChannels
                | AudioBitDepth
                | AudioHasCover
                | AudioCodec
                | AudioLossless
                | AudioComposer
                | AudioTrackTotal
                | AudioDiscTotal
        )
    }
}

/// Field order is the public WDX index TC stores in user configurations — append only.
const FIELDS: &[(Field, &str, c_int)] = &[
    (Field::ImageDHash, "Image_dHash", FT_STRINGW),
    (Field::ImagePHash, "Image_pHash", FT_STRINGW),
    (Field::ImageCoarseHash, "Image_CoarseHash", FT_STRINGW),
    (Field::ImageDimensions, "Image_Dimensions", FT_STRINGW),
    (Field::ImageAspectRatio, "Image_AspectRatio", FT_STRINGW),
    (Field::VideoFingerprint, "Video_Fingerprint", FT_STRINGW),
    (Field::VideoDHashMid, "Video_dHash_Mid", FT_STRINGW),
    (Field::VideoDurationSec, "Video_Duration_Sec", FT_NUMERIC_32),
    (Field::VideoDimensions, "Video_Dimensions", FT_STRINGW),
    (Field::MediaTypeName, "Media_Type", FT_STRINGW),
    (Field::PluginVersion, "Plugin_Version", FT_STRINGW),
    (Field::AudioFingerprint, "Audio_Fingerprint", FT_STRINGW),
    (Field::AudioPcmHash, "Audio_PCM_Hash", FT_STRINGW),
    (Field::AudioDurationSec, "Audio_Duration_Sec", FT_NUMERIC_32),
    (Field::AudioArtistTitle, "Audio_Artist_Title", FT_STRINGW),
    (Field::AudioArtist, "Audio_Artist", FT_STRINGW),
    (Field::AudioTitle, "Audio_Title", FT_STRINGW),
    (Field::AudioAlbum, "Audio_Album", FT_STRINGW),
    (Field::AudioAlbumArtist, "Audio_Album_Artist", FT_STRINGW),
    (Field::AudioYear, "Audio_Year", FT_NUMERIC_32),
    (Field::AudioTrack, "Audio_Track", FT_NUMERIC_32),
    (Field::AudioDisc, "Audio_Disc", FT_NUMERIC_32),
    (Field::AudioGenre, "Audio_Genre", FT_STRINGW),
    (Field::AudioComment, "Audio_Comment", FT_STRINGW),
    (Field::AudioLength, "Audio_Length", FT_TIME),
    (Field::AudioBitrate, "Audio_Bitrate_kbps", FT_NUMERIC_32),
    (Field::AudioSampleRate, "Audio_Sample_Rate_Hz", FT_NUMERIC_32),
    (Field::AudioChannels, "Audio_Channels", FT_NUMERIC_32),
    (Field::AudioBitDepth, "Audio_Bit_Depth", FT_NUMERIC_32),
    (Field::AudioHasCover, "Audio_Has_Cover", FT_BOOLEAN),
    (Field::ImageWidth, "Image_Width", FT_NUMERIC_32),
    (Field::ImageHeight, "Image_Height", FT_NUMERIC_32),
    (Field::PhotoMake, "Photo_Make", FT_STRINGW),
    (Field::PhotoModel, "Photo_Model", FT_STRINGW),
    (Field::PhotoLens, "Photo_Lens", FT_STRINGW),
    (Field::PhotoDateTaken, "Photo_Date_Taken", FT_DATETIME),
    (Field::PhotoExposure, "Photo_Exposure", FT_STRINGW),
    (Field::PhotoFNumber, "Photo_FNumber", FT_NUMERIC_FLOATING),
    (Field::PhotoIso, "Photo_ISO", FT_NUMERIC_32),
    (Field::PhotoFocalLength, "Photo_Focal_Length_mm", FT_NUMERIC_FLOATING),
    (Field::PhotoFocalLength35, "Photo_Focal_Length_35mm", FT_NUMERIC_32),
    (Field::PhotoFlash, "Photo_Flash", FT_BOOLEAN),
    (Field::PhotoOrientation, "Photo_Orientation", FT_NUMERIC_32),
    (Field::PhotoSoftware, "Photo_Software", FT_STRINGW),
    (Field::PhotoGpsLatitude, "Photo_GPS_Latitude", FT_NUMERIC_FLOATING),
    (Field::PhotoGpsLongitude, "Photo_GPS_Longitude", FT_NUMERIC_FLOATING),
    (Field::PhotoHasGps, "Photo_Has_GPS", FT_BOOLEAN),
    (Field::VideoWidth, "Video_Width", FT_NUMERIC_32),
    (Field::VideoHeight, "Video_Height", FT_NUMERIC_32),
    (Field::VideoLength, "Video_Length", FT_TIME),
    (Field::VideoFrameRate, "Video_Frame_Rate", FT_NUMERIC_FLOATING),
    (Field::VideoCodec, "Video_Codec", FT_STRINGW),
    (Field::VideoBitrate, "Video_Bitrate_kbps", FT_NUMERIC_32),
    (Field::VideoAudioCodec, "Video_Audio_Codec", FT_STRINGW),
    (Field::VideoAudioChannels, "Video_Audio_Channels", FT_NUMERIC_32),
    (Field::VideoAudioSampleRate, "Video_Audio_Sample_Rate_Hz", FT_NUMERIC_32),
    (Field::AudioCodec, "Audio_Codec", FT_STRINGW),
    (Field::AudioLossless, "Audio_Lossless", FT_BOOLEAN),
    (Field::AudioComposer, "Audio_Composer", FT_STRINGW),
    (Field::AudioTrackTotal, "Audio_Track_Total", FT_NUMERIC_32),
    (Field::AudioDiscTotal, "Audio_Disc_Total", FT_NUMERIC_32),
    (Field::VideoTitle, "Video_Title", FT_STRINGW),
    (Field::VideoArtist, "Video_Artist", FT_STRINGW),
    (Field::VideoDirector, "Video_Director", FT_STRINGW),
    (Field::VideoDate, "Video_Date", FT_STRINGW),
    (Field::VideoYear, "Video_Year", FT_NUMERIC_32),
    (Field::VideoGenre, "Video_Genre", FT_STRINGW),
    (Field::VideoComment, "Video_Comment", FT_STRINGW),
];

fn field_at(index: c_int) -> Option<&'static (Field, &'static str, c_int)> {
    FIELDS.get(usize::try_from(index).ok()?)
}

/// Kinds the WDX analyzes (audio only when built with the decoder).
pub fn wdx_detect_string() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| {
        let mut kinds = vec![MediaType::StandardImage, MediaType::RawImage, MediaType::PsdImage, MediaType::Video];
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
    let Some(&(_, name, field_type)) = field_at(field_index) else {
        return FT_NOMOREFIELDS;
    };
    let max_len = max_len.max(0) as usize;
    write_ansi(field_name, max_len, name);
    write_ansi(units, max_len, "");
    field_type
}

/// # Safety
/// `file_name` must be null or NUL-terminated; `field_value` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_value_w(
    file_name: *const u16,
    field_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
) -> c_int {
    get_value(pwstr_to_path(file_name), field_index, field_value, max_len, flags, true)
}

/// # Safety
/// `file_name` must be null or NUL-terminated; `field_value` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_value_a(
    file_name: *const c_char,
    field_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
) -> c_int {
    get_value(pstr_to_path(file_name), field_index, field_value, max_len, flags, false)
}

pub fn content_stop_get_value() {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
}

/// # Safety
/// `detect_string` must be null or valid for `max_len` bytes.
pub unsafe fn content_get_detect_string(detect_string: *mut c_char, max_len: c_int) {
    write_ansi(detect_string, max_len.max(0) as usize, wdx_detect_string());
}

pub fn content_plugin_unloading() {
    get_cache().clear();
    crate::mf_init::shutdown_mf();
}

unsafe fn get_value(
    path: Option<PathBuf>,
    field_index: c_int,
    field_value: *mut c_void,
    max_len: c_int,
    flags: c_int,
    unicode: bool,
) -> c_int {
    reset_stop_flag();
    let Some(path) = path else { return FT_FILEERROR };
    let Some(&(field, _, _)) = field_at(field_index) else { return FT_FIELDEMPTY };
    if field_value.is_null() {
        return FT_FIELDEMPTY;
    }

    let kind = probe_file(&path);
    if (flags & CONTENT_DELAYIFSLOW) != 0 && is_slow(&path, field, kind) {
        return FT_DELAYED;
    }

    let out = Output { dest: field_value, max_bytes: max_len.max(0) as usize, unicode };
    match compute(&path, field, kind) {
        Some(Value::Text(text)) => out.text(&text),
        Some(Value::Int(n)) => out.int(n),
        Some(Value::Bool(b)) => out.boolean(b),
        Some(Value::Time(seconds)) => out.time(seconds),
        Some(Value::Float(x)) => out.float(x),
        Some(Value::DateTime(filetime)) => out.datetime(filetime),
        None => FT_FIELDEMPTY,
    }
}

/// Whether computing the field now would stall TC's file list (not cached yet and needs decoding
/// or Media Foundation).
fn is_slow(path: &Path, field: Field, kind: MediaType) -> bool {
    let analysis_pending = || kind.is_slow_kind() && !get_cache().is_cached(path);
    match field.source() {
        Source::Constant | Source::Probe | Source::Exif | Source::VideoTags => false,
        Source::Tags => needs_audio_meta(path, field, kind) && !crate::cache::is_audio_meta_cached(path),
        Source::ImageSize => kind != MediaType::StandardImage && analysis_pending(),
        Source::VideoMeta => kind == MediaType::Video && !crate::cache::is_video_meta_cached(path),
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

fn compute(path: &Path, field: Field, kind: MediaType) -> Option<Value> {
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
        Source::Tags => return tag_value(path, field),
        Source::Exif => return exif_value(path, field, kind),
        Source::VideoMeta => return video_meta_value(path, field, kind),
        Source::VideoTags => return video_tag_value(path, field, kind),
        Source::ImageSize if kind == MediaType::StandardImage => {
            let (w, h) = crate::image_decode::header_dimensions(path)?;
            return Some(int(if field == ImageWidth { w } else { h }));
        }
        Source::ImageSize | Source::Analysis => {}
    }
    match (field, get_cache().get_or_analyze(path, &is_stop_requested)) {
        (ImageWidth, CachedMedia::Image(img)) => Some(int(img.width)),
        (ImageHeight, CachedMedia::Image(img)) => Some(int(img.height)),
        (ImageDHash, CachedMedia::Image(img)) => text(img.dhash_hex()),
        (ImagePHash, CachedMedia::Image(img)) => text(img.phash_hex()),
        (ImageCoarseHash, CachedMedia::Image(img)) => text(img.coarse_hash_hex()),
        (ImageDimensions, CachedMedia::Image(img)) => text(img.dimensions_str()),
        (ImageAspectRatio, CachedMedia::Image(img)) => text(img.aspect_ratio.clone()),
        (VideoFingerprint, CachedMedia::Video(vid)) => text(vid.fingerprint.clone()),
        (VideoDHashMid, CachedMedia::Video(vid)) => text(vid.dhash_mid_hex()),
        (VideoDurationSec, CachedMedia::Video(vid)) => Some(int(vid.duration_sec)),
        (VideoDimensions, CachedMedia::Video(vid)) => text(vid.dimensions_str()),
        (AudioFingerprint, CachedMedia::Audio(a)) => a.fingerprint.clone().map(Value::Text),
        (AudioPcmHash, CachedMedia::Audio(a)) => text(a.pcm_hash.clone()),
        (AudioDurationSec, CachedMedia::Audio(a)) => Some(int(a.duration_sec)),
        _ => None,
    }
}

fn exif_value(path: &Path, field: Field, kind: MediaType) -> Option<Value> {
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
        PhotoOrientation => exif.orientation.filter(|o| (1..=8).contains(o)).map(|o| int(o.into())),
        PhotoSoftware => text(&exif.software),
        PhotoGpsLatitude => exif.gps_latitude.map(Value::Float),
        PhotoGpsLongitude => exif.gps_longitude.map(Value::Float),
        PhotoHasGps => Some(Value::Bool(exif.gps_latitude.is_some())),
        _ => None,
    }
}

fn video_meta_value(path: &Path, field: Field, kind: MediaType) -> Option<Value> {
    use Field::*;
    if kind != MediaType::Video {
        return None;
    }
    let meta = crate::cache::get_video_meta(path)?;
    let positive = |n: u32| (n > 0).then(|| int(n));
    match field {
        VideoWidth => positive(meta.width),
        VideoHeight => positive(meta.height),
        VideoLength => (meta.duration_sec > 0.0).then(|| Value::Time(meta.duration_sec.round() as u32)),
        // Two decimals: 29.97, 23.98.
        VideoFrameRate => (meta.frame_rate > 0.0).then(|| Value::Float((meta.frame_rate * 100.0).round() / 100.0)),
        VideoCodec => meta.codec.clone().map(Value::Text),
        VideoBitrate => meta.bitrate_kbps.and_then(positive),
        VideoAudioCodec => meta.audio_codec.clone().map(Value::Text),
        VideoAudioChannels => meta.audio_channels.map(int),
        VideoAudioSampleRate => meta.audio_sample_rate.map(int),
        _ => None,
    }
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
    /// Stream properties, which Media Foundation can supply when lofty can't parse the file.
    fn is_audio_stream_property(self) -> bool {
        use Field::*;
        matches!(
            self,
            AudioLength | AudioBitrate | AudioSampleRate | AudioChannels | AudioBitDepth | AudioCodec | AudioLossless
        )
    }
}

/// The field has to come from Media Foundation: an audio file lofty doesn't recognize (WMA, AC3...).
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

fn tag_value(path: &Path, field: Field) -> Option<Value> {
    if probe_file(path) != MediaType::Audio {
        return None;
    }
    if needs_audio_meta(path, field, MediaType::Audio) {
        return audio_meta_value(path, field);
    }
    lofty_value(path, field)
}

fn audio_meta_value(path: &Path, field: Field) -> Option<Value> {
    use Field::*;
    let meta = crate::cache::get_audio_meta(path)?;
    let num = |n: Option<u32>| n.filter(|&n| n > 0).map(int);
    match field {
        AudioLength => (meta.duration_sec > 0.0).then(|| Value::Time(meta.duration_sec.round() as u32)),
        AudioBitrate => num(meta.bitrate_kbps),
        AudioSampleRate => num(meta.sample_rate),
        AudioChannels => num(meta.channels),
        AudioBitDepth => num(meta.bit_depth),
        AudioCodec => meta.codec.clone().map(Value::Text),
        AudioLossless => meta.lossless.map(Value::Bool),
        _ => None,
    }
}

#[cfg(feature = "tags")]
fn lofty_value(path: &Path, field: Field) -> Option<Value> {
    use Field::*;
    let tags = crate::cache::get_tags(path)?;
    let text = |s: &Option<String>| s.clone().map(Value::Text);
    let num = |n: Option<u32>| n.filter(|&n| n > 0).map(int);
    match field {
        AudioArtistTitle => tags.normalized_artist_title().map(Value::Text),
        AudioArtist => text(&tags.artist),
        AudioTitle => text(&tags.title),
        AudioAlbum => text(&tags.album),
        AudioAlbumArtist => text(&tags.album_artist),
        AudioYear => num(tags.year),
        AudioTrack => num(tags.track),
        AudioDisc => num(tags.disc),
        AudioGenre => text(&tags.genre),
        AudioComment => text(&tags.comment),
        AudioLength => (tags.duration_sec > 0.0).then(|| Value::Time(tags.duration_sec.round() as u32)),
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
        _ => None,
    }
}

#[cfg(not(feature = "tags"))]
fn lofty_value(_path: &Path, _field: Field) -> Option<Value> {
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
            if write_wide(self.dest as *mut u16, self.max_bytes, text) { FT_STRINGW } else { FT_FIELDEMPTY }
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
        #[no_mangle]
        pub unsafe extern "system" fn ContentGetSupportedField(
            field_index: ::std::os::raw::c_int,
            field_name: *mut ::std::os::raw::c_char,
            units: *mut ::std::os::raw::c_char,
            max_len: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            $crate::ffi::guard($crate::tc_api::FT_NOMOREFIELDS, || unsafe {
                $crate::wdx_api::content_get_supported_field(field_index, field_name, units, max_len)
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentGetValueW(
            file_name: *const u16,
            field_index: ::std::os::raw::c_int,
            _unit_index: ::std::os::raw::c_int,
            field_value: *mut ::std::os::raw::c_void,
            max_len: ::std::os::raw::c_int,
            flags: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            $crate::ffi::guard($crate::tc_api::FT_FILEERROR, || unsafe {
                $crate::wdx_api::content_get_value_w(file_name, field_index, field_value, max_len, flags)
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentGetValue(
            file_name: *const ::std::os::raw::c_char,
            field_index: ::std::os::raw::c_int,
            _unit_index: ::std::os::raw::c_int,
            field_value: *mut ::std::os::raw::c_void,
            max_len: ::std::os::raw::c_int,
            flags: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            $crate::ffi::guard($crate::tc_api::FT_FILEERROR, || unsafe {
                $crate::wdx_api::content_get_value_a(file_name, field_index, field_value, max_len, flags)
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentStopGetValueW(_file_name: *const u16) {
            $crate::ffi::guard((), $crate::wdx_api::content_stop_get_value)
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentStopGetValue(_file_name: *const ::std::os::raw::c_char) {
            $crate::ffi::guard((), $crate::wdx_api::content_stop_get_value)
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentGetDetectString(
            detect_string: *mut ::std::os::raw::c_char,
            max_len: ::std::os::raw::c_int,
        ) {
            $crate::ffi::guard((), || unsafe {
                $crate::wdx_api::content_get_detect_string(detect_string, max_len)
            })
        }

        #[no_mangle]
        pub unsafe extern "system" fn ContentSetDefaultParams(_dps: *mut $crate::tc_api::ContentDefaultParamStruct) {}

        #[no_mangle]
        pub unsafe extern "system" fn ContentPluginUnloading() {
            $crate::ffi::guard((), $crate::wdx_api::content_plugin_unloading)
        }
    };
}
