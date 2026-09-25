//! Video metadata and keyframe hashing via Windows Media Foundation (IMFSourceReader); also the
//! playability check for audio that only Media Foundation can decode.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFAttributes, IMFMediaBuffer, IMFSourceReader, MF2DBuffer_LockFlags_Read,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video,
    MFVideoFormat_NV12, MFVideoFormat_RGB32, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MFSampleExtension_CleanPoint, MF_SOURCE_READERF_ENDOFSTREAM,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::{VT_I8, VT_UI8};

use crate::mf_init::{ensure_mf_started, ComScope};

#[derive(Debug, Clone)]
pub struct VideoAnalysis {
    pub duration_sec: u32,
    pub width: u32,
    pub height: u32,
    pub dhash_mid: u64,
    pub fingerprint: String,
}

impl VideoAnalysis {
    pub fn dhash_mid_hex(&self) -> String {
        format!("{:016x}", self.dhash_mid)
    }

    pub fn dimensions_str(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

/// Basic stream properties, cheap to obtain (no decoding).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration_sec: f64,
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

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Opens a source reader restricted to the first video stream.
unsafe fn open_reader(path: &Path, video_processing: bool) -> windows::core::Result<IMFSourceReader> {
    open_reader_for(path, STREAM, video_processing)
}

unsafe fn open_reader_for(path: &Path, stream: u32, video_processing: bool) -> windows::core::Result<IMFSourceReader> {
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut attributes: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attributes, 1)?;
    let attributes = attributes.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
    if video_processing {
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
    }
    let reader = MFCreateSourceReaderFromURL(PCWSTR(path_wide.as_ptr()), Some(&attributes))?;
    let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
    reader.SetStreamSelection(stream, true)?;
    Ok(reader)
}

unsafe fn duration_hns(reader: &IMFSourceReader) -> u64 {
    reader
        .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        .ok()
        .and_then(|v| propvariant_u64(&v))
        .unwrap_or(0)
}

/// Checks that Media Foundation can open the file and has a video stream; returns its properties.
pub fn probe_video(path: &Path) -> Option<VideoInfo> {
    let _com = ComScope::new();
    if !ensure_mf_started() {
        return None;
    }
    unsafe {
        let reader = open_reader(path, false).ok()?;
        let native = reader.GetNativeMediaType(STREAM, 0).ok()?;
        let frame_size = native.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        Some(VideoInfo {
            width: (frame_size >> 32) as u32,
            height: frame_size as u32,
            duration_sec: duration_hns(&reader) as f64 / 10_000_000.0,
        })
    }
}

/// Checks that Media Foundation can open the file and has an audio stream; returns the duration
/// in seconds.
pub fn probe_audio(path: &Path) -> Option<f64> {
    let _com = ComScope::new();
    if !ensure_mf_started() {
        return None;
    }
    unsafe {
        let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let reader = open_reader_for(path, stream, false).ok()?;
        reader.GetNativeMediaType(stream, 0).ok()?;
        Some(duration_hns(&reader) as f64 / 10_000_000.0)
    }
}

pub fn analyze_video(path: &Path, cancelled: &dyn Fn() -> bool) -> Result<VideoAnalysis, VideoError> {
    let _com = ComScope::new();
    if !ensure_mf_started() {
        return Err(VideoError::Failed("Media Foundation is unavailable".into()));
    }
    let check = || if cancelled() { Err(VideoError::Cancelled) } else { Ok(()) };
    check()?;

    unsafe {
        let reader = open_reader(path, true)?;

        let format = if set_output_format(&reader, &MFVideoFormat_RGB32).is_ok() {
            PixelFormat::Rgb32
        } else {
            set_output_format(&reader, &MFVideoFormat_NV12)?;
            PixelFormat::Nv12
        };

        let current = reader.GetCurrentMediaType(STREAM)?;
        let frame_size = current.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let geometry = FrameGeometry {
            width: (frame_size >> 32) as u32,
            height: frame_size as u32,
            // MF_MT_DEFAULT_STRIDE is a signed value stored as UINT32; negative means bottom-up.
            default_stride: current.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or(0),
            format,
        };

        let duration_hns = duration_hns(&reader);
        let duration_sec = (duration_hns / 10_000_000) as u32;

        let mut hashes = [0u64; 3];
        for (hash, quarter) in hashes.iter_mut().zip([1u64, 2, 3]) {
            check()?;
            *hash = grab_frame_dhash(&reader, duration_hns * quarter / 4, &geometry).unwrap_or(0);
        }
        let [dhash_25, dhash_mid, dhash_75] = hashes;

        Ok(VideoAnalysis {
            duration_sec,
            width: geometry.width,
            height: geometry.height,
            dhash_mid,
            fingerprint: format!("{}s_{:016x}_{:016x}_{:016x}", duration_sec, dhash_25, dhash_mid, dhash_75),
        })
    }
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

unsafe fn grab_frame_dhash(reader: &IMFSourceReader, timestamp_hns: u64, geo: &FrameGeometry) -> Option<u64> {
    if geo.width < 9 || geo.height < 8 {
        return None;
    }

    set_position(reader, timestamp_hns as i64).ok()?;

    let mut sample = None;
    reader.ReadSample(STREAM, 0, None, None, None, Some(&mut sample)).ok()?;
    let buffer = sample?.ConvertToContiguousBuffer().ok()?;

    if let Ok(buf2d) = buffer.cast::<IMF2DBuffer2>() {
        let (mut scanline0, mut pitch) = (std::ptr::null_mut(), 0i32);
        let (mut start, mut len) = (std::ptr::null_mut(), 0u32);
        if buf2d.Lock2DSize(MF2DBuffer_LockFlags_Read, &mut scanline0, &mut pitch, &mut start, &mut len).is_ok() {
            let hash = if start.is_null() || scanline0 < start {
                None
            } else {
                let data = std::slice::from_raw_parts(start, len as usize);
                let top = scanline0.offset_from(start) as usize;
                FrameView { data, top, pitch: pitch as isize, geo }.dhash()
            };
            let _ = buf2d.Unlock2D();
            return hash;
        }
    }
    hash_linear_buffer(&buffer, geo)
}

unsafe fn hash_linear_buffer(buffer: &IMFMediaBuffer, geo: &FrameGeometry) -> Option<u64> {
    let mut ptr = std::ptr::null_mut();
    let mut cur_len = 0u32;
    buffer.Lock(&mut ptr, None, Some(&mut cur_len)).ok()?;
    let hash = if ptr.is_null() {
        None
    } else {
        let data = std::slice::from_raw_parts(ptr, cur_len as usize);
        let row = geo.width as usize * geo.format.bytes_per_pixel();
        let pitch = if geo.default_stride != 0 { geo.default_stride as isize } else { row as isize };
        // Bottom-up images start with the last row in memory.
        let top = if pitch < 0 { (geo.height as usize - 1) * pitch.unsigned_abs() } else { 0 };
        FrameView { data, top, pitch, geo }.dhash()
    };
    let _ = buffer.Unlock();
    hash
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
    fn luma(&self, x: u32, y: u32) -> Option<u8> {
        let bpp = self.geo.format.bytes_per_pixel();
        let offset = self.top as isize + y as isize * self.pitch + (x as usize * bpp) as isize;
        let offset = usize::try_from(offset).ok()?;
        match self.geo.format {
            PixelFormat::Rgb32 => {
                let px = self.data.get(offset..offset + 3)?;
                let (b, g, r) = (px[0] as f32, px[1] as f32, px[2] as f32);
                Some((0.299 * r + 0.587 * g + 0.114 * b) as u8)
            }
            PixelFormat::Nv12 => self.data.get(offset).copied(),
        }
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
        Some(grid.iter().flat_map(|row| row.windows(2)).fold(0u64, |hash, pair| (hash << 1) | (pair[0] > pair[1]) as u64))
    }
}

unsafe fn set_position(reader: &IMFSourceReader, hns: i64) -> windows::core::Result<()> {
    let mut position = PROPVARIANT::default();
    {
        let inner = &mut *position.Anonymous.Anonymous;
        inner.vt = VT_I8;
        inner.Anonymous.hVal = hns.max(0);
    }
    reader.SetCurrentPosition(&GUID::zeroed(), &position)
}

const HNS_PER_SEC: f64 = 10_000_000.0;
/// Upper bound of compressed samples scanned for one step (a very long GOP at high fps).
const MAX_SCAN_SAMPLES: usize = 3000;

/// Locates key frames (clean points) of the first video stream by reading compressed samples —
/// nothing is decoded, so a step costs a few milliseconds.
pub struct KeyframeIndex {
    reader: IMFSourceReader,
    _com: ComScope,
}

impl KeyframeIndex {
    pub fn open(path: &Path) -> Option<Self> {
        let com = ComScope::new();
        if !ensure_mf_started() {
            return None;
        }
        // No output type is set, so samples stay compressed.
        let reader = unsafe { open_reader(path, false) }.ok()?;
        Some(Self { reader, _com: com })
    }

    /// Next sample; `None` at the end of the stream. Returns (time in seconds, is key frame).
    unsafe fn read(&self) -> Option<(f64, bool)> {
        let mut flags = 0u32;
        let mut sample = None;
        self.reader.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample)).ok()?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return None;
        }
        let sample = sample?;
        let time = sample.GetSampleTime().ok()? as f64 / HNS_PER_SEC;
        let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        Some((time, key))
    }

    /// Seeks the reader to `seconds` and returns the first key frame at or after where it landed.
    /// Sources position on the key frame preceding the requested time.
    unsafe fn key_frame_from(&self, seconds: f64) -> Option<f64> {
        set_position(&self.reader, (seconds.max(0.0) * HNS_PER_SEC) as i64).ok()?;
        (0..MAX_SCAN_SAMPLES).map_while(|_| self.read()).find(|&(_, key)| key).map(|(t, _)| t)
    }

    /// First key frame strictly after `seconds`.
    pub fn next_after(&self, seconds: f64) -> Option<f64> {
        unsafe {
            set_position(&self.reader, (seconds.max(0.0) * HNS_PER_SEC) as i64).ok()?;
            (0..MAX_SCAN_SAMPLES)
                .map_while(|_| self.read())
                .find(|&(t, key)| key && t > seconds)
                .map(|(t, _)| t)
        }
    }

    /// Last key frame strictly before `seconds` (0 if there is none).
    pub fn previous_before(&self, seconds: f64) -> Option<f64> {
        let mut probe = seconds;
        // Probing just before the key frame we landed on normally yields the preceding one; the
        // step only grows if the source keeps returning the same frame.
        let mut back = 0.001;
        for _ in 0..16 {
            if probe <= 0.0 {
                return Some(0.0);
            }
            let found = unsafe { self.key_frame_from(probe) }?;
            if found < seconds {
                return Some(found);
            }
            probe = probe.min(found) - back;
            back = (back * 4.0).max(0.25);
        }
        None
    }
}

unsafe fn propvariant_u64(var: &PROPVARIANT) -> Option<u64> {
    let inner = &var.Anonymous.Anonymous;
    (inner.vt == VT_UI8 || inner.vt == VT_I8).then(|| inner.Anonymous.uhVal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(stride: i32) -> FrameGeometry {
        FrameGeometry { width: 18, height: 16, default_stride: stride, format: PixelFormat::Nv12 }
    }

    #[test]
    fn bottom_up_frame_matches_top_down() {
        let g = geo(18);
        let top_down: Vec<u8> = (0..18 * 16).map(|i| ((i * 37) % 251) as u8).collect();
        let bottom_up: Vec<u8> = top_down.chunks(18).rev().flatten().copied().collect();

        let a = FrameView { data: &top_down, top: 0, pitch: 18, geo: &g }.dhash();
        let b = FrameView { data: &bottom_up, top: 15 * 18, pitch: -18, geo: &g }.dhash();
        assert!(a.is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn short_buffer_is_rejected() {
        let g = geo(18);
        let data = vec![0u8; 18 * 4];
        assert_eq!(FrameView { data: &data, top: 0, pitch: 18, geo: &g }.dhash(), None);
    }
}
