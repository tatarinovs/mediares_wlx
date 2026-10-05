//! Transport bar: previous / play-pause / next, time, seekable timeline, mute and volume.
//!
//! Layout, GDI painting and hit-testing work over a plain [`BarState`]; [`BarControl`] turns
//! mouse input into calls on any [`Transport`] (the video engine or the audio player).

use std::sync::Mutex;
use std::time::Duration;

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    SelectObject, DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    HDC, HFONT,
};

use crate::gdi::{self, contains, fill, polygon};

const BAR_HEIGHT: f32 = 40.0;
pub const BG: u32 = 0x00202020;
const TRACK: u32 = 0x00505050;
const FILL: u32 = 0x00E0A040; // COLORREF is BGR: a light blue
const ICON: u32 = 0x00E8E8E8;
const TEXT: u32 = 0x00D0D0D0;
const ERROR_TEXT: u32 = 0x006060FF;
/// Up / Down step for audio, which has no key frames.
pub const FINE_STEP_SEC: f64 = 1.0;
pub const SEEK_END_MARGIN_SEC: f64 = 1.0;
/// Key auto-repeat of the arrow-key step is cut down to one step per this interval.
pub const SEEK_REPEAT_INTERVAL: Duration = Duration::from_millis(80);
const VOLUME_STEP: f64 = 0.05;

/// Volume and mute survive switching between files and between audio and video; the volume
/// also survives restarting TC (mute doesn't, so a new session never starts silent).
struct AudioLevel {
    volume: f64,
    muted: bool,
    /// Volume changed since it was last written to the INI.
    dirty: bool,
}

/// `None` until the saved volume is first needed.
static AUDIO_LEVEL: Mutex<Option<AudioLevel>> = Mutex::new(None);

fn with_audio_level<R>(f: impl FnOnce(&mut AudioLevel) -> R) -> R {
    let mut level = AUDIO_LEVEL.lock().unwrap_or_else(|e| e.into_inner());
    f(level.get_or_insert_with(|| AudioLevel {
        volume: crate::config::load_volume(),
        muted: false,
        dirty: false,
    }))
}

/// Writes the volume to the INI if it changed; called when the viewer window closes.
pub fn save_audio_level() {
    let volume = with_audio_level(|l| std::mem::take(&mut l.dirty).then_some(l.volume));
    if let Some(volume) = volume {
        crate::config::save_volume(volume);
    }
}

/// Playback controls shared by the video engine and the audio player.
pub trait Transport {
    fn is_playing(&self) -> bool;
    /// Starts or resumes playback (from the start once the end was reached).
    fn play(&self);
    fn pause(&self);
    /// Position in seconds.
    fn position(&self) -> f64;
    /// Duration in seconds (0 while unknown).
    fn duration(&self) -> f64;
    /// `approximate` seeks may snap to a nearby key frame — fast enough for scrubbing.
    fn seek(&self, seconds: f64, approximate: bool);
    fn volume(&self) -> f64;
    fn set_volume(&self, volume: f64);
    fn is_muted(&self) -> bool;
    fn set_muted(&self, muted: bool);

    fn toggle_play(&self) {
        if self.is_playing() {
            self.pause()
        } else {
            self.play()
        }
    }
}

/// Applies the remembered volume / mute to a freshly created player.
pub fn restore_audio_level(t: &dyn Transport) {
    let (volume, muted) = with_audio_level(|l| (l.volume, l.muted));
    t.set_volume(volume);
    t.set_muted(muted);
}

fn remember_audio_level(t: &dyn Transport) {
    let (volume, muted) = (t.volume(), t.is_muted());
    with_audio_level(|l| {
        l.dirty |= l.volume != volume;
        l.volume = volume;
        l.muted = muted;
    });
}

pub fn set_volume(t: &dyn Transport, volume: f64) {
    t.set_volume(volume.clamp(0.0, 1.0));
    if volume > 0.0 {
        t.set_muted(false);
    }
    remember_audio_level(t);
}

