//! Independent audio duplicate detection fields, each computed only when requested:
//!
//! - **PCM hash** — hash of the decoded samples between the leading and trailing silence: equal for
//!   the same audio in any lossless container or with different tags (WAV → FLAC, retagged MP3).
//! - **Fingerprint** — `<seconds>s_<h25>_<h50>_<h75>`: coarse spectral hashes of three windows at
//!   25/50/75 % of the non-silent part. Built to survive re-encoding (bitrate, codec, sample rate,
//!   mono, gain), because TC's duplicate search compares field values for equality — there is no
//!   fuzzy matching, so every bit has to be stable. That is why there are only 6 bits per window:
//!   tuned on re-encoded copies of real tracks, ~99 % of copies give exactly the original's value.
//!
//! The signal is downmixed to mono and resampled to [`ANALYSIS_RATE`]; per frame only a few band
//! energies are kept, so hours of audio are fine.

use std::f32::consts::PI;
use std::path::Path;
use std::sync::OnceLock;

use crate::audio_decode::AudioDecoder;
use crate::cache::{AudioDuration, AudioFingerprint, AudioPcmHash};
use crate::mf_audio::MfAudioReader;

const ANALYSIS_RATE: u32 = 11_025;
const FFT_SIZE: usize = 1024;
/// ~23 ms: window positions must not jump between copies.
const HOP: usize = 256;
/// Log-spaced sub-bands between these frequencies, averaged in groups into [`BANDS`] bands.
const LOW_HZ: f32 = 150.0;
const HIGH_HZ: f32 = 5000.0;
const SUB_BANDS: usize = 24;
const BANDS: usize = 4;
const SLICES: usize = 3;
const WINDOW_SEC: f32 = 30.0;
const WINDOW_AT: [f32; 3] = [0.25, 0.5, 0.75];
/// Start / end of the audible part: frames louder than this relative to the loudest (dB).
const AUDIBLE_DB: f32 = -30.0;
/// Shortest audible part that gets a fingerprint.
const MIN_AUDIBLE_SEC: f32 = 3.0;
/// Samples quieter than this (below one 16-bit step) are silence for the PCM hash.
const PCM_SILENCE: f32 = 1.5 / 32768.0;
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
    fn next_samples(&mut self) -> std::io::Result<Option<&[f32]>>;
}

impl PcmSource for AudioDecoder {
    fn format(&self) -> (u32, usize) {
        (self.sample_rate, self.channels as usize)
    }

    fn duration(&self) -> Option<f64> {
        self.duration
    }

    fn next_samples(&mut self) -> std::io::Result<Option<&[f32]>> {
        self.next_chunk_checked().map(|c| c.map(|c| c.samples))
    }
}

impl PcmSource for MfAudioReader {
    fn format(&self) -> (u32, usize) {
        (self.sample_rate, self.channels as usize)
    }

    fn duration(&self) -> Option<f64> {
        self.duration
    }

    fn next_samples(&mut self) -> std::io::Result<Option<&[f32]>> {
        self.next_chunk()
    }
}

