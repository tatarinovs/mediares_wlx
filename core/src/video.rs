//! Video metadata and keyframe hashing via Windows Media Foundation.

use std::cell::RefCell;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFShutdown, MFStartup,
    MFVideoFormat_NV12, MFVideoFormat_RGB32, MFSTARTUP_NOSOCKET, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE, MF_VERSION,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

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

struct WmfScope {
    co_initialized: bool,
    mf_started: bool,
}

impl WmfScope {
    fn new() -> Self {
        unsafe {
            let co_initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            let mf_started = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).is_ok();
            WmfScope {
                co_initialized,
                mf_started,
            }
        }
    }
}

impl Drop for WmfScope {
    fn drop(&mut self) {
        unsafe {
            if self.mf_started {
                let _ = MFShutdown();
            }
            if self.co_initialized {
                CoUninitialize();
            }
        }
    }
}

thread_local! {
    static THREAD_WMF: RefCell<Option<WmfScope>> = const { RefCell::new(None) };
}

fn ensure_wmf_initialized() {
    THREAD_WMF.with(|cell| {
        let mut opt = cell.borrow_mut();
        if opt.is_none() {
            *opt = Some(WmfScope::new());
        }
    });
}

pub fn analyze_video(path: &Path) -> Result<VideoAnalysis, String> {
    ensure_wmf_initialized();

    if crate::wdx_api::is_stop_requested() {
        return Err("Operation cancelled by user".to_string());
    }

    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    unsafe {
        let mut attributes_opt: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attributes_opt, 1)
            .map_err(|e| format!("Failed to create attributes: {}", e))?;
        let attributes = attributes_opt.ok_or("Failed to create attributes")?;
        let _ = attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1);

        let reader: IMFSourceReader =
            MFCreateSourceReaderFromURL(PCWSTR(path_wide.as_ptr()), Some(&attributes))
                .map_err(|e| format!("Failed to create source reader: {}", e))?;

        let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
        reader
            .SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)
            .map_err(|e| format!("Failed to select video stream: {}", e))?;

        let media_type: IMFMediaType =
            MFCreateMediaType().map_err(|e| format!("Failed to create media type: {}", e))?;
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("SetGUID major type error: {}", e))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
            .map_err(|e| format!("SetGUID subtype error: {}", e))?;

        let is_rgb32 = match reader.SetCurrentMediaType(
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            None,
            &media_type,
        ) {
            Ok(_) => true,
            Err(_) => {
                let nv12_type: IMFMediaType = MFCreateMediaType()
                    .map_err(|e| format!("Failed to create NV12 type: {}", e))?;
                nv12_type
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .map_err(|e| format!("SetGUID major type error: {}", e))?;
                nv12_type
                    .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                    .map_err(|e| format!("SetGUID subtype error: {}", e))?;
                reader
                    .SetCurrentMediaType(
                        MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                        None,
                        &nv12_type,
                    )
                    .map_err(|e| format!("Failed to set output media type: {}", e))?;
                false
            }
        };

        let current_type = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
            .map_err(|e| format!("Failed to get current media type: {}", e))?;
        let frame_size = current_type.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let width = (frame_size >> 32) as u32;
        let height = (frame_size & 0xFFFFFFFF) as u32;

        let default_stride = current_type.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(0) as usize;

        let propvar = reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .map_err(|e| format!("Failed to get duration: {}", e))?;

        let duration_hns: u64 = extract_u64_from_propvariant(&propvar).unwrap_or(0);
        let duration_sec = (duration_hns / 10_000_000) as u32;

        if crate::wdx_api::is_stop_requested() {
            return Err("Operation cancelled by user".to_string());
        }
        let dhash_25 = grab_frame_dhash(
            &reader,
            duration_hns / 4,
            width,
            height,
            default_stride,
            is_rgb32,
        )
        .unwrap_or(0);

        if crate::wdx_api::is_stop_requested() {
            return Err("Operation cancelled by user".to_string());
        }
        let dhash_mid = grab_frame_dhash(
            &reader,
            duration_hns / 2,
            width,
            height,
            default_stride,
            is_rgb32,
        )
        .unwrap_or(0);

        if crate::wdx_api::is_stop_requested() {
            return Err("Operation cancelled by user".to_string());
        }
        let dhash_75 = grab_frame_dhash(
            &reader,
            (duration_hns * 3) / 4,
            width,
            height,
            default_stride,
            is_rgb32,
        )
        .unwrap_or(0);

        let fingerprint = format!(
            "{}s_{:016x}_{:016x}_{:016x}",
            duration_sec, dhash_25, dhash_mid, dhash_75
        );

        Ok(VideoAnalysis {
            duration_sec,
            width,
            height,
            dhash_mid,
            fingerprint,
        })
    }
}

