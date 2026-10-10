//! Key frames of a video file, read from the compressed stream with Media Foundation: where they
//! are (for key-frame seeking) and, for transport streams, what they show.
//!
//! Containers with an index (MP4, Matroska, AVI...) flag key frames as clean points and their
//! sources seek straight to them. A transport stream has no index: Media Foundation's source
//! flags every frame as a clean point and seeks by a bitrate estimate, which on recordings with
//! timestamp gaps lands up to a minute early. Key frames are recognised in the bitstream there,
//! and the player shows them decoded from here while its own exact seeks take seconds (as mpv
//! scrubs by key frames).

use std::cell::{OnceCell, RefCell};
use std::mem::ManuallyDrop;
use std::path::Path;

use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFMediaBuffer, IMFSample, IMFSourceReader, IMFTransform,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFSampleExtension_CleanPoint, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_NV12, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_LOCALMFT,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_FRAME_SIZE,
    MF_MT_SUBTYPE,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::mf_init::{mf_scope, ComScope};
use crate::video_frame::{
    next_sample, open_reader, set_position, video_codec_name, HNS_PER_SEC, STREAM,
};

/// Upper bound of frames scanned for one lookup: a very long group of pictures at a high frame
/// rate, or a transport stream seek landing well before the requested time.
const MAX_SCAN_FRAMES: usize = 3000;
/// Samples without a time read in a row before the stream counts as ended.
const MAX_UNTIMED_SAMPLES: usize = 64;
/// A key frame's access unit larger than this is not gathered (an 8K intra frame is a few MB).
const MAX_KEY_FRAME_BYTES: usize = 32 << 20;

/// A key frame found by [`KeyframeIndex`].
pub struct KeyFrame {
    /// Seconds, on the timeline the media engine plays.
    pub time: f64,
    /// The compressed access unit, kept for transport streams ([`KeyframeIndex::picture`]).
    data: Option<Vec<u8>>,
}

/// A decoded key frame, top-down BGRA.
pub struct KeyPicture {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Locates key frames of the first video stream by reading compressed samples: nothing is
/// decoded, so a lookup costs a few milliseconds (transport streams: tens of milliseconds, as
/// their source lands far before the requested time).
pub struct KeyframeIndex {
    reader: IMFSourceReader,
    /// Transport streams: key frames are recognised in this bitstream, the source's clean point
    /// flag can't be trusted.
    bitstream: Option<KeyCodec>,
    /// A timed sample read past the end of a key frame's data (see [`Self::read`]).
    pending: RefCell<Option<IMFSample>>,
    /// Opened on the first [`Self::picture`]; `None` inside: no decoder for this stream.
    decoder: OnceCell<Option<RefCell<KeyFrameDecoder>>>,
    _com: ComScope,
}

/// One frame of the compressed stream.
struct Frame {
    time: f64,
    key: bool,
    /// Key frames of transport streams: the whole access unit.
    data: Option<Vec<u8>>,
}

impl Frame {
    fn into_key(self) -> KeyFrame {
        KeyFrame {
            time: self.time,
            data: self.data,
        }
    }
}

impl KeyframeIndex {
    /// `None` if Media Foundation can't read the file, or it is a transport stream in a codec
    /// whose key frames aren't recognised.
    pub fn open(path: &Path) -> Option<Self> {
        let com = mf_scope()?;
        // No output type is set, so samples stay compressed.
        let reader = unsafe { open_reader(path, false) }.ok()?;
        let bitstream = if crate::probe::is_transport_stream(path) {
            let subtype = unsafe { reader.GetNativeMediaType(STREAM, 0) }
                .and_then(|t| unsafe { t.GetGUID(&MF_MT_SUBTYPE) })
                .ok()?;
            Some(KeyCodec::of(&subtype)?)
        } else {
            None
        };
        Some(Self {
            reader,
            bitstream,
            pending: RefCell::new(None),
            decoder: OnceCell::new(),
            _com: com,
        })
    }

    /// First key frame strictly after `seconds`.
    pub fn next_after(&self, seconds: f64) -> Option<KeyFrame> {
        unsafe { self.frames_from(seconds) }?
            .find(|f| f.key && f.time > seconds)
            .map(Frame::into_key)
    }