pub fn fingerprint_audio(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<AudioFingerprint, AudioError> {
    analyze_audio::<BandAnalyzer>(path, cancelled)
}

pub fn hash_audio_pcm(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<AudioPcmHash, AudioError> {
    analyze_audio::<PcmHash>(path, cancelled)
}

/// Counts decoded frames without hashing samples or analyzing their spectrum.
pub fn decoded_duration(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<AudioDuration, AudioError> {
    analyze_audio::<()>(path, cancelled)
}

trait SampleAnalysis {
    type Output;
    fn start(rate: u32, channels: usize, duration_sec: Option<f64>) -> Self;
    fn samples(&mut self, samples: &[f32], channels: usize);
    fn result(self, duration: f64) -> Self::Output;
}

impl SampleAnalysis for BandAnalyzer {
    type Output = AudioFingerprint;

    fn start(rate: u32, _: usize, duration_sec: Option<f64>) -> Self {
        Self::with_duration(rate, duration_sec)
    }

    fn samples(&mut self, samples: &[f32], channels: usize) {
        self.push_samples(samples, channels);
    }

    fn result(self, duration: f64) -> Self::Output {
        AudioFingerprint {
            duration_sec: duration.round() as u32,
            fingerprint: fingerprint(&self.mono, &self.hop_sq, duration),
        }
    }
}

impl SampleAnalysis for PcmHash {
    type Output = AudioPcmHash;

    fn start(rate: u32, channels: usize, _: Option<f64>) -> Self {
        Self::new(rate, channels)
    }

    fn samples(&mut self, samples: &[f32], _: usize) {
        self.update(samples);
    }

    fn result(self, duration: f64) -> Self::Output {
        AudioPcmHash {
            duration_sec: duration.round() as u32,
            pcm_hash: self.finish(),
        }
    }
}

impl SampleAnalysis for () {
    type Output = AudioDuration;

    fn start(_: u32, _: usize, _: Option<f64>) -> Self {}
    fn samples(&mut self, _: &[f32], _: usize) {}
    fn result(self, duration: f64) -> Self::Output {
        AudioDuration(duration.round() as u32)
    }
}

/// Decodes with symphonia; files it can't open or decode go through Media Foundation, whose
/// results depend on the system's decoders (still stable on one machine, which is what TC's
/// duplicate search compares).
fn analyze_audio<A: SampleAnalysis>(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<A::Output, AudioError> {
    if cancelled() {
        return Err(AudioError::Cancelled);
    }
    if let Some(mut decoder) = AudioDecoder::open_sequential(path) {
        match analyze_source::<A>(&mut decoder, cancelled) {
            Err(AudioError::Unsupported) => {}
            result => return result,
        }
    }
    let mut reader = MfAudioReader::open(path).ok_or(AudioError::Unsupported)?;
    analyze_source::<A>(&mut reader, cancelled)
}

fn analyze_source<A: SampleAnalysis>(
    source: &mut dyn PcmSource,
    cancelled: &dyn Fn() -> bool,
) -> Result<A::Output, AudioError> {
    let (rate, channels) = source.format();
    if rate == 0 || channels == 0 {
        return Err(AudioError::Invalid);
    }
    let expected = source.duration();
    let mut analysis = A::start(rate, channels, expected);
    let mut frames_total: u64 = 0;
    let mut chunks = 0u32;

    while let Some(samples) = source.next_samples().map_err(|_| {
        if frames_total == 0 {
            AudioError::Unsupported
        } else {
            AudioError::Invalid
        }
    })? {
        chunks += 1;
        if chunks.is_multiple_of(CANCEL_CHECK_EVERY) && cancelled() {
            return Err(AudioError::Cancelled);
        }
        analysis.samples(samples, channels);
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
    Ok(analysis.result(duration))
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
    /// Silent frames seen since the last audible one; hashed once audio resumes.
    pending: Vec<f32>,
}

impl PcmHash {
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    /// A longer silence inside the track is hashed right away (bounds memory).
    const MAX_PENDING: usize = 1 << 22;

    fn new(rate: u32, channels: usize) -> Self {
        let mut h = Self {
            hash: 0xCBF2_9CE4_8422_2325,
            channels,
            started: false,
            pending: Vec::new(),
        };
        h.mix(rate as i64);
        h.mix(channels as i64);
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

    fn flush_pending(&mut self) {
        let mut h = self.hash;
        for &s in &self.pending {
            h = (h ^ quantize(s) as u64).wrapping_mul(Self::PRIME);
        }
        self.hash = h;
        self.pending.clear();
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
            if !self.pending.is_empty() {
                self.flush_pending();
            }
            let active_end = (last + 1) * self.channels;
            self.mix_samples(&samples[..active_end]);
            let tail = &samples[active_end..];
            if !tail.is_empty() {
                self.pending.extend_from_slice(tail);
                if self.pending.len() > Self::MAX_PENDING {
                    self.flush_pending();
                }
            }
        } else {
            self.pending.extend_from_slice(samples);
            if self.pending.len() > Self::MAX_PENDING {
                self.flush_pending();
            }
        }
    }

    fn finish(&self) -> String {
        format!("{:016x}", self.hash)
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
        while self.phase >= self.rate {
            self.phase -= self.rate;
            let v = self.sum / self.count.max(1) as f32;
            (self.sum, self.count) = (0.0, 0);
            self.push_resampled(v);
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

/// `None` when there is too little audible audio to describe.
fn fingerprint(mono: &[f32], hop_sq: &[f32], duration: f64) -> Option<String> {
    let n = hop_sq.len().saturating_sub(BLOCKS_PER_FRAME - 1);
    if n == 0 {
        return None;
    }
    let sum_sq = |i: usize| hop_sq[i] + hop_sq[i + 1] + hop_sq[i + 2] + hop_sq[i + 3];
    let max_sq = (0..n).map(sum_sq).fold(0.0f32, f32::max);
    let threshold_sq = max_sq * 10f32.powf(AUDIBLE_DB / 10.0);
    let first = (0..n).position(|i| sum_sq(i) > threshold_sq)?;
    let last = (0..n).rposition(|i| sum_sq(i) > threshold_sq)?;
    let frames_per_sec = ANALYSIS_RATE as f32 / HOP as f32;
    let span = last - first + 1;
    if (span as f32) < MIN_AUDIBLE_SEC * frames_per_sec {
        return None;
    }
    let window = ((WINDOW_SEC * frames_per_sec) as usize).min(n);
    let tables = spectral_tables();
    let mut re = [0.0f32; FFT_SIZE];
    let mut im = [0.0f32; FFT_SIZE];
    let mut prev_start = 0usize;
    let mut prev_frames: Vec<[f32; BANDS]> = Vec::new();
    let mut hashes = Vec::with_capacity(WINDOW_AT.len());

    for &at in &WINDOW_AT {
        let center = first + (span as f32 * at) as usize;
        let start = center.saturating_sub(window / 2).min(n - window);
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
    Some(format!("{}s_{}", duration.round() as u32, hashes.join("_")))
}

/// Signs of the change of the spectral slope between neighbouring slices (Haitsma–Kalker style,
/// on long slices): (SLICES-1)*(BANDS-1) bits.
fn window_bits(frames: &[[f32; BANDS]]) -> u32 {
    // Mean of the log energies (geometric mean): a few loud frames don't dominate a slice.
    let mut energy = [[0.0f32; BANDS]; SLICES];
    for (t, slice) in split_even(frames, SLICES).enumerate() {
        for (b, e) in energy[t].iter_mut().enumerate() {
            *e = slice.iter().map(|bands| (bands[b] + 1e-9).ln()).sum::<f32>()
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
        assert!(fingerprint_audio(&path, &|| false).is_ok());
        assert!(hash_audio_pcm(&path, &|| false).is_ok());
        assert!(decoded_duration(&path, &|| false).is_ok());
        let mut bytes = std::fs::read(&path).unwrap();
        // The same prefix, with a header declaring five times as much PCM.
        bytes[4..8].copy_from_slice(&(36u32 + 32000 * 2 * 5).to_le_bytes());
        bytes[40..44].copy_from_slice(&(32000u32 * 2 * 5).to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            fingerprint_audio(&path, &|| false),
            Err(AudioError::Invalid)
        ));
        assert!(matches!(
            hash_audio_pcm(&path, &|| false),
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
            fn next_samples(&mut self) -> std::io::Result<Option<&[f32]>> {
                if std::mem::replace(&mut self.read, true) {
                    Err(std::io::Error::other("broken packet"))
                } else {
                    Ok(Some(&[0.5; 256]))
                }
            }
        }
        assert!(matches!(
            analyze_source::<BandAnalyzer>(&mut Broken { read: false }, &|| false),
            Err(AudioError::Invalid)
        ));
        assert!(matches!(
            analyze_source::<PcmHash>(&mut Broken { read: false }, &|| false),
            Err(AudioError::Invalid)
        ));
        assert!(matches!(
            analyze_source::<()>(&mut Broken { read: false }, &|| false),
            Err(AudioError::Invalid)
        ));
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
        assert_eq!(fingerprint(&analyzer.mono, &analyzer.hop_sq, 120.0), None);
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
    fn pcm_hash_ignores_edge_silence_only() {
        let audio = [0.5, -0.25, 0.0, 0.125];
        let padded = [0.0, 0.0, 0.5, -0.25, 0.0, 0.125, 0.0];
        assert_eq!(pcm_hash(&audio), pcm_hash(&padded));
        assert_ne!(pcm_hash(&audio), pcm_hash(&[0.5, -0.25, 0.125]));
        assert_ne!(pcm_hash(&audio), pcm_hash(&[0.5, -0.25, 0.0, 0.126]));
        let mut stereo = PcmHash::new(44100, 2);
        stereo.update(&audio);
        assert_ne!(stereo.finish(), pcm_hash(&audio));
    }
}