pub fn change_volume(t: &dyn Transport, up: bool) {
    set_volume(t, t.volume() + if up { VOLUME_STEP } else { -VOLUME_STEP });
}

pub fn toggle_mute(t: &dyn Transport) {
    t.set_muted(!t.is_muted());
    remember_audio_level(t);
}

/// A seek target within the stream; `duration` 0 (unknown, e.g. a file without an index) leaves
/// it unbounded rather than sending every seek to the start.
pub fn clamp_to_duration(seconds: f64, duration: f64) -> f64 {
    if duration > 0.0 {
        seconds.clamp(0.0, duration)
    } else {
        seconds.max(0.0)
    }
}

/// `base` ± `step` s, kept within the stream (`duration` 0 = unknown). Stepping forward stops
/// [`SEEK_END_MARGIN_SEC`] short of the end: reaching it would end the file and let the play queue
/// move on to the next one while the key is still held.
pub fn step_target(base: f64, duration: f64, forward: bool, step: f64) -> f64 {
    if !forward {
        return (base - step).max(0.0);
    }
    let target = base + step;
    if duration > 0.0 {
        // Never backwards when already inside the margin.
        target.min((duration - SEEK_END_MARGIN_SEC).max(base))
    } else {
        target
    }
}

/// ±`step` s. For a player whose position follows a seek at once (audio; video has its own).
pub fn seek_by(t: &dyn Transport, forward: bool, step: f64) {
    let base = t.position();
    let target = step_target(base, t.duration(), forward, step);
    // Already at the start / end: seeking there again would only restart the seek.
    if target != base {
        t.seek(target, false);
    }
}

