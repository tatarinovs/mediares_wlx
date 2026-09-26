//! Audio through Media Foundation (`IMFSourceReader`) for files symphonia and lofty can't open
//! (WMA, AC3, ... — whatever decoders the system has): decoding to `f32` PCM for the duplicate
//! detection fields, and stream properties for WDX columns. The output depends on the installed
//! decoders, so the pure-Rust path stays the first choice.

use std::path::Path;

use windows::Win32::Media::MediaFoundation::{
    IMFSourceReader, MFAudioFormat_Float, MFCreateMediaType, MFMediaType_Audio,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
};

use crate::mf_init::{ensure_mf_started, ComScope};
use crate::video_frame::{audio_codec_name, duration_hns, open_reader_for};

const STREAM: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
/// Consecutive reads without samples (stream ticks, gaps) after which the stream is treated as finished.
const MAX_EMPTY_READS: u32 = 64;

/// Sequential decoder of the first audio stream to interleaved `f32` at the native rate and
/// channel count.
pub struct MfAudioReader {
    reader: IMFSourceReader,
    pub sample_rate: u32,
    pub channels: u16,
    out: Vec<f32>,
    /// Declared after `reader`, so COM is released after it.
    _com: ComScope,
}

impl MfAudioReader {
    /// `None` if Media Foundation can't open the file or decode its audio to float PCM.
    pub fn open(path: &Path) -> Option<Self> {
        let com = ComScope::new();
        if !ensure_mf_started() {
            return None;
        }
        unsafe {
            let reader = open_reader_for(path, STREAM, false).ok()?;
            let wanted = MFCreateMediaType().ok()?;
            wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).ok()?;
            wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float).ok()?;
            reader.SetCurrentMediaType(STREAM, None, &wanted).ok()?;
            let (sample_rate, channels) = float_format(&reader)?;
            Some(Self {
                reader,
                sample_rate,
                channels,
                out: Vec::new(),
                _com: com,
            })
        }
    }

    /// Decodes the next run of interleaved samples; `None` at the end of the stream, on an error
    /// or when the format changes mid-stream.
    pub fn next_chunk(&mut self) -> Option<&[f32]> {
        unsafe {
            for _ in 0..MAX_EMPTY_READS {
                let (mut flags, mut sample) = (0u32, None);
                self.reader
                    .ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample))
                    .ok()?;
                if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                    return None;
                }
                if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0
                    && float_format(&self.reader) != Some((self.sample_rate, self.channels))
                {
                    return None;
                }
                let Some(sample) = sample else {
                    if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                        return None;
                    }
                    continue;
                };
                let buffer = sample.ConvertToContiguousBuffer().ok()?;
                let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
                buffer.Lock(&mut ptr, None, Some(&mut len)).ok()?;
                self.out.clear();
                if !ptr.is_null() {
                    let bytes = std::slice::from_raw_parts(ptr, len as usize);
                    let frame_bytes = 4 * self.channels as usize;
                    let whole = bytes.len() / frame_bytes * frame_bytes;
                    // The buffer isn't guaranteed to be aligned for f32.
                    self.out.extend(
                        bytes[..whole]
                            .chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                    );
                }
                let _ = buffer.Unlock();
                if !self.out.is_empty() {
                    return Some(&self.out);
                }
            }
            None
        }
    }
}

/// Rate and channel count of the current output type, if it is 32-bit float.
unsafe fn float_format(reader: &IMFSourceReader) -> Option<(u32, u16)> {
    let current = reader.GetCurrentMediaType(STREAM).ok()?;
    if current.GetGUID(&MF_MT_SUBTYPE).ok()? != MFAudioFormat_Float
        || current.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).ok()? != 32
    {
        return None;
    }
    let rate = current
        .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
        .ok()
        .filter(|&r| r > 0)?;
    let channels = current
        .GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
        .ok()
        .filter(|&c| c > 0)?;
    Some((rate, u16::try_from(channels).ok()?))
}

