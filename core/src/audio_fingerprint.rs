//! Audio duplicate detection fields:
//!
//! - **PCM hash** and **Fingerprint** are computed in one sequential decoding pass:
//!   - **PCM hash** — hash of the decoded samples between the leading and trailing silence: equal
//!     for the same audio in any lossless container or with different tags (WAV → FLAC, retagged MP3).
//!   - **Fingerprint** — `<audible_sec>s_<h25>_<h50>_<h75>`: 24-bit coarse spectral hashes of three
//!     windows at 25/50/75 % of the non-silent part (proportional windows on tracks `< 60` s, and
//!     `<audible_sec>s_pcm_<pcm_hash>` fallback when the audible part is `< 3` s).
//! - **Fast fingerprint** — seek-based variant for long tracks (`> 75` s) that probes head/tail
//!   silence boundaries and decodes only three 12 s windows instead of the full file.
//!
//! The signal is downmixed to mono and resampled to [`ANALYSIS_RATE`]; per frame only a few band
//! energies are kept, so hours of audio are fine.

use std::f32::consts::PI;
use std::path::Path;
use std::sync::OnceLock;

use crate::audio_decode::AudioDecoder;
use crate::cache::{AudioAnalysis, AudioDuration, AudioFastFingerprint};
use crate::mf_audio::MfAudioReader;

const ANALYSIS_RATE: u32 = 11_025;
const FFT_SIZE: usize = 1024;
/// ~23 ms: window positions must not jump between copies.
const HOP: usize = 256;
/// Log-spaced sub-bands between these frequencies, averaged in groups into [`BANDS`] bands.
const LOW_HZ: f32 = 150.0;
const HIGH_HZ: f32 = 5000.0;
const SUB_BANDS: usize = 25;
const BANDS: usize = 5;
const _: () = assert!(SUB_BANDS.is_multiple_of(BANDS));
const SLICES: usize = 3;
const WINDOW_SEC: f32 = 30.0;
const FAST_WINDOW_SEC: f32 = 12.0;
const EDGE_PROBE_SEC: f64 = 15.0;
const SEEK_THRESHOLD_SEC: f64 = 75.0;
const WINDOW_AT: [f32; 3] = [0.25, 0.5, 0.75];
/// Start / end of the audible part: frames louder than this relative to the loudest (dB).
const AUDIBLE_DB: f32 = -30.0;
/// Shortest audible part that gets a spectral fingerprint (shorter audible files fall back to PCM hash).
const MIN_AUDIBLE_SEC: f32 = 3.0;
/// Samples quieter than this (below one 16-bit step) are silence for the PCM hash.
const PCM_SILENCE: f32 = 1.5 / 32768.0;
/// Minimum peak frame energy required to consider a track non-silent.
const SILENCE_FLOOR_SQ: f32 = (PCM_SILENCE * PCM_SILENCE) * FFT_SIZE as f32;
/// Checks for cancellation this often (decoded chunks).
const CANCEL_CHECK_EVERY: u32 = 64;

#[derive(Debug)]
pub enum AudioError {
    /// The host asked to stop; the result must not be cached.
    Cancelled,
    Unsupported,
    /// Decoding stopped early or encountered damaged packets. Never fingerprint a prefix.
    Invalid,
}

/// Interleaved PCM from either decoder.
trait PcmSource {
    fn format(&self) -> (u32, usize);
    fn duration(&self) -> Option<f64>;
    fn next_timed_samples(&mut self) -> std::io::Result<Option<(Option<f64>, &[f32])>>;
    fn seek(&mut self, seconds: f64) -> Option<f64>;
}

impl PcmSource for AudioDecoder {
    fn format(&self) -> (u32, usize) {
        (self.sample_rate, self.channels as usize)
    }

    fn duration(&self) -> Option<f64> {
        self.duration
    }

    fn next_timed_samples(&mut self) -> std::io::Result<Option<(Option<f64>, &[f32])>> {
        self.next_chunk_checked()
            .map(|c| c.map(|c| (Some(c.start), c.samples)))
    }

    fn seek(&mut self, seconds: f64) -> Option<f64> {
        AudioDecoder::seek(self, seconds)
    }
}

impl PcmSource for MfAudioReader {
    fn format(&self) -> (u32, usize) {
        (self.sample_rate, self.channels as usize)
    }

    fn duration(&self) -> Option<f64> {
        self.duration
    }

    fn next_timed_samples(&mut self) -> std::io::Result<Option<(Option<f64>, &[f32])>> {
        self.next_chunk_timed()
    }

    fn seek(&mut self, seconds: f64) -> Option<f64> {
        MfAudioReader::seek(self, seconds)
    }
}

