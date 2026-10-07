//! Audio-only playback, pure Rust decoding: `core::audio_decode` (symphonia) on a decoder thread
//! feeds a WASAPI shared-mode stream on the default output device. The stream is opened in the
//! track's own format and Windows converts the rate and channels (`AUTOCONVERTPCM`). Media
//! Foundation is not involved.
//!
//! The decoder thread pushes chunks into a bounded channel; the render thread only takes ready
//! chunks and never waits for disk I/O. Seeks bump an epoch: chunks decoded before the seek are
//! recognised by their old epoch and dropped.
//!
//! Gapless: the next file of the queue can be decoded ahead ([`AudioPlayer::preload`]); when the
//! track ends, the render thread goes straight on with it in the same stream, and opening that
//! file then takes over the track already playing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mediares_core::audio_decode::AudioDecoder;
use mediares_core::mf_init::ComScope;
use windows::core::GUID;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::transport_bar::Transport;

/// Decoded chunks buffered ahead of the output (a packet is ~20–100 ms of audio).
const CHUNKS_AHEAD: usize = 32;
/// Device buffer (100 ns units): rides out scheduling hiccups, short enough for a quick pause.
const DEVICE_BUFFER_HNS: i64 = 1_000_000;
/// A lost device (unplugged) is looked for again this often.
const REOPEN_INTERVAL: Duration = Duration::from_millis(500);

/// Player-wide controls, read on the render thread.
struct Levels {
    paused: AtomicBool,
    muted: AtomicBool,
    /// `f32` bits.
    volume: AtomicU32,
}

/// State of one opened file, shared between the UI, the decoder thread and the render thread.
struct TrackState {
    /// Incremented by every seek.
    epoch: AtomicU64,
    /// Seconds, `f64` bits.
    position: AtomicU64,
    ended: AtomicBool,
    /// Set when the track is replaced or the player dropped: the render thread closes its stream.
    stopped: AtomicBool,
    /// ReplayGain volume factor (`f32` bits).
    gain: AtomicU32,
    /// The render thread plays it (a preloaded track only once the one before has ended).
    started: AtomicBool,
}

impl TrackState {
    fn new(started: bool, gain: f32) -> Arc<Self> {
        Arc::new(TrackState {
            epoch: AtomicU64::new(0),
            position: AtomicU64::new(0f64.to_bits()),
            ended: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            gain: AtomicU32::new(gain.to_bits()),
            started: AtomicBool::new(started),
        })
    }

    fn set_position(&self, seconds: f64) {
        self.position.store(seconds.to_bits(), Ordering::Relaxed);
    }
}

/// A track decoded ahead, handed to the render thread to go on with.
struct NextTrack {
    state: Arc<TrackState>,
    chunks: Receiver<Chunk>,
}

type NextSlot = Arc<Mutex<Option<NextTrack>>>;

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
    /// Stream format: a track of another can't follow in the same stream.
    format: (u16, u32),
}

impl Drop for Track {
    fn drop(&mut self) {
        // The render thread stops on its next wake-up; dropping `commands` ends the decoder.
        self.state.stopped.store(true, Ordering::Relaxed);
    }
}

pub struct AudioPlayer {
    track: Option<Track>,
    levels: Arc<Levels>,
    /// Where the render thread looks for the track to go on with.
    next_slot: NextSlot,
    /// The file decoded ahead and its track.
    pending: Option<(PathBuf, Track)>,
}

impl AudioPlayer {
    /// `None` if there is no output device.
    pub fn new() -> Option<Self> {
        let _com = ComScope::new();
        unsafe { default_device() }.ok()?;
        Some(Self {
            track: None,
            levels: Arc::new(Levels {
                paused: AtomicBool::new(false),
                muted: AtomicBool::new(false),
                volume: AtomicU32::new(1.0f32.to_bits()),
            }),
            next_slot: Arc::new(Mutex::new(None)),
            pending: None,
        })
    }