/// Properties of the first audio stream as the container declares them. No decoding.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioStreamMeta {
    /// "WMA", "AC-3", "WMA Lossless"... or the WAVE format tag.
    pub codec: Option<String>,
    pub lossless: Option<bool>,
    pub duration_sec: f64,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    /// Only for lossless codecs: for lossy ones MF reports the decoder output, not the source.
    pub bit_depth: Option<u32>,
}

pub fn probe_audio_meta(path: &Path) -> Option<AudioStreamMeta> {
    let _com = ComScope::new();
    if !ensure_mf_started() {
        return None;
    }
    unsafe {
        let reader = open_reader_for(path, STREAM, false).ok()?;
        let native = reader.GetNativeMediaType(STREAM, 0).ok()?;
        let positive = |v: windows::core::Result<u32>| v.ok().filter(|&v| v > 0);
        let codec = native
            .GetGUID(&MF_MT_SUBTYPE)
            .ok()
            .and_then(|g| audio_codec_name(&g));
        let lossless = codec.as_deref().and_then(is_lossless);
        let duration_sec = duration_hns(&reader) as f64 / 10_000_000.0;
        let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let bitrate_kbps = positive(native.GetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND))
            .map(|b| (b as f64 * 8.0 / 1000.0).round() as u32)
            .or_else(|| {
                (duration_sec >= 1.0 && file_size > 0)
                    .then(|| (file_size as f64 * 8.0 / 1000.0 / duration_sec).round() as u32)
            })
            .filter(|&k| k > 0);
        Some(AudioStreamMeta {
            codec,
            lossless,
            duration_sec,
            bitrate_kbps,
            sample_rate: positive(native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)),
            channels: positive(native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)),
            bit_depth: if lossless == Some(true) {
                positive(native.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE))
            } else {
                None
            },
        })
    }
}

/// Whether a codec (as named by `audio_codec_name`) is lossless; `None` for unknown tags.
fn is_lossless(codec: &str) -> Option<bool> {
    match codec {
        "PCM" | "PCM float" | "WMA Lossless" | "FLAC" | "ALAC" => Some(true),
        _ if codec.starts_with("0x") => None,
        _ => Some(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_by_codec_name() {
        assert_eq!(is_lossless("WMA Lossless"), Some(true));
        assert_eq!(is_lossless("WMA"), Some(false));
        assert_eq!(is_lossless("AC-3"), Some(false));
        assert_eq!(is_lossless("0x1234"), None);
    }

    /// A plain WAV is readable by MF too: the decoded ramp must come out intact.
    #[test]
    fn decodes_pcm_wav() {
        let rate = 8000u32;
        let frames = rate * 2;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + frames * 2).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(frames * 2).to_le_bytes());
        for i in 0..frames {
            b.extend_from_slice(&((i % 1000) as i16 * 16).to_le_bytes());
        }
        let path = std::env::temp_dir().join(format!("mediares_mf_{}.wav", std::process::id()));
        std::fs::write(&path, b).unwrap();

        let mut reader = MfAudioReader::open(&path).expect("open");
        assert_eq!((reader.sample_rate, reader.channels), (rate, 1));
        let mut samples = Vec::new();
        while let Some(chunk) = reader.next_chunk() {
            samples.extend_from_slice(chunk);
        }
        assert_eq!(samples.len(), frames as usize);
        assert!(
            (samples[500] * 32768.0 - 8000.0).abs() < 1.0,
            "{}",
            samples[500] * 32768.0
        );

        let meta = probe_audio_meta(&path).expect("meta");
        assert_eq!(meta.codec.as_deref(), Some("PCM"));
        assert_eq!(
            (meta.sample_rate, meta.channels, meta.bit_depth),
            (Some(rate), Some(1), Some(16))
        );
        assert_eq!(meta.bitrate_kbps, Some(128));
        assert!((meta.duration_sec - 2.0).abs() < 0.01);
        let _ = std::fs::remove_file(path);
    }
}