    /// Last key frame strictly before `seconds` (the first frame if there is none).
    pub fn previous_before(&self, seconds: f64) -> Option<KeyFrame> {
        let mut probe = seconds;
        // Probing just before where the source landed normally yields the preceding key frame;
        // the step only grows if the source keeps landing at or after `seconds`.
        let mut back = 0.001;
        for _ in 0..16 {
            if probe <= 0.0 {
                return unsafe { self.frames_from(0.0) }?
                    .find(|f| f.key)
                    .map(Frame::into_key);
            }
            let (mut landed, mut last_key) = (None, None);
            for frame in unsafe { self.frames_from(probe) }? {
                landed.get_or_insert(frame.time);
                if frame.time >= seconds {
                    break;
                }
                if frame.key {
                    last_key = Some(frame);
                }
            }
            if let Some(key) = last_key {
                return Some(key.into_key());
            }
            // Nothing read (past the end): just probe earlier.
            probe = landed.map_or(probe, |l: f64| probe.min(l)) - back;
            back = (back * 4.0).max(0.25);
        }
        None
    }

    /// What `key` shows; transport streams only (elsewhere the engine's own seeks are quick).
    pub fn picture(&self, key: &KeyFrame) -> Option<KeyPicture> {
        let data = key.data.as_deref()?;
        let decoder = self
            .decoder
            .get_or_init(|| unsafe { KeyFrameDecoder::new(&self.reader) }.map(RefCell::new))
            .as_ref()?;
        unsafe { decoder.borrow_mut().decode(data, key.time) }
    }

    /// Frames from `seconds` on: the source positions on or before the requested time (on the
    /// preceding key frame, or wherever a transport stream's estimate lands).
    unsafe fn frames_from(&self, seconds: f64) -> Option<impl Iterator<Item = Frame> + '_> {
        self.pending.borrow_mut().take();
        set_position(&self.reader, (seconds.max(0.0) * HNS_PER_SEC) as i64).ok()?;
        Some((0..MAX_SCAN_FRAMES).map_while(|_| self.read()))
    }

    /// Next frame; `None` at the end of the stream. A transport stream splits frames into
    /// several samples, only the first with a time: the rest are skipped, or for a key frame
    /// gathered into its data.
    unsafe fn read(&self) -> Option<Frame> {
        let first = self.pending.borrow_mut().take();
        let (sample, time) = match first {
            Some(sample) => {
                let time = sample.GetSampleTime().ok()?;
                (sample, time)
            }
            None => self.next_timed()?,
        };
        let time = time as f64 / HNS_PER_SEC;
        let Some(codec) = self.bitstream else {
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            return Some(Frame {
                time,
                key,
                data: None,
            });
        };
        let mut data = sample_bytes(&sample).unwrap_or_default();
        if !codec.is_key_frame(&data) {
            return Some(Frame {
                time,
                key: false,
                data: None,
            });
        }
        let mut complete = true;
        while let Some(next) = next_sample(&self.reader) {
            if next.GetSampleTime().is_ok() {
                *self.pending.borrow_mut() = Some(next);
                break;
            }
            if data.len() > MAX_KEY_FRAME_BYTES {
                // A damaged stream without times further on: the rest of the file would be read.
                complete = false;
                break;
            }
            data.extend(sample_bytes(&next).unwrap_or_default());
        }
        Some(Frame {
            time,
            key: true,
            data: complete.then_some(data),
        })
    }

    /// The next sample with a time, and the time.
    unsafe fn next_timed(&self) -> Option<(IMFSample, i64)> {
        for _ in 0..MAX_UNTIMED_SAMPLES {
            let sample = next_sample(&self.reader)?;
            if let Ok(time) = sample.GetSampleTime() {
                return Some((sample, time));
            }
        }
        None
    }
}

/// Bitstreams whose key frames are recognised.
#[derive(Clone, Copy, PartialEq, Debug)]
enum KeyCodec {
    H264,
    Hevc,
    Mpeg2,
}