    /// Replaces the current track with `path` and starts playing it. False if the file can't be
    /// decoded or played (the previous track keeps playing then). The file decoded ahead that
    /// already follows on is taken over as it plays.
    pub fn open(&mut self, path: &Path) -> bool {
        if let Some((ahead, track)) = self.pending.take() {
            let playing = track.state.started.load(Ordering::Acquire);
            if playing && crate::state::same_path(&ahead, path) {
                self.track = Some(track);
                self.levels.paused.store(false, Ordering::Relaxed);
                return true;
            }
            // Not taken: the render thread must not go on with it.
            track.state.stopped.store(true, Ordering::Relaxed);
            self.next_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        }
        let Some((track, chunks)) = start_track(path, true, 1.0) else {
            return false;
        };
        let (channels, rate) = track.format;
        let state = track.state.clone();
        self.next_slot = Arc::new(Mutex::new(None));

        let source = TrackSource {
            levels: self.levels.clone(),
            state: state.clone(),
            chunks,
            next: self.next_slot.clone(),
            channels: channels as usize,
            rate,
            buf: Vec::new(),
            pos: 0,
            buf_epoch: 0,
            buf_start: 0.0,
            frames_played: 0,
        };
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("mediares-audio-output".into())
            .spawn(move || output_loop(source, ready_tx));
        // Without an output stream the decoder ends too: its channel is dropped.
        if spawned.is_err() || ready.recv() != Ok(true) {
            state.stopped.store(true, Ordering::Relaxed);
            return false;
        }

        self.track = Some(track);
        self.levels.paused.store(false, Ordering::Relaxed);
        true
    }

    /// Decodes `path` ahead to follow the current track without a gap, at ReplayGain `gain`.
    /// False if it can't (another sample rate or channel count, or not decodable).
    pub fn preload(&mut self, path: &Path, gain: f32) -> bool {
        let Some(current) = &self.track else {
            return false;
        };
        if self
            .pending
            .as_ref()
            .is_some_and(|(ahead, _)| crate::state::same_path(ahead, path))
        {
            return true;
        }
        let Some((track, chunks)) = start_track(path, false, gain) else {
            return false;
        };
        if track.format != current.format {
            track.state.stopped.store(true, Ordering::Relaxed);
            return false;
        }
        if let Some((_, old)) = self.pending.take() {
            old.state.stopped.store(true, Ordering::Relaxed);
        }
        *self.next_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(NextTrack {
            state: track.state.clone(),
            chunks,
        });
        self.pending = Some((path.to_path_buf(), track));
        true
    }

    /// ReplayGain volume factor of the track playing.
    pub fn set_track_gain(&self, gain: f32) {
        if let Some(track) = &self.track {
            track.state.gain.store(gain.to_bits(), Ordering::Relaxed);
        }
    }

    /// The track played to its end (and nobody pressed play since).
    pub fn is_ended(&self) -> bool {
        self.track
            .as_ref()
            .is_some_and(|t| t.state.ended.load(Ordering::Relaxed))
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
        self.track.as_ref().map_or(0.0, |t| {
            f64::from_bits(t.state.position.load(Ordering::Relaxed))
        })
    }

    fn duration(&self) -> f64 {
        self.track.as_ref().map_or(0.0, |t| t.duration)
    }

    fn seek(&self, seconds: f64, _approximate: bool) {
        let Some(track) = &self.track else { return };
        let seconds = crate::transport_bar::clamp_to_duration(seconds, track.duration);
        let epoch = track.state.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        track.state.set_position(seconds);
        track.state.ended.store(false, Ordering::Relaxed);
        let _ = track.commands.send(Command::Seek { seconds, epoch });
    }

    fn volume(&self) -> f64 {
        f32::from_bits(self.levels.volume.load(Ordering::Relaxed)) as f64
    }

    fn set_volume(&self, volume: f64) {
        self.levels
            .volume
            .store((volume.clamp(0.0, 1.0) as f32).to_bits(), Ordering::Relaxed);
    }

    fn is_muted(&self) -> bool {
        self.levels.muted.load(Ordering::Relaxed)
    }

    fn set_muted(&self, muted: bool) {
        self.levels.muted.store(muted, Ordering::Relaxed);
    }
}

