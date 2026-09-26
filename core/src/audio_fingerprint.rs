//! Audio duplicate detection fields, computed in one decoding pass:
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

use crate::audio_decode::AudioDecoder;
use crate::cache::AudioAnalysis;
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
}

/// Interleaved PCM from either decoder.
trait PcmSource {
    fn format(&self) -> (u32, usize);
    fn next_samples(&mut self) -> Option<&[f32]>;
}

impl PcmSource for AudioDecoder {
    fn format(&self) -> (u32, usize) {
        (self.sample_rate, self.channels as usize)
    }

    fn next_samples(&mut self) -> Option<&[f32]> {
        self.next_chunk().map(|c| c.samples)
    }
}

impl PcmSource for MfAudioReader {
    fn format(&self) -> (u32, usize) {
        (self.sample_rate, self.channels as usize)
    }

    fn next_samples(&mut self) -> Option<&[f32]> {
        self.next_chunk()
    }
}

/// Decodes with symphonia; files it can't open or decode go through Media Foundation, whose
/// results depend on the system's decoders (still stable on one machine, which is what TC's
/// duplicate search compares).
pub fn analyze_audio(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<AudioAnalysis, AudioError> {
    if let Some(mut decoder) = AudioDecoder::open(path) {
        match analyze_source(&mut decoder, cancelled) {
            Err(AudioError::Unsupported) => {}
            result => return result,
        }
    }
    let mut reader = MfAudioReader::open(path).ok_or(AudioError::Unsupported)?;
    analyze_source(&mut reader, cancelled)
}

fn analyze_source(
    source: &mut dyn PcmSource,
    cancelled: &dyn Fn() -> bool,
) -> Result<AudioAnalysis, AudioError> {
    let (rate, channels) = source.format();
    let mut pcm = PcmHash::new(rate, channels);
    let mut bands = BandAnalyzer::new(rate);
    let mut frames_total: u64 = 0;
    let mut chunks = 0u32;

    while let Some(samples) = source.next_samples() {
        chunks += 1;
        if chunks.is_multiple_of(CANCEL_CHECK_EVERY) && cancelled() {
            return Err(AudioError::Cancelled);
        }
        pcm.update(samples);
        for frame in samples.chunks_exact(channels) {
            bands.push(frame.iter().sum::<f32>() / channels as f32);
        }
        frames_total += (samples.len() / channels) as u64;
    }
    if frames_total == 0 {
        return Err(AudioError::Unsupported);
    }

    let duration = frames_total as f64 / rate as f64;
    Ok(AudioAnalysis {
        duration_sec: duration.round() as u32,
        pcm_hash: pcm.finish(),
        fingerprint: fingerprint(&bands.frames, duration),
    })
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
        for &s in samples {
            self.mix((s.clamp(-1.0, 1.0) * 8_388_608.0).round() as i64);
        }
    }

    fn update(&mut self, samples: &[f32]) {
        for frame in samples.chunks_exact(self.channels) {
            let audible = frame.iter().any(|s| s.abs() >= PCM_SILENCE);
            if audible {
                self.started = true;
                if !self.pending.is_empty() {
                    let pending = std::mem::take(&mut self.pending);
                    self.mix_samples(&pending);
                }
                self.mix_samples(frame);
            } else if self.started {
                self.pending.extend_from_slice(frame);
                if self.pending.len() > Self::MAX_PENDING {
                    let pending = std::mem::take(&mut self.pending);
                    self.mix_samples(&pending);
                }
            }
        }
    }

    fn finish(&self) -> String {
        format!("{:016x}", self.hash)
    }
}

/// Per-frame loudness and band energies of the mono signal at [`ANALYSIS_RATE`].
struct BandAnalyzer {
    rate: u32,
    /// Box-filter decimation state.
    phase: u32,
    sum: f32,
    count: u32,
    /// The last `FFT_SIZE` samples; a frame is analyzed every `HOP` new ones.
    ring: Vec<f32>,
    filled: usize,
    since_frame: usize,
    fft: Fft,
    window: Vec<f32>,
    sub_band_bins: Vec<(usize, usize)>,
    frames: Vec<Frame>,
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    rms: f32,
    bands: [f32; BANDS],
}

