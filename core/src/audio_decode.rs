//! Audio decoding to interleaved `f32` PCM via `symphonia` — pure Rust, independent of the codecs
//! installed in the system. Used by the player in `combo` (and later for waveforms).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use symphonia::core::codecs::audio::{AudioDecoder as CodecDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase, Timestamp};

/// Consecutive undecodable packets after which the stream is treated as finished.
const MAX_BAD_PACKETS: u32 = 64;

/// Sequential decoder of the default audio track of a file.
pub struct AudioDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn CodecDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    pub sample_rate: u32,
    /// Output channel count; every chunk is converted to it.
    pub channels: u16,
    /// Total length in seconds, if the container declares it.
    pub duration: Option<f64>,
    /// After an accurate seek: frames before this timestamp are dropped.
    skip_until: Option<Timestamp>,
    decoded: Vec<f32>,
    out: Vec<f32>,
}

/// A decoded run of interleaved samples and the time of its first frame.
pub struct Chunk<'a> {
    pub start: f64,
    pub samples: &'a [f32],
}

impl AudioDecoder {
    /// Opens the file; `None` if the container or codec is not supported.
    pub fn open(path: &Path) -> Option<Self> {
        let format = open_format(path)?;
        let track = format.default_track(TrackType::Audio)?;
        let params = track.codec_params.as_ref()?.audio()?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .ok()?;
        let sample_rate = params.sample_rate?;
        let channels = params
            .channels
            .as_ref()
            .map_or(2, |c| c.count())
            .clamp(1, 8) as u16;
        let time_base = track.time_base;
        let duration = track_duration(track, sample_rate);
        let track_id = track.id;

        Some(Self {
            format,
            decoder,
            track_id,
            time_base,
            sample_rate,
            channels,
            duration,
            skip_until: None,
            decoded: Vec::new(),
            out: Vec::new(),
        })
    }

    fn seconds(&self, ts: Timestamp) -> f64 {
        match self.time_base {
            Some(tb) => tb.calc_time(ts).map_or(0.0, |t| t.as_secs_f64()),
            None => ts.get() as f64 / self.sample_rate as f64,
        }
    }

    /// Decodes the next run of samples; `None` at the end of the stream (or on a fatal error).
    pub fn next_chunk(&mut self) -> Option<Chunk<'_>> {
        let mut bad_packets = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return None,
                Err(Error::IoError(_)) | Err(Error::DecodeError(_))
                    if bad_packets < MAX_BAD_PACKETS =>
                {
                    bad_packets += 1;
                    continue;
                }
                Err(_) => return None,
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let buf = match self.decoder.decode(&packet) {
                Ok(buf) => buf,
                Err(Error::IoError(_)) | Err(Error::DecodeError(_))
                    if bad_packets < MAX_BAD_PACKETS =>
                {
                    bad_packets += 1;
                    continue;
                }
                Err(_) => return None,
            };
            let frames = buf.frames();
            if frames == 0 {
                continue;
            }
            let src_channels = buf.spec().channels().count().max(1);
            buf.copy_to_vec_interleaved(&mut self.decoded);

            // Accurate seek: drop the frames before the requested position.
            let mut skip = 0usize;
            if let Some(until) = self.skip_until {
                let delta = until.get().saturating_sub(packet.pts.get()).max(0) as f64;
                let per_frame = match self.time_base {
                    Some(tb) => {
                        tb.calc_time(Timestamp::new(1))
                            .map_or(0.0, |t| t.as_secs_f64())
                            * self.sample_rate as f64
                    }
                    None => 1.0,
                };
                skip = ((delta * per_frame).round() as usize).min(frames);
                if skip == frames {
                    continue;
                }
                self.skip_until = None;
            }