/// Opens `path` and starts decoding it: the track and the chunks for the render thread.
fn start_track(path: &Path, started: bool, gain: f32) -> Option<(Track, Receiver<Chunk>)> {
    let decoder = AudioDecoder::open(path)?;
    let (channels, rate) = (decoder.channels, decoder.sample_rate);
    if channels == 0 || rate == 0 {
        return None;
    }
    let duration = decoder.duration.unwrap_or(0.0);
    let state = TrackState::new(started, gain);
    let (commands, command_rx) = mpsc::channel();
    let (chunk_tx, chunks) = mpsc::sync_channel(CHUNKS_AHEAD);
    std::thread::Builder::new()
        .name("mediares-audio-decoder".into())
        .spawn(move || decode_loop(decoder, command_rx, chunk_tx))
        .ok()?;
    let track = Track {
        state,
        commands,
        duration,
        format: (channels, rate),
    };
    Some((track, chunks))
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
        if let Some(Command::Seek {
            mut seconds,
            epoch: mut latest,
        }) = command
        {
            // Scrubbing queues many seeks: only the last one matters.
            while let Ok(Command::Seek {
                seconds: s,
                epoch: e,
            }) = commands.try_recv()
            {
                (seconds, latest) = (s, e);
            }
            epoch = latest;
            // A seek to the very end may fail: decoding would then go on from the old place.
            let near_end = decoder.duration.is_some_and(|d| seconds >= d - 1.0);
            finished = decoder.seek(seconds).is_none() && near_end;
        }

        let chunk = match (!finished).then(|| decoder.next_chunk()).flatten() {
            Some(c) => Chunk {
                epoch,
                start: c.start,
                samples: c.samples.to_vec(),
                end: false,
            },
            None => {
                finished = true;
                Chunk {
                    epoch,
                    start: 0.0,
                    samples: Vec::new(),
                    end: true,
                }
            }
        };
        // Blocks while the buffer is full; the render thread drains stale chunks even when paused,
        // so a pending seek is never stuck behind them.
        if chunks.send(chunk).is_err() {
            return;
        }
    }
}

/// Render thread of one track: keeps the device buffer filled until the track is stopped. The
/// source never ends by itself (paused / finished = silence), so playback can resume after the
/// end. `ready` reports whether the first stream could be opened.
fn output_loop(mut source: TrackSource, ready: SyncSender<bool>) {
    let _com = ComScope::new();
    let (channels, rate) = (source.channels as u16, source.rate);
    let mut stream = unsafe { Stream::open(channels, rate) }.ok();
    let _ = ready.send(stream.is_some());
    while stream.is_some() && !source.state.stopped.load(Ordering::Relaxed) {
        let alive = stream
            .as_ref()
            .is_some_and(|s| unsafe { s.render(&mut source) });
        if !alive {
            // The device went away: silence until the (new) default device can be opened.
            stream = None;
            while stream.is_none() && !source.state.stopped.load(Ordering::Relaxed) {
                std::thread::sleep(REOPEN_INTERVAL);
                stream = unsafe { Stream::open(channels, rate) }.ok();
            }
        }
    }
}

unsafe fn default_device() -> windows::core::Result<IMMDevice> {
    let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    devices.GetDefaultAudioEndpoint(eRender, eConsole)
}

/// An event-driven shared-mode stream of interleaved `f32` samples.
struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    buffer_frames: u32,
    channels: usize,
}