pub fn analyze_audio(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<AudioAnalysis, AudioError> {
    analyze_sequential::<(BandAnalyzer, PcmHash)>(path, cancelled)
}

#[derive(Debug, Default, Clone)]
pub struct AudioProfile {
    pub backend: &'static str,
    pub open_us: u64,
    pub decode_us: u64,
    pub math_us: u64,
    pub sample_rate: u32,
    pub channels: usize,
    pub chunks: u32,
    pub frames: u64,
}

#[derive(Debug, Default, Clone)]
pub struct FastProfile {
    pub backend: &'static str,
    pub mode: &'static str,
}

pub fn analyze_audio_profiled(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> (Result<AudioAnalysis, AudioError>, AudioProfile) {
    let mut prof = AudioProfile {
        backend: "none",
        ..Default::default()
    };
    let res = analyze_sequential_with::<(BandAnalyzer, PcmHash), AudioProfile>(
        path, cancelled, &mut prof,
    );
    (res, prof)
}

pub fn fast_fingerprint_audio_profiled(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> (Result<AudioFastFingerprint, AudioError>, FastProfile) {
    let mut prof = FastProfile {
        backend: "none",
        mode: "error",
    };
    let res = fast_fingerprint_audio_with(path, cancelled, Some(&mut prof));
    (res, prof)
}

pub fn fast_fingerprint_audio(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<AudioFastFingerprint, AudioError> {
    fast_fingerprint_audio_with(path, cancelled, None)
}

fn fast_fingerprint_audio_with(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
    mut prof: Option<&mut FastProfile>,
) -> Result<AudioFastFingerprint, AudioError> {
    if cancelled() {
        return Err(AudioError::Cancelled);
    }
    if let Some(mut decoder) = AudioDecoder::open(path) {
        if let Some(expected) = decoder
            .duration
            .filter(|d| d.is_finite() && *d > SEEK_THRESHOLD_SEC)
        {
            match analyze_source_seek(&mut decoder, expected, cancelled) {
                Ok(SeekProbe::Done(fingerprint)) => {
                    if let Some(p) = prof.as_deref_mut() {
                        p.backend = "symphonia";
                        p.mode = "seek";
                    }
                    return Ok(AudioFastFingerprint {
                        duration_sec: expected.round() as u32,
                        fingerprint,
                    });
                }
                Ok(SeekProbe::FallbackSequential) => {
                    if let Some(p) = prof.as_deref_mut() {
                        p.backend = "symphonia";
                        p.mode = "fallback_seq";
                    }
                    return analyze_sequential::<FastSequential>(path, cancelled);
                }
                Err(AudioError::Unsupported) => {}
                Err(e) => return Err(e),
            }
        } else {
            if let Some(p) = prof.as_deref_mut() {
                p.backend = "symphonia";
                p.mode = "short";
            }
            drop(decoder);
            return analyze_sequential::<FastSequential>(path, cancelled);
        }
    }
    let mut reader = MfAudioReader::open(path).ok_or(AudioError::Unsupported)?;
    if let Some(expected) = reader
        .duration
        .filter(|d| d.is_finite() && *d > SEEK_THRESHOLD_SEC)
    {
        match analyze_source_seek(&mut reader, expected, cancelled) {
            Ok(SeekProbe::Done(fingerprint)) => {
                if let Some(p) = prof.as_deref_mut() {
                    p.backend = "mf";
                    p.mode = "seek";
                }
                return Ok(AudioFastFingerprint {
                    duration_sec: expected.round() as u32,
                    fingerprint,
                });
            }
            Ok(SeekProbe::FallbackSequential) => {
                if let Some(p) = prof.as_deref_mut() {
                    p.backend = "mf";
                    p.mode = "fallback_seq";
                }
                drop(reader);
                let mut reader = MfAudioReader::open(path).ok_or(AudioError::Unsupported)?;
                return analyze_source::<FastSequential>(&mut reader, cancelled);
            }
            Err(e) => return Err(e),
        }
    }
    if let Some(p) = prof {
        p.backend = "mf";
        p.mode = "short";
    }
    analyze_source::<FastSequential>(&mut reader, cancelled)
}

/// Counts decoded frames without hashing samples or analyzing their spectrum.
pub fn decoded_duration(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<AudioDuration, AudioError> {
    analyze_sequential::<()>(path, cancelled)
}

// Estimate the mono working set before allocating. A single very long recording
// may exceed the budget on its own; let it finish without competing scans.
const SCAN_BUDGET: usize = 384 * 1024 * 1024;
static SCAN_BYTES: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
static SCAN_READY: std::sync::Condvar = std::sync::Condvar::new();

struct ScanReservation(usize);

impl ScanReservation {
    fn acquire(duration: Option<f64>, cancelled: &dyn Fn() -> bool) -> Result<Self, AudioError> {
        let bytes = duration
            .filter(|d| d.is_finite() && *d > 0.0)
            .map_or(SCAN_BUDGET, |d| {
                // Include mono samples, per-hop energy, and the sequential input buffer.
                ((d * f64::from(ANALYSIS_RATE) * 4.02) as usize)
                    .saturating_add(crate::audio_decode::SCAN_READ_BUF)
                    .min(SCAN_BUDGET)
            });
        let mut used = SCAN_BYTES.lock().unwrap_or_else(|e| e.into_inner());
        while *used + bytes > SCAN_BUDGET {
            if cancelled() {
                return Err(AudioError::Cancelled);
            }
            used = SCAN_READY
                .wait_timeout(used, std::time::Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *used += bytes;
        Ok(Self(bytes))
    }
}

impl Drop for ScanReservation {
    fn drop(&mut self) {
        *SCAN_BYTES.lock().unwrap_or_else(|e| e.into_inner()) -= self.0;
        SCAN_READY.notify_all();
    }
}

trait SampleAnalysis {
    const STORES_MONO: bool = true;
    type Output;
    fn start(rate: u32, channels: usize, duration_sec: Option<f64>) -> Self;
    fn samples(&mut self, samples: &[f32], channels: usize);
    fn result(self, duration: f64) -> Self::Output;
}

impl SampleAnalysis for () {
    const STORES_MONO: bool = false;
    type Output = AudioDuration;

    fn start(_: u32, _: usize, _: Option<f64>) -> Self {}
    fn samples(&mut self, _: &[f32], _: usize) {}
    fn result(self, duration: f64) -> Self::Output {
        AudioDuration(duration.round() as u32)
    }
}

impl SampleAnalysis for (BandAnalyzer, PcmHash) {
    type Output = AudioAnalysis;

    fn start(rate: u32, channels: usize, duration_sec: Option<f64>) -> Self {
        (
            BandAnalyzer::with_duration(rate, duration_sec),
            PcmHash::new(rate, channels),
        )
    }

    fn samples(&mut self, samples: &[f32], channels: usize) {
        self.0.push_samples(samples, channels);
        self.1.update(samples);
    }

    fn result(self, duration: f64) -> Self::Output {
        let pcm_hash = self.1.finish();
        let fingerprint = fingerprint_with_window(
            &self.0,
            WINDOW_SEC,
            self.1.started.then_some(pcm_hash.as_str()),
        );
        AudioAnalysis {
            duration_sec: duration.round() as u32,
            pcm_hash,
            fingerprint,
        }
    }
}

struct FastSequential(BandAnalyzer, PcmHash);

impl SampleAnalysis for FastSequential {
    type Output = AudioFastFingerprint;

    fn start(rate: u32, channels: usize, duration_sec: Option<f64>) -> Self {
        Self(
            BandAnalyzer::with_duration(rate, duration_sec),
            PcmHash::new(rate, channels),
        )
    }

    fn samples(&mut self, samples: &[f32], channels: usize) {
        self.0.push_samples(samples, channels);
        self.1.update(samples);
    }

    fn result(self, duration: f64) -> Self::Output {
        let pcm_hash = self.1.finish();
        let fingerprint = fingerprint_with_window(
            &self.0,
            FAST_WINDOW_SEC,
            self.1.started.then_some(pcm_hash.as_str()),
        );
        AudioFastFingerprint {
            duration_sec: duration.round() as u32,
            fingerprint,
        }
    }
}

trait Profiler {
    type Mark;
    fn mark(&self) -> Self::Mark;
    fn record_open(&mut self, start: Self::Mark, backend: &'static str);
    fn record_decode(&mut self, start: Self::Mark);
    fn record_math(&mut self, start: Self::Mark);
    fn record_format(&mut self, rate: u32, channels: usize, chunks: u32, frames: u64);
}

impl Profiler for () {
    type Mark = ();
    #[inline]
    fn mark(&self) {}
    #[inline]
    fn record_open(&mut self, _: (), _: &'static str) {}
    #[inline]
    fn record_decode(&mut self, _: ()) {}
    #[inline]
    fn record_math(&mut self, _: ()) {}
    #[inline]
    fn record_format(&mut self, _: u32, _: usize, _: u32, _: u64) {}
}

impl Profiler for AudioProfile {
    type Mark = std::time::Instant;
    #[inline]
    fn mark(&self) -> Self::Mark {
        std::time::Instant::now()
    }
    #[inline]
    fn record_open(&mut self, start: Self::Mark, backend: &'static str) {
        self.open_us += start.elapsed().as_micros() as u64;
        self.backend = backend;
    }
    #[inline]
    fn record_decode(&mut self, start: Self::Mark) {
        self.decode_us += start.elapsed().as_micros() as u64;
    }
    #[inline]
    fn record_math(&mut self, start: Self::Mark) {
        self.math_us += start.elapsed().as_micros() as u64;
    }
    #[inline]
    fn record_format(&mut self, rate: u32, channels: usize, chunks: u32, frames: u64) {
        self.sample_rate = rate;
        self.channels = channels;
        self.chunks = chunks;
        self.frames = frames;
    }
}

/// Decodes with symphonia; files it can't open or decode go through Media Foundation, whose
/// results depend on the system's decoders (still stable on one machine, which is what TC's
/// duplicate search compares).
fn analyze_sequential<A: SampleAnalysis>(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<A::Output, AudioError> {
    let mut noop = ();
    analyze_sequential_with::<A, ()>(path, cancelled, &mut noop)
}

fn analyze_sequential_with<A: SampleAnalysis, P: Profiler>(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
    prof: &mut P,
) -> Result<A::Output, AudioError> {
    if cancelled() {
        return Err(AudioError::Cancelled);
    }
    let m = prof.mark();
    if let Some(mut decoder) = AudioDecoder::open_sequential(path) {
        prof.record_open(m, "symphonia");
        match analyze_source_with::<A, P>(&mut decoder, cancelled, prof) {
            Err(AudioError::Unsupported) => {}
            result => return result,
        }
    }
    let m = prof.mark();
    let mut reader = MfAudioReader::open(path).ok_or(AudioError::Unsupported)?;
    prof.record_open(m, "mf");
    analyze_source_with::<A, P>(&mut reader, cancelled, prof)
}

fn analyze_source<A: SampleAnalysis>(
    source: &mut dyn PcmSource,
    cancelled: &dyn Fn() -> bool,
) -> Result<A::Output, AudioError> {
    let mut noop = ();
    analyze_source_with::<A, ()>(source, cancelled, &mut noop)
}

fn analyze_source_with<A: SampleAnalysis, P: Profiler>(
    source: &mut dyn PcmSource,
    cancelled: &dyn Fn() -> bool,
    prof: &mut P,
) -> Result<A::Output, AudioError> {
    let (rate, channels) = source.format();
    if rate == 0 || channels == 0 {
        return Err(AudioError::Invalid);
    }
    let expected = source.duration();
    let _reservation = A::STORES_MONO
        .then(|| ScanReservation::acquire(expected, cancelled))
        .transpose()?;
    let mut analysis = A::start(rate, channels, expected);
    let mut frames_total: u64 = 0;
    let mut chunks = 0u32;

    loop {
        let m_dec = prof.mark();
        let next = source.next_timed_samples().map_err(|_| {
            if frames_total == 0 {
                AudioError::Unsupported
            } else {
                AudioError::Invalid
            }
        })?;
        prof.record_decode(m_dec);
        let Some((_, samples)) = next else { break };
        chunks += 1;
        if chunks.is_multiple_of(CANCEL_CHECK_EVERY) && cancelled() {
            return Err(AudioError::Cancelled);
        }
        let m_math = prof.mark();
        analysis.samples(samples, channels);
        prof.record_math(m_math);
        frames_total += (samples.len() / channels) as u64;
    }
    if frames_total == 0 {
        return Err(AudioError::Unsupported);
    }

    let duration = frames_total as f64 / rate as f64;
    // Allow codec padding/rounding, but not a file ending seconds before its declared end.
    if expected.is_some_and(|expected| expected.is_finite() && expected > duration + 0.25) {
        return Err(AudioError::Invalid);
    }
    if cancelled() {
        return Err(AudioError::Cancelled);
    }
    let m_math = prof.mark();
    let result = analysis.result(duration);
    prof.record_math(m_math);
    prof.record_format(rate, channels, chunks, frames_total);
    Ok(result)
}

enum SeekProbe {
    Done(Option<String>),
    FallbackSequential,
}

fn read_window(
    source: &mut dyn PcmSource,
    target_frames: Option<u64>,
    max_frames: u64,
    fallback_start: f64,
    analyzer: &mut BandAnalyzer,
    cancelled: &dyn Fn() -> bool,
    unsupported_if_empty: bool,
) -> Result<(u64, f64), AudioError> {
    let (_, channels) = source.format();
    let mut frames_read = 0u64;
    let mut chunks = 0u32;
    let mut start_sec = None;

    while let Some((chunk_start, samples)) = source.next_timed_samples().map_err(|_| {
        if unsupported_if_empty && frames_read == 0 {
            AudioError::Unsupported
        } else {
            AudioError::Invalid
        }
    })? {
        if start_sec.is_none() {
            start_sec = Some(chunk_start.unwrap_or(fallback_start));
        }
        chunks += 1;
        if chunks.is_multiple_of(CANCEL_CHECK_EVERY) && cancelled() {
            return Err(AudioError::Cancelled);
        }
        let chunk_frames = (samples.len() / channels) as u64;
        let take_frames = match target_frames {
            Some(target) => chunk_frames.min(target.saturating_sub(frames_read)),
            None => chunk_frames,
        };
        if take_frames > 0 {
            analyzer.push_samples(&samples[..take_frames as usize * channels], channels);
            frames_read += take_frames;
        }
        if target_frames.is_some_and(|t| frames_read >= t) || frames_read >= max_frames {
            break;
        }
    }
    if cancelled() {
        return Err(AudioError::Cancelled);
    }
    Ok((frames_read, start_sec.unwrap_or(fallback_start)))
}

fn analyze_source_seek(
    source: &mut dyn PcmSource,
    expected: f64,
    cancelled: &dyn Fn() -> bool,
) -> Result<SeekProbe, AudioError> {
    let (rate, channels) = source.format();
    if rate == 0 || channels == 0 {
        return Err(AudioError::Invalid);
    }

    let head_target = (EDGE_PROBE_SEC * rate as f64).ceil() as u64;
    let mut head = BandAnalyzer::with_duration(rate, Some(EDGE_PROBE_SEC));
    let (head_frames, head_start) = read_window(
        source,
        Some(head_target),
        head_target,
        0.0,
        &mut head,
        cancelled,
        true,
    )?;
    if head_frames == 0 {
        return Err(AudioError::Unsupported);
    }
    if head_frames < head_target {
        return Ok(SeekProbe::FallbackSequential);
    }

    let tail_target_sec = (expected - EDGE_PROBE_SEC).max(EDGE_PROBE_SEC);
    let Some(tail_seek_sec) = source.seek(tail_target_sec) else {
        return Ok(SeekProbe::FallbackSequential);
    };
    let max_tail_frames = ((EDGE_PROBE_SEC + 30.0) * rate as f64).ceil() as u64;
    let mut tail = BandAnalyzer::with_duration(rate, Some(EDGE_PROBE_SEC + 2.0));
    let (tail_frames, tail_start) = read_window(
        source,
        None,
        max_tail_frames,
        tail_seek_sec,
        &mut tail,
        cancelled,
        false,
    )?;
    if tail_frames == 0 || tail_frames >= max_tail_frames || tail_start < tail_target_sec - 15.0 {
        return Ok(SeekProbe::FallbackSequential);
    }

    let head_n = head.hop_sq.len().saturating_sub(BLOCKS_PER_FRAME - 1);
    let tail_n = tail.hop_sq.len().saturating_sub(BLOCKS_PER_FRAME - 1);
    if head_n == 0 || tail_n == 0 {
        return Ok(SeekProbe::FallbackSequential);
    }
    let max_sq = max_frame_sq(&head.hop_sq, head_n).max(max_frame_sq(&tail.hop_sq, tail_n));
    if max_sq <= SILENCE_FLOOR_SQ {
        return Ok(SeekProbe::FallbackSequential);
    }
    let threshold_sq = max_sq * 10f32.powf(AUDIBLE_DB / 10.0);
    let Some(first) = (0..head_n).position(|i| frame_sum_sq(&head.hop_sq, i) > threshold_sq) else {
        return Ok(SeekProbe::FallbackSequential);
    };
    let Some(last) = (0..tail_n).rposition(|i| frame_sum_sq(&tail.hop_sq, i) > threshold_sq) else {
        return Ok(SeekProbe::FallbackSequential);
    };

    let audible_start_sec = head_start + (first * HOP) as f64 / ANALYSIS_RATE as f64;
    let audible_end_sec =
        tail_start + ((last + BLOCKS_PER_FRAME) * HOP) as f64 / ANALYSIS_RATE as f64;
    let span_sec = audible_end_sec - audible_start_sec;
    if span_sec < MIN_AUDIBLE_SEC as f64 {
        return Ok(SeekProbe::FallbackSequential);
    }

    let audible_sec = span_sec.round().max(1.0) as u32;
    let win_sec = (FAST_WINDOW_SEC as f64).min(span_sec * 0.5);
    let win_target_frames = (win_sec * rate as f64).ceil() as u64;
    let tables = spectral_tables();
    let mut re = [0.0f32; FFT_SIZE];
    let mut im = [0.0f32; FFT_SIZE];
    let mut hashes = Vec::with_capacity(WINDOW_AT.len());

    for &at in &WINDOW_AT {
        let center = audible_start_sec + span_sec * at as f64;
        let win_start =
            (center - win_sec * 0.5).clamp(audible_start_sec, audible_end_sec - win_sec);
        let Some(seek_sec) = source.seek(win_start) else {
            return Ok(SeekProbe::FallbackSequential);
        };
        let mut analyzer = BandAnalyzer::with_duration(rate, Some(win_sec));
        let (got_frames, _) = read_window(
            source,
            Some(win_target_frames),
            win_target_frames,
            seek_sec,
            &mut analyzer,
            cancelled,
            false,
        )?;
        if got_frames == 0 {
            return Ok(SeekProbe::FallbackSequential);
        }
        let n = analyzer.hop_sq.len().saturating_sub(BLOCKS_PER_FRAME - 1);
        if n < SLICES {
            return Ok(SeekProbe::FallbackSequential);
        }
        let cur_frames: Vec<[f32; BANDS]> = (0..n)
            .map(|i| analyze_frame_bands(&analyzer.mono, i, tables, &mut re, &mut im))
            .collect();
        hashes.push(format!("{:02x}", window_bits(&cur_frames)));
    }

    Ok(SeekProbe::Done(Some(format!(
        "{audible_sec}s_{}",
        hashes.join("_")
    ))))
}

#[inline(always)]
fn quantize(s: f32) -> i64 {
    let x = s.clamp(-1.0, 1.0) * 8_388_608.0;
    (x + 0.5f32.copysign(x)) as i32 as i64
}

/// 64-bit FNV-1a over the samples quantized to 24 bits (exact for 16/24-bit integer sources),
/// seeded with the format. Leading and trailing silence is left out: decoders differ in how much
/// encoder delay they trim, and that must not change the hash.
struct PcmHash {
    hash: u64,
    channels: usize,
    started: bool,
    last_audible_hash: u64,
}

impl PcmHash {
    const PRIME: u64 = 0x0000_0100_0000_01B3;

    fn new(rate: u32, channels: usize) -> Self {
        let mut h = Self {
            hash: 0xCBF2_9CE4_8422_2325,
            channels,
            started: false,
            last_audible_hash: 0,
        };
        h.mix(rate as i64);
        h.mix(channels as i64);
        h.last_audible_hash = h.hash;
        h
    }

    fn mix(&mut self, v: i64) {
        self.hash = (self.hash ^ v as u64).wrapping_mul(Self::PRIME);
    }

    fn mix_samples(&mut self, samples: &[f32]) {
        let mut h = self.hash;
        for &s in samples {
            h = (h ^ quantize(s) as u64).wrapping_mul(Self::PRIME);
        }
        self.hash = h;
    }

    fn update(&mut self, samples: &[f32]) {
        let whole = samples.len() / self.channels * self.channels;
        let mut samples = &samples[..whole];
        if samples.is_empty() {
            return;
        }
        let is_audible = |s: &f32| s.abs() >= PCM_SILENCE;
        if !self.started {
            let Some(first_sample) = samples.iter().position(is_audible) else {
                return;
            };
            let first = first_sample / self.channels;
            self.started = true;
            samples = &samples[first * self.channels..];
        }
        if let Some(last_sample) = samples.iter().rposition(is_audible) {
            let last = last_sample / self.channels;
            let end = (last + 1) * self.channels;
            self.mix_samples(&samples[..end]);
            self.last_audible_hash = self.hash;
            samples = &samples[end..];
        }
        self.mix_samples(samples);
    }

    fn finish(&self) -> String {
        format!("{:016x}", self.last_audible_hash)
    }
}

const BLOCKS_PER_FRAME: usize = FFT_SIZE / HOP;
const _: () = assert!(FFT_SIZE.is_multiple_of(HOP));

struct SpectralTables {
    window: [f32; FFT_SIZE],
    sub_band_bins: [(usize, usize); SUB_BANDS],
    bitrev: [u16; FFT_SIZE],
    cos: [f32; FFT_SIZE / 2],
    sin: [f32; FFT_SIZE / 2],
}

fn spectral_tables() -> &'static SpectralTables {
    static TABLES: OnceLock<SpectralTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let window =
            std::array::from_fn(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / FFT_SIZE as f32).cos());
        let hz_per_bin = ANALYSIS_RATE as f32 / FFT_SIZE as f32;
        let edge = |i: usize| LOW_HZ * (HIGH_HZ / LOW_HZ).powf(i as f32 / SUB_BANDS as f32);
        let sub_band_bins = std::array::from_fn(|b| {
            let lo = (edge(b) / hz_per_bin).round() as usize;
            (
                lo,
                ((edge(b + 1) / hz_per_bin).round() as usize).max(lo + 1),
            )
        });
        let bitrev = std::array::from_fn(|mut i| {
            let mut rev = 0usize;
            let mut bit = FFT_SIZE >> 1;
            while i > 0 {
                if i & 1 != 0 {
                    rev |= bit;
                }
                i >>= 1;
                bit >>= 1;
            }
            rev as u16
        });
        let cos = std::array::from_fn(|k| (-2.0 * PI * k as f32 / FFT_SIZE as f32).cos());
        let sin = std::array::from_fn(|k| (-2.0 * PI * k as f32 / FFT_SIZE as f32).sin());
        SpectralTables {
            window,
            sub_band_bins,
            bitrev,
            cos,
            sin,
        }
    })
}

/// Resampled mono signal at [`ANALYSIS_RATE`] and per-hop energy; FFT runs lazily only on the
/// three target windows once the audible span is known.
struct BandAnalyzer {
    rate: u32,
    /// Box-filter decimation state.
    phase: u32,
    sum: f32,
    count: u32,
    mono: Vec<f32>,
    hop_sq: Vec<f32>,
    cur_hop_sq: f32,
    cur_hop_len: usize,
}

impl BandAnalyzer {
    #[cfg(test)]
    fn new(rate: u32) -> Self {
        Self::with_duration(rate, None)
    }

    fn with_duration(rate: u32, duration_sec: Option<f64>) -> Self {
        let samples = duration_sec
            .filter(|d| d.is_finite() && *d > 0.0)
            .map_or(0, |d| (d.min(7200.0) * ANALYSIS_RATE as f64) as usize);
        Self {
            rate,
            phase: 0,
            sum: 0.0,
            count: 0,
            mono: Vec::with_capacity(samples),
            hop_sq: Vec::with_capacity(if samples > 0 { samples / HOP + 1 } else { 0 }),
            cur_hop_sq: 0.0,
            cur_hop_len: 0,
        }
    }

    fn push_samples(&mut self, samples: &[f32], channels: usize) {
        if self.rate == ANALYSIS_RATE * 4 && (channels == 1 || channels == 2) {
            let mut rest = samples;
            while !rest.is_empty() && (self.phase != 0 || self.count != 0) {
                if rest.len() < channels {
                    return;
                }
                let (frame, tail) = rest.split_at(channels);
                let x = if channels == 2 {
                    (frame[0] + frame[1]) * 0.5
                } else {
                    frame[0]
                };
                self.push(x);
                rest = tail;
            }
            if channels == 2 {
                let (chunks, rem) = rest.as_chunks::<8>();
                for c in chunks {
                    self.push_resampled(
                        (c[0] + c[1] + c[2] + c[3] + c[4] + c[5] + c[6] + c[7]) * 0.125,
                    );
                }
                for frame in rem.as_chunks::<2>().0 {
                    self.push((frame[0] + frame[1]) * 0.5);
                }
            } else {
                let (chunks, rem) = rest.as_chunks::<4>();
                for c in chunks {
                    self.push_resampled((c[0] + c[1] + c[2] + c[3]) * 0.25);
                }
                for &x in rem {
                    self.push(x);
                }
            }
            return;
        }
        match channels {
            1 => {
                for &x in samples {
                    self.push(x);
                }
            }
            2 => {
                for frame in samples.as_chunks::<2>().0 {
                    self.push((frame[0] + frame[1]) * 0.5);
                }
            }
            _ => {
                let inv = 1.0 / channels as f32;
                for frame in samples.chunks_exact(channels) {
                    self.push(frame.iter().sum::<f32>() * inv);
                }
            }
        }
    }

    /// One mono sample at the source rate.
    #[inline]
    fn push(&mut self, x: f32) {
        self.sum += x;
        self.count += 1;
        self.phase += ANALYSIS_RATE;
        // One averaged sample per analysis period (repeated when upsampling).
        if self.phase >= self.rate {
            let v = self.sum / self.count as f32;
            (self.sum, self.count) = (0.0, 0);
            while self.phase >= self.rate {
                self.phase -= self.rate;
                self.push_resampled(v);
            }
        }
    }

    #[inline]
    fn push_resampled(&mut self, v: f32) {
        self.mono.push(v);
        self.cur_hop_sq += v * v;
        self.cur_hop_len += 1;
        if self.cur_hop_len == HOP {
            self.hop_sq.push(self.cur_hop_sq);
            self.cur_hop_sq = 0.0;
            self.cur_hop_len = 0;
        }
    }
}

fn analyze_frame_bands(
    mono: &[f32],
    frame_idx: usize,
    tables: &SpectralTables,
    re: &mut [f32; FFT_SIZE],
    im: &mut [f32; FFT_SIZE],
) -> [f32; BANDS] {
    let samples = &mono[frame_idx * HOP..frame_idx * HOP + FFT_SIZE];
    for (dst, &rev) in re.iter_mut().zip(&tables.bitrev) {
        let idx = rev as usize;
        *dst = samples[idx] * tables.window[idx];
    }
    for pair in re.as_chunks_mut::<2>().0 {
        let (a, b) = (pair[0], pair[1]);
        pair[0] = a + b;
        pair[1] = a - b;
    }
    im.fill(0.0);
    fft_stages(&tables.cos, &tables.sin, re, im, 4);
    let mut bands = [0.0f32; BANDS];
    let per_band = SUB_BANDS / BANDS;
    for (s, &(lo, hi)) in tables.sub_band_bins.iter().enumerate() {
        let e = (lo..hi).map(|k| re[k] * re[k] + im[k] * im[k]).sum::<f32>() / (hi - lo) as f32;
        bands[s / per_band] += e / per_band as f32;
    }
    bands
}

#[cfg(test)]
fn fft_run<const N: usize>(cos: &[f32], sin: &[f32], re: &mut [f32; N], im: &mut [f32; N]) {
    let n = N;
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    fft_stages(cos, sin, re, im, 2);
}

fn fft_stages<const N: usize>(
    cos: &[f32],
    sin: &[f32],
    re: &mut [f32; N],
    im: &mut [f32; N],
    mut len: usize,
) {
    let n = N;
    while len <= n {
        let step = n / len;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (c, s) = (cos[k * step], sin[k * step]);
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * c - im[b] * s, re[b] * s + im[b] * c);
                (re[b], im[b]) = (re[a] - tr, im[a] - ti);
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

/// Radix-2 FFT with precomputed twiddles.
#[cfg(test)]
struct Fft {
    cos: Vec<f32>,
    sin: Vec<f32>,
}

#[cfg(test)]
impl Fft {
    fn new(n: usize) -> Self {
        let angle = |k: usize| -2.0 * PI * k as f32 / n as f32;
        Self {
            cos: (0..n / 2).map(|k| angle(k).cos()).collect(),
            sin: (0..n / 2).map(|k| angle(k).sin()).collect(),
        }
    }

    /// In place; `re.len()` must be the size given to `new`.
    fn run<const N: usize>(&self, re: &mut [f32; N], im: &mut [f32; N]) {
        fft_run(&self.cos, &self.sin, re, im);
    }
}

#[inline]
fn frame_sum_sq(hop_sq: &[f32], i: usize) -> f32 {
    hop_sq[i] + hop_sq[i + 1] + hop_sq[i + 2] + hop_sq[i + 3]
}

fn max_frame_sq(hop_sq: &[f32], n: usize) -> f32 {
    (0..n)
        .map(|i| frame_sum_sq(hop_sq, i))
        .fold(0.0f32, f32::max)
}

/// `None` when there is too little audible audio to describe (falls back to PCM hash if `< 3` s).
fn fingerprint_with_window(
    analyzer: &BandAnalyzer,
    window_sec: f32,
    pcm_fallback: Option<&str>,
) -> Option<String> {
    let mono = &analyzer.mono;
    let hop_sq = &analyzer.hop_sq;
    let n = hop_sq.len().saturating_sub(BLOCKS_PER_FRAME - 1);
    if n == 0 {
        return pcm_fallback.map(|h| format!("1s_pcm_{h}"));
    }
    let max_sq = max_frame_sq(hop_sq, n);
    if max_sq <= SILENCE_FLOOR_SQ {
        // The last, incomplete FFT frame may contain the only sound in the file.
        // Include any input samples still waiting in the resampler.
        let pending = analyzer.sum / analyzer.count.max(1) as f32;
        let tail_sq = hop_sq[n..].iter().sum::<f32>() + analyzer.cur_hop_sq + pending * pending;
        if tail_sq > SILENCE_FLOOR_SQ {
            return pcm_fallback.map(|h| format!("1s_pcm_{h}"));
        }
        return None;
    }
    let threshold_sq = max_sq * 10f32.powf(AUDIBLE_DB / 10.0);
    let first = (0..n).position(|i| frame_sum_sq(hop_sq, i) > threshold_sq)?;
    let last = (0..n).rposition(|i| frame_sum_sq(hop_sq, i) > threshold_sq)?;
    let frames_per_sec = ANALYSIS_RATE as f32 / HOP as f32;
    let span = last - first + 1;
    let audible_sec = (((span + BLOCKS_PER_FRAME - 1) * HOP) as f64 / ANALYSIS_RATE as f64)
        .round()
        .max(1.0) as u32;
    if (span as f32) < MIN_AUDIBLE_SEC * frames_per_sec {
        return pcm_fallback.map(|h| format!("{audible_sec}s_pcm_{h}"));
    }
    let window = ((window_sec * frames_per_sec) as usize)
        .min(span / 2)
        .max(SLICES)
        .min(span);
    let tables = spectral_tables();
    let mut re = [0.0f32; FFT_SIZE];
    let mut im = [0.0f32; FFT_SIZE];
    let mut prev_start = 0usize;
    let mut prev_frames: Vec<[f32; BANDS]> = Vec::new();
    let mut hashes = Vec::with_capacity(WINDOW_AT.len());

    for &at in &WINDOW_AT {
        let center = first + (span as f32 * at) as usize;
        let start = center
            .saturating_sub(window / 2)
            .clamp(first, first + span - window);
        let prev_end = prev_start + prev_frames.len();
        let mut cur_frames = Vec::with_capacity(window);
        for i in start..start + window {
            let bands = if i >= prev_start && i < prev_end {
                prev_frames[i - prev_start]
            } else {
                analyze_frame_bands(mono, i, tables, &mut re, &mut im)
            };
            cur_frames.push(bands);
        }
        hashes.push(format!("{:02x}", window_bits(&cur_frames)));
        prev_start = start;
        prev_frames = cur_frames;
    }
    Some(format!("{audible_sec}s_{}", hashes.join("_")))
}

/// Signs of the change of the spectral slope between neighbouring slices (Haitsma–Kalker style,
/// on long slices): (SLICES-1)*(BANDS-1) bits.
fn window_bits(frames: &[[f32; BANDS]]) -> u32 {
    // Mean of the log energies (geometric mean): a few loud frames don't dominate a slice.
    let mut energy = [[0.0f32; BANDS]; SLICES];
    for (t, slice) in split_even(frames, SLICES).enumerate() {
        for (b, e) in energy[t].iter_mut().enumerate() {
            *e = slice
                .iter()
                .map(|bands| (bands[b] + 1e-3).ln())
                .sum::<f32>()
                / slice.len().max(1) as f32;
        }
    }
    let mut bits = 0u32;
    for t in 0..SLICES - 1 {
        for b in 0..BANDS - 1 {
            let now = energy[t][b] - energy[t][b + 1];
            let next = energy[t + 1][b] - energy[t + 1][b + 1];
            bits = bits << 1 | (next - now > 0.0) as u32;
        }
    }
    bits
}

/// `parts` consecutive runs whose lengths differ by at most one (longer ones first).
fn split_even<T>(items: &[T], parts: usize) -> impl Iterator<Item = &[T]> {
    let (base, extra) = (items.len() / parts, items.len() % parts);
    let mut start = 0;
    (0..parts).map(move |i| {
        let len = base + (i < extra) as usize;
        let run = &items[start..start + len];
        start += len;
        run
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_audio_does_not_get_a_hash_of_its_valid_prefix() {
        let path = crate::test_util::pcm16_wav("truncated-analysis", 8000, 1, 32000, |i| {
            (i % 1000) as i16 * 16
        });
        assert!(analyze_audio(&path, &|| false).is_ok());
        assert!(fast_fingerprint_audio(&path, &|| false).is_ok());
        assert!(decoded_duration(&path, &|| false).is_ok());
        let mut bytes = std::fs::read(&path).unwrap();
        // The same prefix, with a header declaring five times as much PCM.
        bytes[4..8].copy_from_slice(&(36u32 + 32000 * 2 * 5).to_le_bytes());
        bytes[40..44].copy_from_slice(&(32000u32 * 2 * 5).to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            analyze_audio(&path, &|| false),
            Err(AudioError::Invalid)
        ));
        assert!(matches!(
            fast_fingerprint_audio(&path, &|| false),
            Err(AudioError::Invalid)
        ));
        assert!(matches!(
            decoded_duration(&path, &|| false),
            Err(AudioError::Invalid)
        ));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn errors_after_decoded_samples_are_not_clean_eof() {
        struct Broken {
            read: bool,
        }
        impl PcmSource for Broken {
            fn format(&self) -> (u32, usize) {
                (8000, 1)
            }
            fn duration(&self) -> Option<f64> {
                None
            }
            fn next_timed_samples(&mut self) -> std::io::Result<Option<(Option<f64>, &[f32])>> {
                if std::mem::replace(&mut self.read, true) {
                    Err(std::io::Error::other("broken packet"))
                } else {
                    Ok(Some((Some(0.0), &[0.5; 256])))
                }
            }
            fn seek(&mut self, _: f64) -> Option<f64> {
                None
            }
        }
        assert!(matches!(
            analyze_source::<(BandAnalyzer, PcmHash)>(&mut Broken { read: false }, &|| false),
            Err(AudioError::Invalid)
        ));
        assert!(matches!(
            analyze_source::<FastSequential>(&mut Broken { read: false }, &|| false),
            Err(AudioError::Invalid)
        ));
        assert!(matches!(
            analyze_source::<()>(&mut Broken { read: false }, &|| false),
            Err(AudioError::Invalid)
        ));
    }

    #[test]
    fn short_tracks_with_different_spectra_do_not_collide() {
        let rate = 11_025u32;
        let frames = rate * 10;
        // Track A: tone rises in frequency over 10 s; Track B: tone falls over 10 s.
        let make_sweep = |name: &str, ascending: bool| {
            let dur = frames as f32 / rate as f32;
            let (f0, f1) = if ascending {
                (250.0f32, 3250.0f32)
            } else {
                (3250.0f32, 250.0f32)
            };
            crate::test_util::pcm16_wav(name, rate, 1, frames, move |i| {
                let t = i as f32 / rate as f32;
                let phase = 2.0 * PI * (f0 * t + 0.5 * (f1 - f0) * t * t / dur);
                (phase.sin() * 12000.0) as i16
            })
        };
        let a = make_sweep("short-asc", true);
        let b = make_sweep("short-desc", false);
        let fp_a = analyze_audio(&a, &|| false).unwrap().fingerprint.unwrap();
        let fp_b = analyze_audio(&b, &|| false).unwrap().fingerprint.unwrap();
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
        assert!(fp_a.starts_with("10s_"), "{fp_a}");
        assert!(fp_b.starts_with("10s_"), "{fp_b}");
        assert_ne!(fp_a, fp_b);
        let parts: Vec<&str> = fp_a.split('_').collect();
        assert_eq!(parts.len(), 4);
        assert!(parts[1] != parts[2] || parts[2] != parts[3], "{fp_a}");
    }

    #[test]
    fn very_short_audible_track_falls_back_to_pcm_hash_while_silence_stays_empty() {
        let rate = 11_025u32;
        let frames = rate * 16 / 10; // 1.6 s < MIN_AUDIBLE_SEC
        let path = crate::test_util::pcm16_wav("tiny-audible", rate, 1, frames, |i| {
            ((2.0 * PI * 440.0 * i as f32 / rate as f32).sin() * 12000.0) as i16
        });
        let analysis = analyze_audio(&path, &|| false).unwrap();
        let fast = fast_fingerprint_audio(&path, &|| false).unwrap();
        let _ = std::fs::remove_file(path);
        let fp = analysis.fingerprint.expect("PCM fallback expected");
        assert!(fp.starts_with("2s_pcm_"), "{fp}");
        assert!(fp.ends_with(&analysis.pcm_hash), "{fp}");
        assert_eq!(fast.fingerprint.as_deref(), Some(fp.as_str()));

        let silent = crate::test_util::pcm16_wav("tiny-silent", rate, 1, frames, |_| 0);
        assert_eq!(analyze_audio(&silent, &|| false).unwrap().fingerprint, None);
        assert_eq!(
            fast_fingerprint_audio(&silent, &|| false)
                .unwrap()
                .fingerprint,
            None
        );
        let _ = std::fs::remove_file(silent);
    }

    #[test]
    fn fast_fingerprint_handles_long_tracks_and_edge_silence() {
        let rate = 11_025u32;
        let make_long = |name: &str, lead_silence_sec: u32, trail_silence_sec: u32| {
            let audible_sec = 80u32;
            let total = rate * (lead_silence_sec + audible_sec + trail_silence_sec);
            let dur = audible_sec as f32;
            crate::test_util::pcm16_wav(name, rate, 1, total, move |i| {
                let sec = i / rate;
                if sec < lead_silence_sec || sec >= lead_silence_sec + audible_sec {
                    0
                } else {
                    let t = (i - lead_silence_sec * rate) as f32 / rate as f32;
                    let phase = 2.0 * PI * (250.0 * t + 0.5 * 3000.0 * t * t / dur);
                    (phase.sin() * 12000.0) as i16
                }
            })
        };
        let clean = make_long("long-clean", 0, 0);
        let padded = make_long("long-padded", 2, 2);
        let fp_clean = fast_fingerprint_audio(&clean, &|| false)
            .unwrap()
            .fingerprint
            .unwrap();
        let fp_padded = fast_fingerprint_audio(&padded, &|| false)
            .unwrap()
            .fingerprint
            .unwrap();
        let seq_clean = analyze_audio(&clean, &|| false)
            .unwrap()
            .fingerprint
            .unwrap();
        let seq_padded = analyze_audio(&padded, &|| false)
            .unwrap()
            .fingerprint
            .unwrap();
        let _ = std::fs::remove_file(clean);
        let _ = std::fs::remove_file(padded);
        assert!(fp_clean.starts_with("80s_"), "{fp_clean}");
        assert_eq!(fp_clean, fp_padded);
        assert!(seq_clean.starts_with("80s_"), "{seq_clean}");
        assert_eq!(seq_clean, seq_padded);
    }

    #[test]
    fn fft_finds_a_tone() {
        let n = 64;
        let mut re: [f32; 64] =
            std::array::from_fn(|i| (2.0 * PI * 5.0 * i as f32 / n as f32).cos());
        let mut im = [0.0; 64];
        Fft::new(n).run(&mut re, &mut im);
        let peak = (0..n / 2)
            .max_by(|&a, &b| re[a].hypot(im[a]).total_cmp(&re[b].hypot(im[b])))
            .unwrap();
        assert_eq!(peak, 5);
    }

    #[test]
    fn silence_has_no_fingerprint() {
        let mut analyzer = BandAnalyzer::new(ANALYSIS_RATE);
        for _ in 0..(120 * ANALYSIS_RATE as usize) {
            analyzer.push_resampled(0.0);
        }
        assert_eq!(fingerprint_with_window(&analyzer, WINDOW_SEC, None), None);
    }

    #[test]
    fn even_split() {
        let v: Vec<usize> = (0..10).collect();
        let lens: Vec<usize> = split_even(&v, 3).map(|s| s.len()).collect();
        assert_eq!(lens, [4, 3, 3]);
    }

    fn pcm_hash(samples: &[f32]) -> String {
        let mut h = PcmHash::new(44100, 1);
        h.update(samples);
        h.finish()
    }

    #[test]
    fn low_rate_constant_stays_constant() {
        let mut analyzer = BandAnalyzer::new(8000);
        analyzer.push_samples(&[1.0; 8000], 1);
        assert_eq!(analyzer.mono, vec![1.0; 11025]);
    }

    #[test]
    fn audible_clip_shorter_than_fft_uses_pcm() {
        let path = crate::test_util::pcm16_wav("subfft", 22050, 1, 1102, |_| 9000);
        let full = analyze_audio(&path, &|| false).unwrap();
        let fast = fast_fingerprint_audio(&path, &|| false).unwrap();
        assert_eq!(full.fingerprint, Some(format!("1s_pcm_{}", full.pcm_hash)));
        assert_eq!(full.fingerprint, fast.fingerprint);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn brief_impulse_has_the_same_fingerprints_with_edge_silence() {
        for length in [1, 30] {
            let mut reference = None;
            for (lead, tail) in [(0, 0), (0, 22050), (22050, 0)] {
                let path = crate::test_util::pcm16_wav(
                    "edge-impulse",
                    22050,
                    1,
                    lead + length + tail,
                    |i| {
                        if (lead..lead + length).contains(&i) {
                            20000
                        } else {
                            0
                        }
                    },
                );
                let full = analyze_audio(&path, &|| false).unwrap();
                let fast = fast_fingerprint_audio(&path, &|| false).unwrap();
                std::fs::remove_file(path).unwrap();
                let expected = reference.get_or_insert_with(|| format!("1s_pcm_{}", full.pcm_hash));
                assert_eq!(full.fingerprint.as_deref(), Some(expected.as_str()));
                assert_eq!(fast.fingerprint, full.fingerprint);
            }
        }
    }

    #[test]
    fn pcm_hash_ignores_edge_silence_only() {
        let audio = [0.5, -0.25, 0.0, 0.125];
        let padded = [0.0, 0.0, 0.5, -0.25, 0.0, 0.125, 0.0];
        assert_eq!(pcm_hash(&audio), pcm_hash(&padded));
        assert_ne!(pcm_hash(&audio), pcm_hash(&[0.5, -0.25, 0.125]));
        assert_ne!(pcm_hash(&audio), pcm_hash(&[0.5, -0.25, 0.0, 0.126]));
        let mut stereo = PcmHash::new(44100, 2);
        stereo.update(&audio);
        let before = stereo.finish();
        for _ in 0..1025 {
            stereo.update(&[0.0; 4096]);
        }
        assert_eq!(stereo.finish(), before);
        assert_ne!(stereo.finish(), pcm_hash(&audio));
    }
}