impl BandAnalyzer {
    fn new(rate: u32) -> Self {
        let window = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / FFT_SIZE as f32).cos())
            .collect();
        let hz_per_bin = ANALYSIS_RATE as f32 / FFT_SIZE as f32;
        let edge = |i: usize| LOW_HZ * (HIGH_HZ / LOW_HZ).powf(i as f32 / SUB_BANDS as f32);
        let sub_band_bins = (0..SUB_BANDS)
            .map(|b| {
                let lo = (edge(b) / hz_per_bin).round() as usize;
                (
                    lo,
                    ((edge(b + 1) / hz_per_bin).round() as usize).max(lo + 1),
                )
            })
            .collect();
        Self {
            rate,
            phase: 0,
            sum: 0.0,
            count: 0,
            ring: Vec::with_capacity(FFT_SIZE),
            filled: 0,
            since_frame: 0,
            fft: Fft::new(FFT_SIZE),
            window,
            sub_band_bins,
            frames: Vec::new(),
        }
    }

    /// One mono sample at the source rate.
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

    fn push_resampled(&mut self, v: f32) {
        if self.ring.len() < FFT_SIZE {
            self.ring.push(v);
        } else {
            self.ring[self.filled % FFT_SIZE] = v;
        }
        self.filled += 1;
        if self.filled < FFT_SIZE {
            return;
        }
        self.since_frame += 1;
        if self.filled == FFT_SIZE || self.since_frame == HOP {
            self.since_frame = 0;
            self.analyze_frame();
        }
    }

    fn analyze_frame(&mut self) {
        // Oldest sample first.
        let start = self.filled % FFT_SIZE;
        let sample = |i: usize| self.ring[(start + i) % FFT_SIZE];
        let rms =
            ((0..FFT_SIZE).map(|i| sample(i) * sample(i)).sum::<f32>() / FFT_SIZE as f32).sqrt();
        let (mut re, mut im) = (vec![0.0f32; FFT_SIZE], vec![0.0f32; FFT_SIZE]);
        for (i, v) in re.iter_mut().enumerate() {
            *v = sample(i) * self.window[i];
        }
        self.fft.run(&mut re, &mut im);
        let mut bands = [0.0f32; BANDS];
        let per_band = SUB_BANDS / BANDS;
        for (s, &(lo, hi)) in self.sub_band_bins.iter().enumerate() {
            let e = (lo..hi).map(|k| re[k] * re[k] + im[k] * im[k]).sum::<f32>() / (hi - lo) as f32;
            bands[s / per_band] += e / per_band as f32;
        }
        self.frames.push(Frame { rms, bands });
    }
}

/// Radix-2 FFT with precomputed twiddles.
struct Fft {
    cos: Vec<f32>,
    sin: Vec<f32>,
}

impl Fft {
    fn new(n: usize) -> Self {
        let angle = |k: usize| -2.0 * PI * k as f32 / n as f32;
        Self {
            cos: (0..n / 2).map(|k| angle(k).cos()).collect(),
            sin: (0..n / 2).map(|k| angle(k).sin()).collect(),
        }
    }

    /// In place; `re.len()` must be the size given to `new`.
    fn run(&self, re: &mut [f32], im: &mut [f32]) {
        let n = re.len();
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
        let mut len = 2;
        while len <= n {
            let step = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let (c, s) = (self.cos[k * step], self.sin[k * step]);
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
}

/// `None` when there is too little audible audio to describe.
fn fingerprint(frames: &[Frame], duration: f64) -> Option<String> {
    let loudest = frames.iter().map(|f| f.rms).fold(0.0f32, f32::max);
    let threshold = loudest * 10f32.powf(AUDIBLE_DB / 20.0);
    let first = frames.iter().position(|f| f.rms > threshold)?;
    let last = frames.iter().rposition(|f| f.rms > threshold)?;
    let frames_per_sec = ANALYSIS_RATE as f32 / HOP as f32;
    let span = last - first + 1;
    if (span as f32) < MIN_AUDIBLE_SEC * frames_per_sec {
        return None;
    }
    let n = frames.len();
    let window = ((WINDOW_SEC * frames_per_sec) as usize).min(n);
    let hashes: Vec<String> = WINDOW_AT
        .iter()
        .map(|&at| {
            let center = first + (span as f32 * at) as usize;
            let start = center.saturating_sub(window / 2).min(n - window);
            format!("{:02x}", window_bits(&frames[start..start + window]))
        })
        .collect();
    Some(format!("{}s_{}", duration.round() as u32, hashes.join("_")))
}

/// Signs of the change of the spectral slope between neighbouring slices (Haitsma–Kalker style,
/// on long slices): (SLICES-1)*(BANDS-1) bits.
fn window_bits(frames: &[Frame]) -> u32 {
    // Mean of the log energies (geometric mean): a few loud frames don't dominate a slice.
    let mut energy = [[0.0f32; BANDS]; SLICES];
    for (t, slice) in split_even(frames, SLICES).enumerate() {
        for (b, e) in energy[t].iter_mut().enumerate() {
            *e = slice.iter().map(|f| (f.bands[b] + 1e-9).ln()).sum::<f32>()
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
    fn fft_finds_a_tone() {
        let n = 64;
        let mut re: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 5.0 * i as f32 / n as f32).cos())
            .collect();
        let mut im = vec![0.0; n];
        Fft::new(n).run(&mut re, &mut im);
        let peak = (0..n / 2)
            .max_by(|&a, &b| re[a].hypot(im[a]).total_cmp(&re[b].hypot(im[b])))
            .unwrap();
        assert_eq!(peak, 5);
    }

    #[test]
    fn silence_has_no_fingerprint() {
        let frames = vec![
            Frame {
                rms: 0.0,
                bands: [0.0; BANDS]
            };
            5000
        ];
        assert_eq!(fingerprint(&frames, 120.0), None);
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
