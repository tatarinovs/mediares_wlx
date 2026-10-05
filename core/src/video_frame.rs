//! Video metadata and keyframe hashing via Windows Media Foundation (IMFSourceReader); also the
//! playability check for audio that only Media Foundation can decode.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFAttributes, IMFMediaBuffer, IMFSample, IMFSourceReader,
    MF2DBuffer_LockFlags_Read, MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL,
    MFMediaType_Video, MFVideoFormat_NV12, MFVideoFormat_RGB32, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::{VT_I8, VT_UI8};

use crate::mf_init::mf_scope;

#[derive(Debug, Clone)]
pub struct VideoAnalysis {
    pub duration_sec: u32,
    pub width: u32,
    pub height: u32,
    /// `None` if the middle frame could not be decoded.
    pub dhash_mid: Option<u64>,
    /// `None` unless all three frames were decoded: a missing hash must not match other files.
    pub fingerprint: Option<String>,
}

impl VideoAnalysis {
    pub fn dhash_mid_hex(&self) -> Option<String> {
        self.dhash_mid.map(|h| format!("{:016x}", h))
    }

    pub fn dimensions_str(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

/// Stream properties for WDX columns: codecs, frame rate, bitrate, the audio track. No decoding.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoMeta {
    pub width: u32,
    pub height: u32,
    pub duration_sec: f64,
    /// Frames per second (0 if the stream doesn't say).
    pub frame_rate: f64,
    /// Short codec name ("H.264", "HEVC", ...) or the FOURCC.
    pub codec: Option<String>,
    /// Whole file (size / duration), kbit/s.
    pub bitrate_kbps: Option<u32>,
    /// `None` when there is no audio track.
    pub audio_codec: Option<String>,
    pub audio_channels: Option<u32>,
    pub audio_sample_rate: Option<u32>,
}

#[derive(Debug)]
pub enum VideoError {
    /// The host asked to stop (`ContentStopGetValue`); the result must not be cached.
    Cancelled,
    Failed(String),
}

impl From<windows::core::Error> for VideoError {
    fn from(e: windows::core::Error) -> Self {
        VideoError::Failed(e.to_string())
    }
}

pub(crate) const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Opens a source reader restricted to the first video stream.
pub(crate) unsafe fn open_reader(
    path: &Path,
    video_processing: bool,
) -> windows::core::Result<IMFSourceReader> {
    open_reader_for(path, STREAM, video_processing)
}

pub(crate) unsafe fn open_reader_for(
    path: &Path,
    stream: u32,
    video_processing: bool,
) -> windows::core::Result<IMFSourceReader> {
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut attributes: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attributes, 1)?;
    let attributes =
        attributes.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
    if video_processing {
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
    }
    let reader = MFCreateSourceReaderFromURL(PCWSTR(path_wide.as_ptr()), Some(&attributes))?;
    let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
    reader.SetStreamSelection(stream, true)?;
    Ok(reader)
}

pub(crate) unsafe fn duration_hns(reader: &IMFSourceReader) -> u64 {
    reader
        .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        .ok()
        .and_then(|v| propvariant_u64(&v))
        .unwrap_or(0)
}

pub(crate) unsafe fn duration_sec(reader: &IMFSourceReader) -> f64 {
    duration_hns(reader) as f64 / HNS_PER_SEC
}

/// Average bitrate of the whole file (size / duration), kbit/s.
pub(crate) fn file_bitrate_kbps(path: &Path, duration_sec: f64) -> Option<u32> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    (duration_sec >= 1.0 && size > 0)
        .then(|| (size as f64 * 8.0 / 1000.0 / duration_sec).round() as u32)
}

