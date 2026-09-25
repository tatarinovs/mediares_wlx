//! Audio-only playback, pure Rust: `core::audio_decode` (symphonia) on a decoder thread feeds a
//! rodio source on the default output device (WASAPI via cpal). Media Foundation is not involved.
//!
//! The decoder thread pushes chunks into a bounded channel; the source, running on the audio
//! callback thread, only takes ready chunks and never waits for disk I/O. Seeks bump an epoch:
//! chunks decoded before the seek are recognised by their old epoch and dropped.

use std::num::NonZero;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::Arc;

use mediares_core::audio_decode::AudioDecoder;
use rodio::{ChannelCount, DeviceSinkBuilder, MixerDeviceSink, SampleRate, Source};

use crate::transport_bar::Transport;

/// Decoded chunks buffered ahead of the output (a packet is ~20–100 ms of audio).
const CHUNKS_AHEAD: usize = 32;

/// Player-wide controls, read by the source on the audio thread.
struct Levels {
    paused: AtomicBool,
    muted: AtomicBool,
    /// `f32` bits.
    volume: AtomicU32,
}

/// State of one opened file, shared between the UI, the decoder thread and the source.
struct TrackState {
    /// Incremented by every seek.
    epoch: AtomicU64,
    /// Seconds, `f64` bits.
    position: AtomicU64,
    ended: AtomicBool,
    /// Set when the track is replaced or the player dropped: the source leaves the mixer.
    stopped: AtomicBool,
}

impl TrackState {
    fn set_position(&self, seconds: f64) {
        self.position.store(seconds.to_bits(), Ordering::Relaxed);
    }
}

enum Command {
    Seek { seconds: f64, epoch: u64 },
}

struct Chunk {
    epoch: u64,
    start: f64,
    samples: Vec<f32>,
    /// End of stream marker (no samples).
    end: bool,
}

struct Track {
    state: Arc<TrackState>,
    commands: Sender<Command>,
    duration: f64,
}

impl Drop for Track {
    fn drop(&mut self) {
        // The source leaves the mixer on its next sample; dropping `commands` ends the decoder.
        self.state.stopped.store(true, Ordering::Relaxed);
    }
}

/// Field order matters: the track stops before the output device closes.
pub struct AudioPlayer {
    track: Option<Track>,
    levels: Arc<Levels>,
    sink: MixerDeviceSink,
}

impl AudioPlayer {
    /// Opens the default output device; `None` if there is none.
    pub fn new() -> Option<Self> {
        let mut sink = DeviceSinkBuilder::open_default_sink().ok()?;
        sink.log_on_drop(false);
        let levels = Arc::new(Levels {
            paused: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
        });
        Some(Self { track: None, levels, sink })
    }

