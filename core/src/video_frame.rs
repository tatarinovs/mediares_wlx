//! Video metadata and keyframe hashing via Windows Media Foundation (IMFSourceReader).

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFAttributes, IMFMediaBuffer, IMFSourceReader, MF2DBuffer_LockFlags_Read,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video,
    MFVideoFormat_NV12, MFVideoFormat_RGB32, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE,
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

pub fn analyze_video(path: &Path, cancelled: &dyn Fn() -> bool) -> Result<VideoAnalysis, VideoError> {
    let _com = ComScope::new();
    if !ensure_mf_started() {
        return Err(VideoError::Failed("Media Foundation is unavailable".into()));
    }
    let check = || if cancelled() { Err(VideoError::Cancelled) } else { Ok(()) };
    check()?;

    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    unsafe {
        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.ok_or_else(|| VideoError::Failed("no attributes".into()))?;
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;

        let reader = MFCreateSourceReaderFromURL(PCWSTR(path_wide.as_ptr()), Some(&attributes))?;
        let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
        reader.SetStreamSelection(STREAM, true)?;

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

        let duration_var = reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)?;
        let duration_hns = propvariant_u64(&duration_var).unwrap_or(0);
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

    let mut position = PROPVARIANT::default();
    {
        let inner = &mut *position.Anonymous.Anonymous;
        inner.vt = VT_I8;
        inner.Anonymous.hVal = timestamp_hns as i64;
    }
    reader.SetCurrentPosition(&GUID::zeroed(), &position).ok()?;

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