impl KeyCodec {
    fn of(subtype: &GUID) -> Option<Self> {
        match video_codec_name(subtype)?.as_str() {
            "H.264" => Some(Self::H264),
            "HEVC" => Some(Self::Hevc),
            "MPEG-2" => Some(Self::Mpeg2),
            _ => None,
        }
    }

    /// The access unit starts a group of pictures: an IDR picture (H.264), an IRAP picture
    /// (HEVC), an I picture (MPEG-2).
    fn is_key_frame(self, data: &[u8]) -> bool {
        let mut units = start_codes(data);
        match self {
            Self::H264 => units.any(|(_, b)| b & 0x1f == 5),
            Self::Hevc => units.any(|(_, b)| (16..=21).contains(&((b >> 1) & 0x3f))),
            // Picture header: 10 bits temporal reference, then 3 bits picture coding type (1 = I).
            Self::Mpeg2 => units
                .any(|(at, code)| code == 0 && data.get(at + 2).is_some_and(|b| (b >> 3) & 7 == 1)),
        }
    }
}

/// Annex B NAL unit / MPEG start codes (`00 00 01`) of `data`: the offset and value of the byte
/// following each.
fn start_codes(data: &[u8]) -> impl Iterator<Item = (usize, u8)> + '_ {
    data.windows(4)
        .enumerate()
        .filter(|(_, w)| w[0] == 0 && w[1] == 0 && w[2] == 1)
        .map(|(i, w)| (i + 3, w[3]))
}

unsafe fn sample_bytes(sample: &IMFSample) -> Option<Vec<u8>> {
    let buffer = sample.ConvertToContiguousBuffer().ok()?;
    let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
    buffer.Lock(&mut ptr, None, Some(&mut len)).ok()?;
    let bytes = (!ptr.is_null()).then(|| std::slice::from_raw_parts(ptr, len as usize).to_vec());
    let _ = buffer.Unlock();
    bytes
}

/// A Media Foundation video decoder fed single key frames.
struct KeyFrameDecoder {
    mft: IMFTransform,
    /// Shown size.
    size: (u32, u32),
    /// Decoded frame as allocated (may be padded, e.g. 1088 lines for 1080); a contiguous
    /// buffer's stride is its width.
    frame: (u32, u32),
}