    /// Replaces the current track with `path` and starts playing it. False if the file can't be
    /// decoded (the previous track keeps playing then).
    pub fn open(&mut self, path: &Path) -> bool {
        let Some(decoder) = AudioDecoder::open(path) else { return false };
        let (Some(channels), Some(rate)) = (NonZero::new(decoder.channels), NonZero::new(decoder.sample_rate)) else {
            return false;
        };
        let duration = decoder.duration.unwrap_or(0.0);
        let state = Arc::new(TrackState {
            epoch: AtomicU64::new(0),
            position: AtomicU64::new(0f64.to_bits()),
            ended: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        let (commands, command_rx) = mpsc::channel();
        let (chunk_tx, chunks) = mpsc::sync_channel(CHUNKS_AHEAD);
        let spawned = std::thread::Builder::new()
            .name("mediares-audio-decoder".into())
            .spawn(move || decode_loop(decoder, command_rx, chunk_tx));
        if spawned.is_err() {
            return false;
        }

        self.track = Some(Track { state: state.clone(), commands, duration });
        self.levels.paused.store(false, Ordering::Relaxed);
        self.sink.mixer().add(TrackSource {
            levels: self.levels.clone(),
            state,
            chunks,
            channels,
            rate,
            buf: Vec::new(),
            pos: 0,
            buf_epoch: 0,
            buf_start: 0.0,
            frames_played: 0,
            phase: 0,
            silent: true,
            gain: 1.0,
        });
        true
    }

    /// The track played to its end (and nobody pressed play since).
    pub fn is_ended(&self) -> bool {
        self.track.as_ref().is_some_and(|t| t.state.ended.load(Ordering::Relaxed))
    }
}

impl Transport for AudioPlayer {
    fn is_playing(&self) -> bool {
        self.track.is_some() && !self.levels.paused.load(Ordering::Relaxed) && !self.is_ended()
    }

    fn play(&self) {
        if self.is_ended() {
            self.seek(0.0, false);
        }
        self.levels.paused.store(false, Ordering::Relaxed);
    }

    fn pause(&self) {
        self.levels.paused.store(true, Ordering::Relaxed);
    }

    fn position(&self) -> f64 {
        self.track.as_ref().map_or(0.0, |t| f64::from_bits(t.state.position.load(Ordering::Relaxed)))
    }

    fn duration(&self) -> f64 {
        self.track.as_ref().map_or(0.0, |t| t.duration)
    }

    fn seek(&self, seconds: f64, _approximate: bool) {
        let Some(track) = &self.track else { return };
        let seconds = if track.duration > 0.0 { seconds.clamp(0.0, track.duration) } else { seconds.max(0.0) };
        let epoch = track.state.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        track.state.set_position(seconds);
        track.state.ended.store(false, Ordering::Relaxed);
        let _ = track.commands.send(Command::Seek { seconds, epoch });
    }

    fn volume(&self) -> f64 {
        f32::from_bits(self.levels.volume.load(Ordering::Relaxed)) as f64
    }

    fn set_volume(&self, volume: f64) {
        self.levels.volume.store((volume.clamp(0.0, 1.0) as f32).to_bits(), Ordering::Relaxed);
    }

    fn is_muted(&self) -> bool {
        self.levels.muted.load(Ordering::Relaxed)
    }

    fn set_muted(&self, muted: bool) {
        self.levels.muted.store(muted, Ordering::Relaxed);
    }
}

fn decode_loop(mut decoder: AudioDecoder, commands: Receiver<Command>, chunks: SyncSender<Chunk>) {
    let mut epoch = 0;
    let mut finished = false;
    loop {
        // At the end of the stream there is nothing to do until a seek (or until the track closes).
        let command = if finished {
            match commands.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        } else {
            match commands.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        };
        if let Some(Command::Seek { mut seconds, epoch: mut latest }) = command {
            // Scrubbing queues many seeks: only the last one matters.
            while let Ok(Command::Seek { seconds: s, epoch: e }) = commands.try_recv() {
                (seconds, latest) = (s, e);
            }
            epoch = latest;
            decoder.seek(seconds);
            finished = false;
        }

        let chunk = match decoder.next_chunk() {
            Some(c) => Chunk { epoch, start: c.start, samples: c.samples.to_vec(), end: false },
            None => {
                finished = true;
                Chunk { epoch, start: 0.0, samples: Vec::new(), end: true }
            }
        };
        // Blocks while the buffer is full; the source drains stale chunks even when paused, so a
        // pending seek is never stuck behind them.
        if chunks.send(chunk).is_err() {
            return;
        }
    }
}

/// The rodio source of one track. It never ends by itself (paused / finished = silence), so
/// playback can resume after the end; it leaves the mixer once the track is stopped.
struct TrackSource {
    levels: Arc<Levels>,
    state: Arc<TrackState>,
    chunks: Receiver<Chunk>,
    channels: ChannelCount,
    rate: SampleRate,
    buf: Vec<f32>,
    pos: usize,
    buf_epoch: u64,
    buf_start: f64,
    frames_played: u64,
    /// Index of the next sample within its frame: decisions are made on frame boundaries only,
    /// so channels never get shifted.
    phase: u16,
    /// The current frame is silence rather than data.
    silent: bool,
    gain: f32,
}

impl TrackSource {
    /// Decides whether the next frame comes from decoded data (true) or is silence.
    fn next_frame_has_data(&mut self) -> bool {
        let epoch = self.state.epoch.load(Ordering::Acquire);
        if self.buf_epoch != epoch {
            self.buf.clear();
            self.pos = 0;
        }
        let paused = self.levels.paused.load(Ordering::Relaxed);
        if self.pos >= self.buf.len() {
            loop {
                match self.chunks.try_recv() {
                    Ok(chunk) if chunk.epoch != epoch => continue,
                    Ok(chunk) if chunk.end => {
                        self.buf_epoch = epoch;
                        self.state.ended.store(true, Ordering::Relaxed);
                        return false;
                    }
                    Ok(chunk) => {
                        self.buf = chunk.samples;
                        self.pos = 0;
                        self.buf_epoch = chunk.epoch;
                        self.buf_start = chunk.start;
                        self.frames_played = 0;
                        break;
                    }
                    // Not decoded yet (just after a seek), or the decoder has gone.
                    Err(_) => return false,
                }
            }
        }
        if paused || self.buf.is_empty() {
            return false;
        }
        self.frames_played += 1;
        self.state.set_position(self.buf_start + self.frames_played as f64 / self.rate.get() as f64);
        true
    }
}

impl Iterator for TrackSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.phase == 0 {
            if self.state.stopped.load(Ordering::Relaxed) {
                return None;
            }
            self.silent = !self.next_frame_has_data();
            self.gain = if self.levels.muted.load(Ordering::Relaxed) {
                0.0
            } else {
                f32::from_bits(self.levels.volume.load(Ordering::Relaxed))
            };
        }
        self.phase = (self.phase + 1) % self.channels.get();
        if self.silent {
            return Some(0.0);
        }
        let sample = self.buf.get(self.pos).copied().unwrap_or(0.0);
        self.pos += 1;
        Some(sample * self.gain)
    }
}

impl Source for TrackSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.rate
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}