/// Reads [`VideoMeta`] from the native media types of the first video and audio streams.
/// Also the check that Media Foundation can open the file and has a video stream.
pub fn probe_video_meta(path: &Path) -> Option<VideoMeta> {
    let _com = mf_scope()?;
    unsafe {
        let reader = open_reader(path, false).ok()?;
        let video = reader.GetNativeMediaType(STREAM, 0).ok()?;
        let frame_size = video.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        // Packed as numerator << 32 | denominator.
        let rate = video.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        let (num, den) = ((rate >> 32) as u32, rate as u32);
        let duration_sec = duration_sec(&reader);
        let audio = reader
            .GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0)
            .ok();
        let positive = |v: windows::core::Result<u32>| v.ok().filter(|&v| v > 0);
        Some(VideoMeta {
            width: (frame_size >> 32) as u32,
            height: frame_size as u32,
            duration_sec,
            frame_rate: if den > 0 {
                num as f64 / den as f64
            } else {
                0.0
            },
            codec: video
                .GetGUID(&MF_MT_SUBTYPE)
                .ok()
                .and_then(|g| video_codec_name(&g)),
            bitrate_kbps: file_bitrate_kbps(path, duration_sec),
            audio_codec: audio
                .as_ref()
                .and_then(|a| a.GetGUID(&MF_MT_SUBTYPE).ok())
                .and_then(|g| audio_codec_name(&g)),
            audio_channels: audio
                .as_ref()
                .and_then(|a| positive(a.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS))),
            audio_sample_rate: audio
                .as_ref()
                .and_then(|a| positive(a.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND))),
        })
    }
}

/// Most MF subtypes are `XXXXXXXX-0000-0010-8000-00AA00389B71`, with a FOURCC (video) or a WAVE
/// format tag (audio) in the first field.
fn fourcc_base(guid: &GUID) -> Option<u32> {
    (guid.data2 == 0x0000
        && guid.data3 == 0x0010
        && guid.data4 == [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71])
    .then_some(guid.data1)
}

/// Subtypes outside the FOURCC family (MPEG-1/2 video and Dolby audio from DirectShow).
const MPEG1_VIDEO: GUID = GUID::from_u128(0xe436eb81_524f_11ce_9f53_0020af0ba770);
const MPEG2_VIDEO: GUID = GUID::from_u128(0xe06d8026_db46_11cf_b4d1_00805f6cbbea);
/// `MFVideoFormat_H264_ES`: H.264 from the MPEG transport stream source.
const H264_ES: GUID = GUID::from_u128(0x3f40f4f0_5622_4ff8_b6d8_a17a584bee5e);
const DOLBY_AC3: GUID = GUID::from_u128(0xe06d802c_db46_11cf_b4d1_00805f6cbbea);
const DOLBY_DDPLUS: GUID = GUID::from_u128(0xa7fb87af_2d02_42fb_a4d4_05cd93843bdd);

pub(crate) fn video_codec_name(guid: &GUID) -> Option<String> {
    if *guid == MPEG2_VIDEO {
        return Some("MPEG-2".into());
    }
    if *guid == MPEG1_VIDEO {
        return Some("MPEG-1".into());
    }
    if *guid == H264_ES {
        return Some("H.264".into());
    }
    let fourcc = fourcc_base(guid)?.to_le_bytes();
    let name = match &fourcc.map(|b| b.to_ascii_uppercase()) {
        b"H264" | b"AVC1" | b"X264" => "H.264",
        b"HEVC" | b"HEVS" | b"H265" | b"HVC1" | b"HEV1" => "HEVC",
        b"AV01" => "AV1",
        b"VP90" => "VP9",
        b"VP80" => "VP8",
        b"MP4V" | b"M4S2" | b"MP4S" => "MPEG-4",
        b"XVID" => "Xvid",
        b"DIVX" | b"DX50" | b"DIV3" => "DivX",
        b"MPG1" => "MPEG-1",
        b"MP2V" | b"MPG2" => "MPEG-2",
        b"MJPG" => "MJPEG",
        b"WMV1" | b"WMV2" | b"WMV3" => "WMV",
        b"WVC1" => "VC-1",
        b"H263" => "H.263",
        b"DVSD" | b"DV25" | b"DV50" => "DV",
        _ => {
            return fourcc
                .iter()
                .all(|b| b.is_ascii_graphic())
                .then(|| String::from_utf8_lossy(&fourcc).trim().to_string())
        }
    };
    Some(name.into())
}