            let start = self.seconds(packet.pts) + skip as f64 / self.sample_rate as f64;
            remap_channels(
                &self.decoded[skip * src_channels..],
                src_channels,
                self.channels as usize,
                &mut self.out,
            );
            return Some(Chunk {
                start,
                samples: &self.out,
            });
        }
    }

    /// Seeks so that the next chunk starts at `seconds` (clamped to the stream). Returns the
    /// position actually reached.
    pub fn seek(&mut self, seconds: f64) -> Option<f64> {
        let seconds = match self.duration {
            Some(d) => seconds.clamp(0.0, d),
            None => seconds.max(0.0),
        };
        let time = Time::try_from_secs_f64(seconds)?;
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .ok()?;
        self.decoder.reset();
        self.skip_until = Some(seeked.required_ts);
        Some(self.seconds(seeked.required_ts))
    }
}

/// Probes the container of `path` (tags and pictures included). MP3 wrapped in a WAV header
/// (format tag 0x55) is handed over as a plain MPEG stream — symphonia's WAV reader only takes
/// PCM — keeping any tags in front of and behind the RIFF structure.
pub(crate) fn open_format(path: &Path) -> Option<Box<dyn FormatReader>> {
    let mut file = File::open(path).ok()?;
    let mut hint = Hint::new();
    let source: Box<dyn MediaSource> = match riff_mp3_ranges(&mut file) {
        Some(ranges) => {
            hint.with_extension("mp3");
            Box::new(Spliced::new(file, ranges))
        }
        None => {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                hint.with_extension(ext);
            }
            Box::new(file)
        }
    };
    let mss = MediaSourceStream::new(source, Default::default());
    symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .ok()
}

/// Length of `track` in seconds, if the container declares it.
pub(crate) fn track_duration(track: &Track, sample_rate: u32) -> Option<f64> {
    let seconds = match (track.time_base, track.duration) {
        (Some(tb), Some(d)) => tb.calc_duration(d).map(|t| t.as_secs_f64()),
        _ => track.num_frames.map(|n| n as f64 / sample_rate as f64),
    };
    seconds.filter(|d| d.is_finite() && *d > 0.0)
}

/// For a RIFF/WAVE file (possibly behind an ID3v2 tag) whose format is MPEG audio: the byte
/// ranges `(offset, length)` of everything except the RIFF structure around the `data` payload.
fn riff_mp3_ranges(file: &mut File) -> Option<Vec<(u64, u64)>> {
    let file_len = file.metadata().ok()?.len();
    let mut read_at = |pos: u64, buf: &mut [u8]| {
        file.seek(SeekFrom::Start(pos)).is_ok() && file.read_exact(buf).is_ok()
    };
    let mut head = [0u8; 12];
    if !read_at(0, &mut head[..10]) {
        return None;
    }
    let riff = if &head[..3] == b"ID3" {
        let size = head[6..10]
            .iter()
            .fold(0u64, |v, &b| v << 7 | (b & 0x7F) as u64);
        let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
        10 + size + footer
    } else {
        0
    };
    if !read_at(riff, &mut head) || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return None;
    }
    let riff_end = riff + 8 + u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as u64;
    let mut pos = riff + 12;
    let mut is_mpeg = false;
    let data = loop {
        let mut chunk = [0u8; 10];
        if pos + 8 > file_len || !read_at(pos, &mut chunk[..8]) {
            return None;
        }
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        match &chunk[0..4] {
            b"fmt " => {
                is_mpeg = read_at(pos + 8, &mut chunk[8..10])
                    && u16::from_le_bytes([chunk[8], chunk[9]]) == 0x55;
                if !is_mpeg {
                    return None;
                }
            }
            // Some writers leave the size at 0 or too large: the data runs to the end of the file.
            b"data" if is_mpeg => {
                let available = file_len.saturating_sub(pos + 8);
                let len = if size == 0 || size > available {
                    available
                } else {
                    size
                };
                break (pos + 8, len);
            }
            _ => {}
        }
        pos += 8 + size + (size & 1);
    };
    // Chunks after the data (LIST...) are skipped; a RIFF size past the end of the file is bogus,
    // and whatever follows the data is kept then (an ID3v1 tag, typically).
    let data_end = data.0 + data.1;
    let tail = if riff_end <= file_len {
        riff_end.max(data_end)
    } else {
        data_end
    };
    Some(
        [(0, riff), data, (tail, file_len - tail)]
            .into_iter()
            .filter(|&(_, len)| len > 0)
            .collect(),
    )
}

