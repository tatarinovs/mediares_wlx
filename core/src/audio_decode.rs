//! Audio decoding to interleaved `f32` PCM via `symphonia` — pure Rust, independent of the codecs
//! installed in the system. Used by the player in `combo` (and later for waveforms).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use symphonia::core::codecs::audio::{AudioDecoder as CodecDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
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
        let mut file = File::open(path).ok()?;
        let mut hint = Hint::new();
        // MP3 wrapped in a WAV header (format tag 0x55): symphonia's WAV reader only takes PCM,
        // so the MPEG stream inside the `data` chunk is handed over directly.
        let source: Box<dyn MediaSource> = match riff_mp3_data(&mut file) {
            Some((start, len)) => {
                hint.with_extension("mp3");
                Box::new(SubFile::new(file, start, len).ok()?)
            }
            None => {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    hint.with_extension(ext);
                }
                Box::new(file)
            }
        };
        let mss = MediaSourceStream::new(source, Default::default());
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .ok()?;

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
        let duration = match (time_base, track.duration) {
            (Some(tb), Some(d)) => tb.calc_duration(d).map(|t| t.as_secs_f64()),
            _ => track.num_frames.map(|n| n as f64 / sample_rate as f64),
        };
        let track_id = track.id;

        Some(Self {
            format,
            decoder,
            track_id,
            time_base,
            sample_rate,
            channels,
            duration: duration.filter(|d| d.is_finite() && *d > 0.0),
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

/// Offset and length of the `data` chunk of a RIFF/WAVE file whose format is MPEG Layer III.
fn riff_mp3_data(file: &mut File) -> Option<(u64, u64)> {
    let mut header = [0u8; 12];
    file.read_exact(&mut header).ok()?;
    let _ = file.seek(SeekFrom::Start(0));
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return None;
    }
    let file_len = file.metadata().ok()?.len();
    let mut pos = 12u64;
    let mut is_mp3 = false;
    let result = loop {
        let mut chunk = [0u8; 8];
        file.seek(SeekFrom::Start(pos)).ok()?;
        if file.read_exact(&mut chunk).is_err() {
            break None;
        }
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        match &chunk[0..4] {
            b"fmt " => {
                let mut tag = [0u8; 2];
                file.read_exact(&mut tag).ok()?;
                is_mp3 = u16::from_le_bytes(tag) == 0x55;
                if !is_mp3 {
                    break None;
                }
            }
            // Some writers leave the size at 0 or too large: the data runs to the end of the file.
            b"data" if is_mp3 => {
                let available = file_len.saturating_sub(pos + 8);
                break Some((
                    pos + 8,
                    if size == 0 || size > available {
                        available
                    } else {
                        size
                    },
                ));
            }
            _ => {}
        }
        pos += 8 + size + (size & 1);
        if pos >= file_len {
            break None;
        }
    };
    let _ = file.seek(SeekFrom::Start(0));
    result
}

/// A byte range of a file as a seekable media source.
struct SubFile {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}

impl SubFile {
    fn new(mut file: File, start: u64, len: u64) -> io::Result<Self> {
        file.seek(SeekFrom::Start(start))?;
        Ok(Self {
            file,
            start,
            len,
            pos: 0,
        })
    }
}

impl Read for SubFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.len.saturating_sub(self.pos) as usize;
        let take = buf.len().min(left);
        let n = self.file.read(&mut buf[..take])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SubFile {
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
        self.file.seek(SeekFrom::Start(self.start + self.pos))?;
        Ok(self.pos)
    }
}

impl MediaSource for SubFile {
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

    /// 16-bit PCM WAV with a ramp so positions can be recognised by value.
    fn write_wav(path: &Path, rate: u32, channels: u16, seconds: u32) {
        let frames = rate * seconds;
        let data_len = frames * channels as u32 * 2;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        b.extend_from_slice(&(channels * 2).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            // One step per 1/100 s: value encodes the position.
            let v = ((i * 100 / rate) as i16).to_le_bytes();
            for _ in 0..channels {
                b.extend_from_slice(&v);
            }
        }
        std::fs::write(path, b).unwrap();
    }

    fn temp_wav(name: &str, rate: u32, channels: u16, seconds: u32) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("mediares_{}_{}.wav", name, std::process::id()));
        write_wav(&path, rate, channels, seconds);
        path
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