pub(crate) fn audio_codec_name(guid: &GUID) -> Option<String> {
    if *guid == DOLBY_AC3 {
        return Some("AC-3".into());
    }
    if *guid == DOLBY_DDPLUS {
        return Some("E-AC-3".into());
    }
    let tag = fourcc_base(guid)?;
    let name = match tag {
        0x0001 => "PCM",
        0x0003 => "PCM float",
        0x0050 => "MPEG Audio",
        0x0055 => "MP3",
        0x0092 | 0x2000 => "AC-3",
        0x0160 | 0x0161 => "WMA",
        0x0162 => "WMA Pro",
        0x0163 => "WMA Lossless",
        0x1600 | 0x1610 => "AAC",
        0x2001 => "DTS",
        0x6C61 => "ALAC",
        0x704F => "Opus",
        0xF1AC => "FLAC",
        0x674F | 0x6750 | 0x6751 | 0x676F | 0x6770 | 0x6771 => "Vorbis",
        _ => return Some(format!("0x{:04X}", tag)),
    };
    Some(name.into())
}

pub fn analyze_video(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<VideoAnalysis, VideoError> {
    let _com =
        mf_scope().ok_or_else(|| VideoError::Failed("Media Foundation is unavailable".into()))?;
    let check = || {
        if cancelled() {
            Err(VideoError::Cancelled)
        } else {
            Ok(())
        }
    };
    check()?;

    unsafe {
        let reader = open_reader(path, true)?;

        let format = if set_output_format(&reader, &MFVideoFormat_RGB32).is_ok() {
            PixelFormat::Rgb32
        } else {
            set_output_format(&reader, &MFVideoFormat_NV12)?;
            PixelFormat::Nv12
        };

        let geometry = FrameGeometry::current(&reader, format)
            .ok_or_else(|| VideoError::Failed("no output media type".into()))?;

        let duration_hns = duration_hns(&reader);

        let mut hashes = [None; 3];
        for (hash, quarter) in hashes.iter_mut().zip([1u64, 2, 3]) {
            check()?;
            *hash = grab_frame_dhash(&reader, duration_hns * quarter / 4, &geometry);
        }
        let [dhash_25, dhash_mid, dhash_75] = hashes;
        let fingerprint = match (dhash_25, dhash_mid, dhash_75) {
            // Whole seconds cut down, as the fingerprints already in use were made.
            (Some(a), Some(b), Some(c)) => Some(format!(
                "{}s_{:016x}_{:016x}_{:016x}",
                duration_hns / 10_000_000,
                a,
                b,
                c
            )),
            _ => None,
        };

        Ok(VideoAnalysis {
            // Rounded like `Video_Duration` shows it.
            duration_sec: ((duration_hns + 5_000_000) / 10_000_000) as u32,
            width: geometry.width,
            height: geometry.height,
            dhash_mid,
            fingerprint,
        })
    }
}

/// Decodes the frame at `fraction` (0..1) of the duration as RGBA — for thumbnails. The source
/// reader lands on the key frame before that position, which is fine for a preview.
pub fn video_frame_rgba(path: &Path, fraction: f64) -> Option<image::RgbaImage> {
    let _com = mf_scope()?;
    unsafe {
        let reader = open_reader(path, true).ok()?;
        set_output_format(&reader, &MFVideoFormat_RGB32).ok()?;
        let geo = FrameGeometry::current(&reader, PixelFormat::Rgb32)?;
        let (width, height) = (geo.width, geo.height);
        if width == 0
            || height == 0
            || (width as u64) * (height as u64) > crate::image_decode::MAX_PIXELS
        {
            return None;
        }
        let at = (duration_hns(&reader) as f64 * fraction.clamp(0.0, 1.0)) as i64;
        set_position(&reader, at).ok()?;
        let buffer = next_sample(&reader)?.ConvertToContiguousBuffer().ok()?;
        with_linear_frame(&buffer, &geo, |frame| {
            let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
            for y in 0..height {
                rgba.extend(
                    frame
                        .row(y)?
                        .chunks_exact(4)
                        .flat_map(|px| [px[2], px[1], px[0], 255]),
                );
            }
            image::RgbaImage::from_raw(width, height, rgba)
        })
    }
}

/// The next decoded sample of the video stream. The first reads after a seek may carry no
/// sample (stream tick, format change); `None` at the end of the stream or on an error. The flags
/// argument is mandatory: a synchronous reader fails with `E_POINTER` without it.
pub(crate) unsafe fn next_sample(reader: &IMFSourceReader) -> Option<IMFSample> {
    for _ in 0..16 {
        let (mut flags, mut sample) = (0u32, None);
        reader
            .ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample))
            .ok()?;
        if sample.is_some() {
            return sample;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return None;
        }
    }
    None
}