pub fn bar_state(t: &dyn Transport, known_duration: f64) -> BarState {
    BarState {
        playing: t.is_playing(),
        position: t.position(),
        duration: t.duration().max(known_duration),
        volume: t.volume(),
        muted: t.is_muted(),
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct BarState {
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    pub muted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub bar: RECT,
    /// Empty when there is nothing to skip to.
    pub prev: RECT,
    pub play: RECT,
    pub next: RECT,
    pub time: RECT,
    pub timeline: RECT,
    pub speaker: RECT,
    pub volume: RECT,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Prev,
    Play,
    Next,
    /// Fraction of the duration under the cursor (0..=1).
    Timeline(f64),
    Speaker,
    /// Volume under the cursor (0..=1).
    Volume(f64),
    Bar,
    Outside,
}

pub fn height(dpi_scale: f32) -> i32 {
    (BAR_HEIGHT * dpi_scale).round() as i32
}

/// Bar along the bottom of a `width` x `height` client area; `skip` adds previous/next buttons.
pub fn layout(width: i32, height_px: i32, dpi_scale: f32, skip: bool) -> Layout {
    let s = |v: f32| (v * dpi_scale).round() as i32;
    let bar_h = height(dpi_scale);
    let top = height_px - bar_h;
    let bar = RECT {
        left: 0,
        top,
        right: width,
        bottom: height_px,
    };
    let row = |left: i32, right: i32| RECT {
        left,
        top,
        right,
        bottom: height_px,
    };

    let button = |left: i32, on: bool| {
        if on {
            row(left, left + bar_h)
        } else {
            row(left, left)
        }
    };
    let prev = button(s(4.0), skip);
    let play = button(prev.right, true);
    let next = button(play.right, skip);
    let time = row(next.right + s(4.0), next.right + s(4.0) + s(120.0));
    let volume_right = (width - s(12.0)).max(time.right);
    // In a narrow window the timeline keeps its room and the volume slider goes (mute stays).
    let fixed = volume_right - time.right - s(8.0 + 12.0 + 8.0) - bar_h;
    let volume_w = if fixed - s(80.0) >= s(120.0) {
        s(80.0)
    } else {
        0
    };
    let volume = row((volume_right - volume_w).max(time.right), volume_right);
    let speaker = row(volume.left - s(8.0) - bar_h, volume.left - s(8.0));
    let timeline = row(
        time.right + s(8.0),
        (speaker.left - s(12.0)).max(time.right + s(8.0)),
    );
    Layout {
        bar,
        prev,
        play,
        next,
        time,
        timeline,
        speaker,
        volume,
    }
}

fn fraction(r: &RECT, x: i32) -> f64 {
    let w = (r.right - r.left).max(1) as f64;
    ((x - r.left) as f64 / w).clamp(0.0, 1.0)
}

pub fn hit(l: &Layout, x: i32, y: i32) -> Hit {
    if !contains(&l.bar, x, y) {
        Hit::Outside
    } else if contains(&l.prev, x, y) {
        Hit::Prev
    } else if contains(&l.play, x, y) {
        Hit::Play
    } else if contains(&l.next, x, y) {
        Hit::Next
    } else if contains(&l.timeline, x, y) {
        Hit::Timeline(fraction(&l.timeline, x))
    } else if contains(&l.speaker, x, y) {
        Hit::Speaker
    } else if contains(&l.volume, x, y) {
        Hit::Volume(fraction(&l.volume, x))
    } else {
        Hit::Bar
    }
}

/// Position along a slider for a drag that may leave its rectangle.
pub fn timeline_fraction(l: &Layout, x: i32) -> f64 {
    fraction(&l.timeline, x)
}

pub fn volume_fraction(l: &Layout, x: i32) -> f64 {
    fraction(&l.volume, x)
}

enum BarDrag {
    Timeline { was_playing: bool },
    Volume,
}

/// What a click in the viewer meant for the player.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Click {
    /// Not on the bar (the picture / cover): callers toggle playback.
    Outside,
    /// Handled by the bar.
    Bar,
    /// Previous (`false`) / next (`true`) button: the caller switches files.
    Skip(bool),
}

/// Mouse handling of the bar: button clicks and slider dragging.
#[derive(Default)]
pub struct BarControl {
    drag: Option<BarDrag>,
}

impl BarControl {
    pub fn mouse_down(
        &mut self,
        t: &dyn Transport,
        l: &Layout,
        known_duration: f64,
        x: i32,
        y: i32,
    ) -> Click {
        match hit(l, x, y) {
            Hit::Outside => return Click::Outside,
            Hit::Prev => return Click::Skip(false),
            Hit::Next => return Click::Skip(true),
            Hit::Play => t.toggle_play(),
            Hit::Timeline(f) => {
                let was_playing = t.is_playing();
                t.pause();
                t.seek(f * t.duration().max(known_duration), true);
                self.drag = Some(BarDrag::Timeline { was_playing });
            }
            Hit::Speaker => toggle_mute(t),
            Hit::Volume(v) => {
                set_volume(t, v);
                self.drag = Some(BarDrag::Volume);
            }
            Hit::Bar => {}
        }
        Click::Bar
    }

    /// True while a slider is being dragged (the caller keeps mouse capture).
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Returns whether anything changed.
    pub fn mouse_move(
        &mut self,
        t: &dyn Transport,
        l: &Layout,
        known_duration: f64,
        x: i32,
    ) -> bool {
        match self.drag {
            Some(BarDrag::Timeline { .. }) => t.seek(
                timeline_fraction(l, x) * t.duration().max(known_duration),
                true,
            ),
            Some(BarDrag::Volume) => set_volume(t, volume_fraction(l, x)),
            None => return false,
        }
        true
    }

    pub fn mouse_up(&mut self, t: &dyn Transport, l: &Layout, known_duration: f64, x: i32) {
        if let Some(BarDrag::Timeline { was_playing }) = self.drag.take() {
            // Final precise seek, then resume if it was playing before scrubbing.
            t.seek(
                timeline_fraction(l, x) * t.duration().max(known_duration),
                false,
            );
            if was_playing {
                t.play();
            }
        }
    }

    pub fn cancel(&mut self) {
        self.drag = None;
    }
}

pub fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

/// Thin horizontal track through the middle of `r`, filled up to `value` (0..=1).
unsafe fn slider(dc: HDC, r: RECT, value: f64, thickness: i32) {
    let mid = (r.top + r.bottom) / 2;
    let track = RECT {
        left: r.left,
        top: mid - thickness / 2,
        right: r.right,
        bottom: mid - thickness / 2 + thickness,
    };
    fill(dc, track, TRACK);
    let filled = r.left + ((r.right - r.left) as f64 * value.clamp(0.0, 1.0)).round() as i32;
    fill(
        dc,
        RECT {
            right: filled,
            ..track
        },
        FILL,
    );
    let knob = thickness * 2;
    fill(
        dc,
        RECT {
            left: filled - knob / 2,
            top: mid - knob / 2,
            right: filled + knob / 2,
            bottom: mid + knob / 2,
        },
        ICON,
    );
}

unsafe fn text(dc: HDC, r: RECT, s: &str, color: u32, flags: DRAW_TEXT_FORMAT) {
    gdi::text(dc, r, s, None, color, flags | DT_SINGLELINE | DT_VCENTER);
}

/// Play triangle, or pause bars while `playing`, centred in `r`.
pub unsafe fn play_icon(dc: HDC, r: RECT, playing: bool, dpi_scale: f32) {
    let s = |v: f32| (v * dpi_scale).round() as i32;
    let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    let h = s(8.0);
    if playing {
        let (w, gap) = (s(4.0), s(3.0));
        for left in [cx - gap - w, cx + gap] {
            let bar = RECT {
                left,
                top: cy - h,
                right: left + w,
                bottom: cy + h,
            };
            fill(dc, bar, ICON);
        }
    } else {
        let points = [
            POINT {
                x: cx - h * 2 / 3,
                y: cy - h,
            },
            POINT {
                x: cx - h * 2 / 3,
                y: cy + h,
            },
            POINT { x: cx + h, y: cy },
        ];
        polygon(dc, &points, ICON);
    }
}

/// Previous / next: a triangle pointing away from the play button, ending in a bar.
pub unsafe fn skip_icon(dc: HDC, r: RECT, forward: bool, dpi_scale: f32) {
    let s = |v: f32| (v * dpi_scale).round() as i32;
    let dir = if forward { 1 } else { -1 };
    let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    let (h, w) = (s(6.0), s(7.0));
    let tip = cx + dir * w / 2;
    let base = cx - dir * w / 2;
    let points = [
        POINT { x: base, y: cy - h },
        POINT { x: base, y: cy + h },
        POINT { x: tip, y: cy },
    ];
    polygon(dc, &points, ICON);
    let bar_x = if forward { tip } else { tip - s(2.0) };
    let bar = RECT {
        left: bar_x,
        top: cy - h,
        right: bar_x + s(2.0),
        bottom: cy + h,
    };
    fill(dc, bar, ICON);
}

/// A text shown instead of the timeline.
pub enum Message<'a> {
    Error(&'a str),
    Info(&'a str),
}

/// Paints the bar. `message` (a playback error, "frame saved") replaces the timeline when present.
pub unsafe fn paint(
    dc: HDC,
    l: &Layout,
    state: &BarState,
    font: HFONT,
    message: Option<Message<'_>>,
    dpi_scale: f32,
) {
    let s = |v: f32| (v * dpi_scale).round() as i32;
    fill(dc, l.bar, BG);
    let old_font = SelectObject(dc, font.into());

    play_icon(dc, l.play, state.playing, dpi_scale);
    for (r, forward) in [(l.prev, false), (l.next, true)] {
        if r.right > r.left {
            skip_icon(dc, r, forward, dpi_scale);
        }
    }

    let time = format!(
        "{} / {}",
        format_time(state.position),
        format_time(state.duration)
    );
    text(dc, l.time, &time, TEXT, DT_LEFT);

    match message {
        Some(Message::Error(msg)) => {
            text(dc, l.timeline, msg, ERROR_TEXT, DT_LEFT | DT_END_ELLIPSIS)
        }
        Some(Message::Info(msg)) => text(dc, l.timeline, msg, TEXT, DT_LEFT | DT_END_ELLIPSIS),
        None => {
            let progress = if state.duration > 0.0 {
                state.position / state.duration
            } else {
                0.0
            };
            slider(dc, l.timeline, progress, s(4.0));
        }
    }

    // Speaker: box + cone; a red bar when muted.
    let (sx, sy) = (
        (l.speaker.left + l.speaker.right) / 2,
        (l.speaker.top + l.speaker.bottom) / 2,
    );
    let u = s(3.0);
    polygon(
        dc,
        &[
            POINT {
                x: sx - 3 * u,
                y: sy - u,
            },
            POINT {
                x: sx - u,
                y: sy - u,
            },
            POINT {
                x: sx + u,
                y: sy - 3 * u,
            },
            POINT {
                x: sx + u,
                y: sy + 3 * u,
            },
            POINT {
                x: sx - u,
                y: sy + u,
            },
            POINT {
                x: sx - 3 * u,
                y: sy + u,
            },
        ],
        ICON,
    );
    if state.muted {
        text(
            dc,
            RECT {
                left: sx + u,
                ..l.speaker
            },
            "×",
            ERROR_TEXT,
            DT_CENTER,
        );
    }
    if l.volume.right > l.volume.left {
        slider(
            dc,
            l.volume,
            if state.muted { 0.0 } else { state.volume },
            s(3.0),
        );
    }

    SelectObject(dc, old_font);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_format() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(75.9), "1:15");
        assert_eq!(format_time(3725.0), "1:02:05");
        assert_eq!(format_time(f64::NAN), "0:00");
    }

    #[test]
    fn hit_testing() {
        let l = layout(1000, 600, 1.0, false);
        assert_eq!(l.bar.top, 560);
        assert_eq!(hit(&l, l.play.left - 1, 580), Hit::Bar);
        assert_eq!(hit(&l, 10, 100), Hit::Outside);
        assert_eq!(hit(&l, (l.play.left + l.play.right) / 2, 580), Hit::Play);
        match hit(&l, l.timeline.left, 580) {
            Hit::Timeline(f) => assert!(f < 0.01),
            other => panic!("{:?}", other),
        }
        match hit(&l, l.volume.right - 1, 580) {
            Hit::Volume(v) => assert!(v > 0.95),
            other => panic!("{:?}", other),
        }
        assert!(l.timeline.right <= l.speaker.left);

        let l = layout(1000, 600, 1.0, true);
        assert_eq!(hit(&l, l.prev.left + 1, 580), Hit::Prev);
        assert_eq!(hit(&l, l.next.right - 1, 580), Hit::Next);
        assert!(l.play.left >= l.prev.right && l.time.left >= l.next.right);
    }

    #[test]
    fn narrow_window_does_not_invert_rects() {
        let l = layout(150, 100, 1.0, true);
        assert!(l.timeline.right >= l.timeline.left);
        assert!(l.volume.right >= l.volume.left);
    }

    #[test]
    fn step_target_bounds() {
        assert_eq!(step_target(10.0, 100.0, true, 5.0), 15.0);
        assert_eq!(step_target(10.0, 100.0, false, 5.0), 5.0);
        assert_eq!(step_target(2.0, 100.0, false, 5.0), 0.0);
        assert_eq!(step_target(97.0, 100.0, true, 5.0), 99.0);
        assert_eq!(step_target(99.5, 100.0, true, 5.0), 99.5);
        assert_eq!(step_target(99.5, 100.0, false, 5.0), 94.5);
        assert_eq!(step_target(10.0, 0.0, true, 5.0), 15.0);
        assert_eq!(clamp_to_duration(120.0, 100.0), 100.0);
        assert_eq!(clamp_to_duration(120.0, 0.0), 120.0);
        assert_eq!(clamp_to_duration(-1.0, 0.0), 0.0);
    }
}