impl Stream {
    unsafe fn open(channels: u16, rate: u32) -> windows::core::Result<Self> {
        let client: IAudioClient = default_device()?.Activate(CLSCTX_ALL, None)?;
        let block_align = channels * 4;
        let format = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xFFFE, // WAVE_FORMAT_EXTENSIBLE
                nChannels: channels,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * block_align as u32,
                nBlockAlign: block_align,
                wBitsPerSample: 32,
                cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: channel_mask(channels),
            // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            SubFormat: GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71),
        };
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let format_ptr = &format as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            DEVICE_BUFFER_HNS,
            0,
            format_ptr,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        let stream = Self {
            render: client.GetService()?,
            buffer_frames: client.GetBufferSize()?,
            client,
            event,
            channels: channels as usize,
        };
        stream.client.SetEventHandle(event)?;
        stream.client.Start()?;
        Ok(stream)
    }

    /// Waits until the device wants data and fills what it can take. False once the device is
    /// gone.
    unsafe fn render(&self, source: &mut TrackSource) -> bool {
        WaitForSingleObject(self.event, 200);
        let Ok(padding) = self.client.GetCurrentPadding() else {
            return false;
        };
        let frames = self.buffer_frames.saturating_sub(padding);
        if frames == 0 {
            return true;
        }
        let Ok(data) = self.render.GetBuffer(frames) else {
            return false;
        };
        // The engine's buffers are aligned for the sample type.
        let samples =
            std::slice::from_raw_parts_mut(data as *mut f32, frames as usize * self.channels);
        source.fill(samples);
        self.render.ReleaseBuffer(frames, 0).is_ok()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

/// Speaker positions of the usual layouts (mono, stereo, 5.1, 7.1); others are left unassigned.
fn channel_mask(channels: u16) -> u32 {
    match channels {
        1 => 0x4,
        2 => 0x3,
        3 => 0x7,
        4 => 0x33,
        6 => 0x3F,
        8 => 0x63F,
        _ => 0,
    }
}

/// Samples of one track for the render thread.
struct TrackSource {
    levels: Arc<Levels>,
    state: Arc<TrackState>,
    chunks: Receiver<Chunk>,
    /// The track to go on with when this one ends.
    next: NextSlot,
    channels: usize,
    rate: u32,
    buf: Vec<f32>,
    pos: usize,
    buf_epoch: u64,
    buf_start: f64,
    frames_played: u64,
}

impl TrackSource {
    /// Fills whole frames of `out` with decoded audio, or silence while paused or waiting.
    fn fill(&mut self, out: &mut [f32]) {
        let volume = if self.levels.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.levels.volume.load(Ordering::Relaxed))
        };
        let mut gain = volume * f32::from_bits(self.state.gain.load(Ordering::Relaxed));
        let mut played = false;
        for frame in out.chunks_exact_mut(self.channels) {
            let before = Arc::as_ptr(&self.state);
            if !self.next_frame_has_data() {
                frame.fill(0.0);
                continue;
            }
            if !std::ptr::eq(before, Arc::as_ptr(&self.state)) {
                // Went on with the next track: its own gain from here.
                gain = volume * f32::from_bits(self.state.gain.load(Ordering::Relaxed));
            }
            played = true;
            for sample in frame {
                *sample = self.buf.get(self.pos).copied().unwrap_or(0.0) * gain;
                self.pos += 1;
            }
            self.frames_played += 1;
        }
        // A seek during this call already set its own position: don't overwrite it with the old one.
        if played && self.buf_epoch == self.state.epoch.load(Ordering::Acquire) {
            self.state
                .set_position(self.buf_start + self.frames_played as f64 / self.rate as f64);
        }
    }

    /// Decides whether the next frame comes from decoded data (true) or is silence.
    fn next_frame_has_data(&mut self) -> bool {
        let mut epoch = self.state.epoch.load(Ordering::Acquire);
        if self.buf_epoch != epoch {
            self.buf.clear();
            self.pos = 0;
        }
        if self.pos >= self.buf.len() {
            loop {
                match self.chunks.try_recv() {
                    Ok(chunk) if chunk.epoch != epoch => continue,
                    Ok(chunk) if chunk.end => {
                        if self.go_on_with_next() {
                            epoch = self.state.epoch.load(Ordering::Acquire);
                            continue;
                        }
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
        !self.levels.paused.load(Ordering::Relaxed) && !self.buf.is_empty()
    }

    /// The track ended: switches to the one decoded ahead, if there is one still wanted.
    fn go_on_with_next(&mut self) -> bool {
        let next = self.next.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(next) = next.filter(|n| !n.state.stopped.load(Ordering::Relaxed)) else {
            return false;
        };
        self.state.ended.store(true, Ordering::Relaxed);
        next.state.started.store(true, Ordering::Release);
        self.state = next.state;
        self.chunks = next.chunks;
        self.buf.clear();
        self.pos = 0;
        self.buf_epoch = 0;
        self.buf_start = 0.0;
        self.frames_played = 0;
        true
    }
}