unsafe fn set_output_format(reader: &IMFSourceReader, subtype: &GUID) -> windows::core::Result<()> {
    let media_type = MFCreateMediaType()?;
    media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
    reader.SetCurrentMediaType(STREAM, None, &media_type)
}

#[derive(Clone, Copy, PartialEq)]
enum PixelFormat {
    Rgb32,
    /// Only the Y plane (1 byte per pixel) is sampled.
    Nv12,
}

impl PixelFormat {
    fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb32 => 4,
            PixelFormat::Nv12 => 1,
        }
    }
}

struct FrameGeometry {
    width: u32,
    height: u32,
    default_stride: i32,
    format: PixelFormat,
}

impl FrameGeometry {
    /// Size and stride of the reader's current output type.
    unsafe fn current(reader: &IMFSourceReader, format: PixelFormat) -> Option<Self> {
        let current = reader.GetCurrentMediaType(STREAM).ok()?;
        let frame_size = current.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        Some(Self {
            width: (frame_size >> 32) as u32,
            height: frame_size as u32,
            // MF_MT_DEFAULT_STRIDE is a signed value stored as UINT32; negative means bottom-up.
            default_stride: current
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|s| s as i32)
                .unwrap_or(0),
            format,
        })
    }
}

unsafe fn grab_frame_dhash(
    reader: &IMFSourceReader,
    timestamp_hns: u64,
    geo: &FrameGeometry,
) -> Option<u64> {
    if geo.width < 9 || geo.height < 8 {
        return None;
    }

    set_position(reader, timestamp_hns as i64).ok()?;
    let buffer = next_sample(reader)?.ConvertToContiguousBuffer().ok()?;

    if let Ok(buf2d) = buffer.cast::<IMF2DBuffer2>() {
        let (mut scanline0, mut pitch) = (std::ptr::null_mut(), 0i32);
        let (mut start, mut len) = (std::ptr::null_mut(), 0u32);
        if buf2d
            .Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut scanline0,
                &mut pitch,
                &mut start,
                &mut len,
            )
            .is_ok()
        {
            let hash = if start.is_null() || scanline0 < start {
                None
            } else {
                let data = std::slice::from_raw_parts(start, len as usize);
                let top = scanline0.offset_from(start) as usize;
                FrameView {
                    data,
                    top,
                    pitch: pitch as isize,
                    geo,
                }
                .dhash()
            };
            let _ = buf2d.Unlock2D();
            return hash;
        }
    }
    with_linear_frame(&buffer, geo, |frame| frame.dhash())
}

/// Locks a buffer without 2D access and hands its frame to `f`; rows are laid out by the
/// default stride (the tightly packed row size if the type has none).
unsafe fn with_linear_frame<T>(
    buffer: &IMFMediaBuffer,
    geo: &FrameGeometry,
    f: impl FnOnce(&FrameView) -> Option<T>,
) -> Option<T> {
    let mut ptr = std::ptr::null_mut();
    let mut cur_len = 0u32;
    buffer.Lock(&mut ptr, None, Some(&mut cur_len)).ok()?;
    let result = if ptr.is_null() {
        None
    } else {
        let data = std::slice::from_raw_parts(ptr, cur_len as usize);
        let row = geo.width as usize * geo.format.bytes_per_pixel();
        let pitch = if geo.default_stride != 0 {
            geo.default_stride as isize
        } else {
            row as isize
        };
        // Bottom-up images start with the last row in memory.
        let top = if pitch < 0 {
            (geo.height as usize - 1) * pitch.unsigned_abs()
        } else {
            0
        };
        f(&FrameView {
            data,
            top,
            pitch,
            geo,
        })
    };
    let _ = buffer.Unlock();
    result
}

/// A locked frame: `data` is the whole buffer, `top` the offset of the top row, `pitch` the
/// signed distance between rows. Every sample is bounds-checked against `data`.
struct FrameView<'a> {
    data: &'a [u8],
    top: usize,
    pitch: isize,
    geo: &'a FrameGeometry,
}