/// Byte ranges of a file joined into one seekable media source.
struct Spliced {
    file: File,
    /// `(offset in the file, length)`.
    ranges: Vec<(u64, u64)>,
    len: u64,
    pos: u64,
}

impl Spliced {
    fn new(file: File, ranges: Vec<(u64, u64)>) -> Self {
        let len = ranges.iter().map(|&(_, len)| len).sum();
        Self {
            file,
            ranges,
            len,
            pos: 0,
        }
    }
}

impl Read for Spliced {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut start = 0;
        for &(offset, len) in &self.ranges {
            if self.pos < start + len {
                let within = self.pos - start;
                let take = buf.len().min((len - within) as usize);
                self.file.seek(SeekFrom::Start(offset + within))?;
                let n = self.file.read(&mut buf[..take])?;
                self.pos += n as u64;
                return Ok(n);
            }
            start += len;
        }
        Ok(0)
    }
}

impl Seek for Spliced {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.len as i64 + d,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = (target as u64).min(self.len);
        Ok(self.pos)
    }
}

impl MediaSource for Spliced {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

/// Converts interleaved samples between channel counts: extra source channels are dropped,
/// missing ones repeat the last source channel (mono → stereo duplicates).
fn remap_channels(src: &[f32], from: usize, to: usize, out: &mut Vec<f32>) {
    out.clear();
    if from == to {
        out.extend_from_slice(src);
        return;
    }
    out.reserve(src.len() / from * to);
    for frame in src.chunks_exact(from) {
        out.extend((0..to).map(|c| frame[c.min(from - 1)]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16-bit PCM with a ramp so positions can be recognised by value (one step per 1/100 s).
    fn temp_wav(name: &str, rate: u32, channels: u16, seconds: u32) -> std::path::PathBuf {
        crate::test_util::pcm16_wav(name, rate, channels, rate * seconds, |i| {
            (i * 100 / rate) as i16
        })
    }

    #[test]
    fn decodes_wav_and_reports_format() {
        let path = temp_wav("fmt", 8000, 1, 2);
        let mut dec = AudioDecoder::open(&path).expect("open");
        assert_eq!((dec.sample_rate, dec.channels), (8000, 1));
        assert!((dec.duration.unwrap() - 2.0).abs() < 0.01);
        let mut total = 0;
        while let Some(chunk) = dec.next_chunk() {
            total += chunk.samples.len();
        }
        assert_eq!(total, 16000);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn seek_is_sample_accurate() {
        let path = temp_wav("seek", 8000, 2, 3);
        let mut dec = AudioDecoder::open(&path).expect("open");
        let reached = dec.seek(1.5).expect("seek");
        assert!((reached - 1.5).abs() < 0.001, "{}", reached);
        let chunk = dec.next_chunk().expect("chunk");
        assert!((chunk.start - 1.5).abs() < 0.001, "{}", chunk.start);
        // Sample value at 1.5 s is step 150.
        assert!(
            (chunk.samples[0] * 32768.0 - 150.0).abs() < 1.0,
            "{}",
            chunk.samples[0] * 32768.0
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn channel_remap() {
        let mut out = Vec::new();
        remap_channels(&[1.0, 2.0], 1, 2, &mut out);
        assert_eq!(out, [1.0, 1.0, 2.0, 2.0]);
        remap_channels(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 3, 2, &mut out);
        assert_eq!(out, [1.0, 2.0, 4.0, 5.0]);
    }
}