unsafe fn grab_frame_dhash(
    reader: &IMFSourceReader,
    timestamp_hns: u64,
    width: u32,
    height: u32,
    default_stride: usize,
    is_rgb32: bool,
) -> Option<u64> {
    if width < 9 || height < 8 {
        return None;
    }

    let mut var_pos = PROPVARIANT::default();
    set_propvariant_i64(&mut var_pos, timestamp_hns as i64);
    reader.SetCurrentPosition(&GUID::zeroed(), &var_pos).ok()?;

    let mut actual_stream_index = 0u32;
    let mut stream_flags = 0u32;
    let mut timestamp = 0i64;
    let mut sample: Option<IMFSample> = None;

    let res = reader.ReadSample(
        MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
        0,
        Some(&mut actual_stream_index),
        Some(&mut stream_flags),
        Some(&mut timestamp),
        Some(&mut sample),
    );

    if res.is_err() {
        return None;
    }

    let sample = sample?;
    let buffer = sample.ConvertToContiguousBuffer().ok()?;

    if let Ok(buf2d) = buffer.cast::<IMF2DBuffer>() {
        let mut scanline0 = std::ptr::null_mut();
        let mut pitch = 0i32;
        if buf2d.Lock2D(&mut scanline0, &mut pitch).is_ok() && !scanline0.is_null() {
            let stride = pitch.unsigned_abs() as usize;
            let hash = compute_dhash_from_buffer(scanline0, width, height, stride, is_rgb32);
            let _ = buf2d.Unlock2D();
            return Some(hash);
        }
    }

    let mut data_ptr = std::ptr::null_mut();
    let mut max_len = 0u32;
    let mut cur_len = 0u32;

    buffer
        .Lock(&mut data_ptr, Some(&mut max_len), Some(&mut cur_len))
        .ok()?;

    let bytes_per_pixel = if is_rgb32 { 4 } else { 1 };
    let stride = if default_stride > 0 {
        default_stride
    } else {
        (width * bytes_per_pixel) as usize
    };
    let min_len = stride * (height as usize);

    let hash = if cur_len as usize >= min_len && !data_ptr.is_null() {
        Some(compute_dhash_from_buffer(
            data_ptr, width, height, stride, is_rgb32,
        ))
    } else {
        None
    };

    let _ = buffer.Unlock();
    hash
}

unsafe fn compute_dhash_from_buffer(
    ptr: *const u8,
    width: u32,
    height: u32,
    stride: usize,
    is_rgb32: bool,
) -> u64 {
    let mut luma_grid = [[0u8; 9]; 8];

    for gy in 0..8 {
        let py = ((gy * 2 + 1) * height / 16).min(height - 1) as usize;
        for gx in 0..9 {
            let px = ((gx * 2 + 1) * width / 18).min(width - 1) as usize;

            let luma = if is_rgb32 {
                let offset = py * stride + px * 4;
                let b = *ptr.add(offset) as f32;
                let g = *ptr.add(offset + 1) as f32;
                let r = *ptr.add(offset + 2) as f32;
                (0.299 * r + 0.587 * g + 0.114 * b) as u8
            } else {
                let offset = py * stride + px;
                *ptr.add(offset)
            };

            luma_grid[gy as usize][gx as usize] = luma;
        }
    }

    let mut hash: u64 = 0;
    for row in &luma_grid {
        for x in 0..8 {
            let left = row[x];
            let right = row[x + 1];
            hash = (hash << 1) | (if left > right { 1 } else { 0 });
        }
    }

    hash
}

unsafe fn extract_u64_from_propvariant(var: &PROPVARIANT) -> Option<u64> {
    let vt = std::ptr::read(var as *const PROPVARIANT as *const u16);
    if vt == 20 || vt == 21 {
        let val_ptr = (var as *const PROPVARIANT as *const u8).add(8) as *const u64;
        Some(*val_ptr)
    } else {
        None
    }
}

unsafe fn set_propvariant_i64(var: &mut PROPVARIANT, val: i64) {
    let vt_ptr = var as *mut PROPVARIANT as *mut u16;
    *vt_ptr = 20;
    let val_ptr = (var as *mut PROPVARIANT as *mut u8).add(8) as *mut i64;
    *val_ptr = val;
}