impl FrameView<'_> {
    /// Row `y` (0 = top) of `width` pixels.
    fn row(&self, y: u32) -> Option<&[u8]> {
        let start = usize::try_from(self.top as isize + y as isize * self.pitch).ok()?;
        let len = self.geo.width as usize * self.geo.format.bytes_per_pixel();
        self.data.get(start..start.checked_add(len)?)
    }

    fn luma(&self, x: u32, y: u32) -> Option<u8> {
        let bpp = self.geo.format.bytes_per_pixel();
        let px = self.row(y)?.get(x as usize * bpp..(x as usize + 1) * bpp)?;
        Some(match self.geo.format {
            PixelFormat::Rgb32 => {
                let (b, g, r) = (px[0] as f32, px[1] as f32, px[2] as f32);
                (0.299 * r + 0.587 * g + 0.114 * b) as u8
            }
            PixelFormat::Nv12 => px[0],
        })
    }

    /// dHash over a 9x8 grid of point samples.
    fn dhash(&self) -> Option<u64> {
        let (w, h) = (self.geo.width, self.geo.height);
        let mut grid = [[0u8; 9]; 8];
        for (gy, row) in grid.iter_mut().enumerate() {
            let y = ((gy as u32 * 2 + 1) * h / 16).min(h - 1);
            for (gx, cell) in row.iter_mut().enumerate() {
                let x = ((gx as u32 * 2 + 1) * w / 18).min(w - 1);
                *cell = self.luma(x, y)?;
            }
        }
        Some(
            grid.iter()
                .flat_map(|row| row.windows(2))
                .fold(0u64, |hash, pair| (hash << 1) | (pair[0] > pair[1]) as u64),
        )
    }
}

pub(crate) unsafe fn set_position(reader: &IMFSourceReader, hns: i64) -> windows::core::Result<()> {
    let mut position = PROPVARIANT::default();
    {
        let inner = &mut *position.Anonymous.Anonymous;
        inner.vt = VT_I8;
        inner.Anonymous.hVal = hns.max(0);
    }
    reader.SetCurrentPosition(&GUID::zeroed(), &position)
}