impl KeyFrameDecoder {
    /// A decoder for the reader's video stream, NV12 out.
    unsafe fn new(reader: &IMFSourceReader) -> Option<Self> {
        let native = reader.GetNativeMediaType(STREAM, 0).ok()?;
        let subtype = native.GetGUID(&MF_MT_SUBTYPE).ok()?;
        let size = unpack_size(native.GetUINT64(&MF_MT_FRAME_SIZE).ok()?);
        // Decoders register the plain subtypes rather than the transport stream's elementary
        // ones.
        let plain = match KeyCodec::of(&subtype) {
            Some(KeyCodec::H264) => MFVideoFormat_H264,
            Some(KeyCodec::Hevc) => MFVideoFormat_HEVC,
            _ => subtype,
        };
        for candidate in [subtype, plain] {
            let Some(mft) = find_decoder(&candidate) else {
                continue;
            };
            let input = MFCreateMediaType().ok()?;
            native.CopyAllItems(&input).ok()?;
            input.SetGUID(&MF_MT_SUBTYPE, &candidate).ok()?;
            if mft.SetInputType(0, &input, 0).is_err() {
                continue;
            }
            if let Ok(attributes) = mft.GetAttributes() {
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let mut decoder = Self {
                mft,
                size,
                frame: size,
            };
            if decoder.set_output_type() {
                let _ = decoder
                    .mft
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                return Some(decoder);
            }
        }
        None
    }

    /// Picks NV12 output (again after a format change).
    unsafe fn set_output_type(&mut self) -> bool {
        let nv12 = (0..)
            .map_while(|i| self.mft.GetOutputAvailableType(0, i).ok())
            .find(|t| t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12));
        let Some(nv12) = nv12 else { return false };
        if self.mft.SetOutputType(0, &nv12, 0).is_err() {
            return false;
        }
        let frame = unpack_size(nv12.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0));
        if frame.0 >= 2 && frame.1 >= 2 {
            self.frame = frame;
            self.size = if self.size.0 == 0 || self.size.1 == 0 {
                frame
            } else {
                (self.size.0.min(frame.0), self.size.1.min(frame.1))
            };
        }
        true
    }

    /// Decodes the access unit `data` on its own (a key frame needs nothing before it).
    unsafe fn decode(&mut self, data: &[u8], seconds: f64) -> Option<KeyPicture> {
        let mft = self.mft.clone();
        mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0).ok()?;
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        let input = MFCreateSample().ok()?;
        input.AddBuffer(&memory_buffer(data)?).ok()?;
        let _ = input.SetSampleTime((seconds * HNS_PER_SEC) as i64);
        let _ = input.SetUINT32(&MFSampleExtension_CleanPoint, 1);
        mft.ProcessInput(0, &input, 0).ok()?;
        // Draining makes the decoder hand the frame out rather than wait for the next ones.
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).ok()?;
        // A format change (the real frame size) may come first.
        for _ in 0..4 {
            match self.output() {
                Ok(sample) => return self.to_bgra(&sample),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    if !self.set_output_type() {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
        None
    }

    /// One decoded sample.
    unsafe fn output(&self) -> windows::core::Result<IMFSample> {
        let info = self.mft.GetOutputStreamInfo(0)?;
        let provides =
            (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
        let sample = if info.dwFlags & provides == 0 {
            let sample = MFCreateSample()?;
            sample.AddBuffer(&MFCreateMemoryBuffer(info.cbSize)?)?;
            Some(sample)
        } else {
            None
        };
        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = self.mft.ProcessOutput(0, &mut out, &mut status);
        let sample = ManuallyDrop::take(&mut out[0].pSample);
        drop(ManuallyDrop::take(&mut out[0].pEvents));
        result?;
        sample.ok_or_else(|| windows::Win32::Foundation::E_UNEXPECTED.into())
    }

    /// The NV12 `sample` as BGRA at the shown size.
    unsafe fn to_bgra(&self, sample: &IMFSample) -> Option<KeyPicture> {
        let buffer = sample.GetBufferByIndex(0).ok()?;
        let (width, height) = self.size;
        let rows = self.frame.1 as usize;
        let convert = |data: &[u8], pitch: usize| {
            let chroma = pitch * rows;
            (pitch >= width as usize
                && data.len() >= chroma + pitch * (height as usize).div_ceil(2))
            .then(|| nv12_to_bgra(data, pitch, chroma, width, height))
        };
        if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer>() {
            let (mut scan0, mut pitch) = (std::ptr::null_mut(), 0i32);
            if buffer2d.Lock2D(&mut scan0, &mut pitch).is_ok() {
                let picture = (pitch > 0 && !scan0.is_null())
                    .then(|| {
                        let len = pitch as usize * rows.div_ceil(2) * 3;
                        convert(std::slice::from_raw_parts(scan0, len), pitch as usize)
                    })
                    .flatten();
                let _ = buffer2d.Unlock2D();
                return picture;
            }
        }
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
        buffer.Lock(&mut ptr, None, Some(&mut len)).ok()?;
        let picture = (!ptr.is_null())
            .then(|| {
                let data = std::slice::from_raw_parts(ptr, len as usize);
                convert(data, self.frame.0 as usize)
            })
            .flatten();
        let _ = buffer.Unlock();
        picture
    }
}

/// `MF_MT_FRAME_SIZE`: width << 32 | height.
fn unpack_size(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

/// The first synchronous decoder taking `subtype`.
unsafe fn find_decoder(subtype: &GUID) -> Option<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *subtype,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        None,
        &mut activates,
        &mut count,
    )
    .ok()?;
    if activates.is_null() {
        return None;
    }
    let list = std::slice::from_raw_parts_mut(activates, count as usize);
    let mft = list
        .iter()
        .flatten()
        .find_map(|a| a.ActivateObject::<IMFTransform>().ok());
    for activate in list.iter_mut() {
        drop(activate.take());
    }
    CoTaskMemFree(Some(activates as *const _));
    mft
}

unsafe fn memory_buffer(bytes: &[u8]) -> Option<IMFMediaBuffer> {
    let buffer = MFCreateMemoryBuffer(u32::try_from(bytes.len()).ok()?).ok()?;
    let mut ptr = std::ptr::null_mut();
    buffer.Lock(&mut ptr, None, None).ok()?;
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    let _ = buffer.Unlock();
    buffer.SetCurrentLength(bytes.len() as u32).ok()?;
    Some(buffer)
}

/// Video range NV12 (`chroma`: offset of the interleaved UV plane) to BGRA: BT.709 for HD,
/// BT.601 below.
fn nv12_to_bgra(data: &[u8], pitch: usize, chroma: usize, width: u32, height: u32) -> KeyPicture {
    let (w, h) = (width as usize, height as usize);
    // R = Y + rv·V, G = Y - gu·U - gv·V, B = Y + bu·U, in 8.8 fixed point.
    let (rv, gu, gv, bu) = if h >= 720 {
        (459, 55, 136, 541)
    } else {
        (409, 100, 208, 516)
    };
    let mut bgra = vec![0u8; w * h * 4];
    for (y, out) in bgra.chunks_exact_mut(w * 4).enumerate() {
        let luma = &data[y * pitch..y * pitch + w];
        let uv_row = chroma + y / 2 * pitch;
        let uv = &data[uv_row..uv_row + w.div_ceil(2) * 2];
        for (x, px) in out.chunks_exact_mut(4).enumerate() {
            let c = 298 * (luma[x] as i32 - 16) + 128;
            let u = uv[x / 2 * 2] as i32 - 128;
            let v = uv[x / 2 * 2 + 1] as i32 - 128;
            px[0] = ((c + bu * u) >> 8).clamp(0, 255) as u8;
            px[1] = ((c - gu * u - gv * v) >> 8).clamp(0, 255) as u8;
            px[2] = ((c + rv * v) >> 8).clamp(0, 255) as u8;
            px[3] = 255;
        }
    }
    KeyPicture {
        width,
        height,
        bgra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Transport stream samples: an IDR access unit (AUD, SPS, PPS, IDR slice) versus a P frame.
    #[test]
    fn bitstream_key_frames() {
        let idr = [
            0, 0, 0, 1, 9, 0xf0, 0, 0, 0, 1, 0x67, 0x64, 0, 0, 1, 0x68, 0, 0, 1, 0x65, 0x88,
        ];
        let p = [
            0, 0, 0, 1, 9, 0xf0, 0, 0, 1, 0x41, 0x9a, 0, 0, 1, 0x41, 0x9b,
        ];
        assert!(KeyCodec::H264.is_key_frame(&idr));
        assert!(!KeyCodec::H264.is_key_frame(&p));
        // HEVC: NAL type 19 (IDR_W_RADL) vs 1 (TRAIL_R); the type sits in bits 1..7.
        assert!(KeyCodec::Hevc.is_key_frame(&[0, 0, 1, 19 << 1, 1, 0xaf]));
        assert!(!KeyCodec::Hevc.is_key_frame(&[0, 0, 1, 1 << 1, 1, 0xd0]));
        // MPEG-2 picture header: temporal reference 0, coding type 1 (I) / 2 (P).
        assert!(KeyCodec::Mpeg2.is_key_frame(&[0, 0, 1, 0, 0, 0x0f, 0xff]));
        assert!(!KeyCodec::Mpeg2.is_key_frame(&[0, 0, 1, 0, 0, 0x17, 0xff]));
    }

    /// Gray, white and black survive the conversion (video range: 16..235).
    #[test]
    fn nv12_levels() {
        // 2x2 luma 16 / 126 / 235 / 235, one neutral chroma pair.
        let data = [16, 126, 235, 235, 128, 128];
        let picture = nv12_to_bgra(&data, 2, 4, 2, 2);
        let px = |i: usize| &picture.bgra[i * 4..i * 4 + 3];
        assert_eq!(px(0), [0, 0, 0]);
        assert!(
            px(1).iter().all(|&c| (127..=129).contains(&c)),
            "{:?}",
            px(1)
        );
        assert_eq!(px(2), [255, 255, 255]);
    }
}