pub(crate) const HNS_PER_SEC: f64 = 10_000_000.0;
unsafe fn propvariant_u64(var: &PROPVARIANT) -> Option<u64> {
    let inner = &var.Anonymous.Anonymous;
    (inner.vt == VT_UI8 || inner.vt == VT_I8).then(|| inner.Anonymous.uhVal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 6 s of 8x8 gray blocks whose layout changes every second; `seed` makes another video.
    fn blocks_video(name: &str, seed: u32) -> std::path::PathBuf {
        crate::test_util::gray_avi(name, (64, 48), 10, 60, |frame, x, y| {
            let second = frame / 10;
            ((x / 8 * 7 + y / 8 * 13 + second * 29 + seed * 53) * 37 % 256) as u8
        })
    }

    /// 6 s of per-pixel noise that changes every second; `seed` makes another video.
    fn noise_video(name: &str, seed: u32) -> std::path::PathBuf {
        crate::test_util::gray_avi(name, (64, 48), 10, 60, |frame, x, y| {
            // murmur3's finalizer over (x, y, second, seed).
            let mut h = x | y << 8 | (frame / 10) << 16 | seed << 24;
            h ^= h >> 16;
            h = h.wrapping_mul(0x85eb_ca6b);
            h ^= h >> 13;
            h = h.wrapping_mul(0xc2b2_ae35);
            (h ^ h >> 16) as u8
        })
    }

    /// Regression: the frames were read with a null flags pointer, which a synchronous source
    /// reader rejects (`E_POINTER`), so every video hashed to zeros and all videos of the same
    /// length looked like duplicates.
    #[test]
    fn videos_get_real_distinct_hashes() {
        let (a, b) = (noise_video("hash_a", 0), noise_video("hash_b", 1));
        let analyze = |p: &Path| analyze_video(p, &|| false).expect("analysis");
        let (first, again, other) = (analyze(&a), analyze(&a), analyze(&b));
        let _ = (std::fs::remove_file(&a), std::fs::remove_file(&b));

        assert_eq!((first.duration_sec, first.width, first.height), (6, 64, 48));
        let fingerprint = first.fingerprint.clone().expect("all three frames decoded");
        let parts: Vec<&str> = fingerprint.split('_').collect();
        assert_eq!(parts.len(), 4, "{fingerprint}");
        assert_eq!(parts[0], "6s");
        // The frames at 25 / 50 / 75 % come from different seconds, so they differ.
        assert!(
            parts[1] != parts[2] && parts[2] != parts[3] && parts[1] != parts[3],
            "{fingerprint}"
        );
        assert!(
            parts[1..].iter().all(|h| *h != "0000000000000000"),
            "{fingerprint}"
        );
        assert_eq!(first.dhash_mid_hex().as_deref(), Some(parts[2]));

        assert_eq!(
            again.fingerprint.as_deref(),
            Some(fingerprint.as_str()),
            "stable"
        );
        let other = other.fingerprint.expect("other video decoded");
        assert!(other.starts_with("6s_"));
        assert_ne!(other, fingerprint, "a different video of the same length");
    }

    #[test]
    fn thumbnail_frame_is_decoded() {
        let path = blocks_video("thumb", 2);
        let frame = video_frame_rgba(&path, 0.5);
        let _ = std::fs::remove_file(&path);
        let frame = frame.expect("frame");
        assert_eq!(frame.dimensions(), (64, 48));
        // Second 3, block (0, 0): (3 * 29 + 2 * 53) * 37 % 256.
        let expected = ((3 * 29 + 2 * 53) * 37 % 256) as u8;
        let px = frame.get_pixel(3, 3).0;
        assert!(
            px[..3].iter().all(|&c| c.abs_diff(expected) <= 2),
            "{px:?} vs {expected}"
        );
    }

    #[test]
    fn codec_names() {
        use windows::Win32::Media::MediaFoundation::{
            MFAudioFormat_AAC, MFVideoFormat_H264, MFVideoFormat_HEVC,
        };
        assert_eq!(
            video_codec_name(&MFVideoFormat_H264).as_deref(),
            Some("H.264")
        );
        assert_eq!(
            video_codec_name(&MFVideoFormat_HEVC).as_deref(),
            Some("HEVC")
        );
        assert_eq!(video_codec_name(&MPEG2_VIDEO).as_deref(), Some("MPEG-2"));
        assert_eq!(
            video_codec_name(&GUID::from_u128(0x5a5a5a5a_0000_0010_8000_00aa00389b71)).as_deref(),
            Some("ZZZZ")
        );
        assert_eq!(video_codec_name(&GUID::zeroed()), None);
        assert_eq!(audio_codec_name(&MFAudioFormat_AAC).as_deref(), Some("AAC"));
        assert_eq!(audio_codec_name(&DOLBY_AC3).as_deref(), Some("AC-3"));
        assert_eq!(
            audio_codec_name(&GUID::from_u128(0x00001234_0000_0010_8000_00aa00389b71)).as_deref(),
            Some("0x1234")
        );
    }

    fn geo(stride: i32) -> FrameGeometry {
        FrameGeometry {
            width: 18,
            height: 16,
            default_stride: stride,
            format: PixelFormat::Nv12,
        }
    }

    #[test]
    fn bottom_up_frame_matches_top_down() {
        let g = geo(18);
        let top_down: Vec<u8> = (0..18 * 16).map(|i| ((i * 37) % 251) as u8).collect();
        let bottom_up: Vec<u8> = top_down.chunks(18).rev().flatten().copied().collect();

        let a = FrameView {
            data: &top_down,
            top: 0,
            pitch: 18,
            geo: &g,
        }
        .dhash();
        let b = FrameView {
            data: &bottom_up,
            top: 15 * 18,
            pitch: -18,
            geo: &g,
        }
        .dhash();
        assert!(a.is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn short_buffer_is_rejected() {
        let g = geo(18);
        let data = vec![0u8; 18 * 4];
        assert_eq!(
            FrameView {
                data: &data,
                top: 0,
                pitch: 18,
                geo: &g
            }
            .dhash(),
            None
        );
    }
}
