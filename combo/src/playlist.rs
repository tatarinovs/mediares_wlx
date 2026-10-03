//! Play queue on top of the viewer's file list (the folder, or an M3U playlist): what plays next
//! when a track ends, previous/next track, repeat and shuffle.
//!
//! The plugin's own switching (auto-advance, the bar's ⏮/⏭) moves within this list; TC's own
//! navigation arrives through `ListLoadNext` and simply picks a file in it (or rescans).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mediares_core::ffi::ansi_to_os_string;
use mediares_core::probe::probe_file;

use crate::i18n::tr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    /// Start the list over after the last file.
    All,
    /// Loop the current file.
    One,
}

impl Repeat {
    pub const ALL: [Repeat; 3] = [Repeat::Off, Repeat::All, Repeat::One];

    pub fn from_index(i: i32) -> Self {
        Self::ALL.get(i as usize).copied().unwrap_or_default()
    }

    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn label(self) -> &'static str {
        match self {
            Repeat::Off => tr("No repeat"),
            Repeat::All => tr("Repeat list"),
            Repeat::One => tr("Repeat file"),
        }
    }
}

/// Playback options of the queue (stored in the config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueOptions {
    /// Go on to the next audio/video file when one ends.
    pub auto_advance: bool,
    pub repeat: Repeat,
    pub shuffle: bool,
}

impl Default for QueueOptions {
    fn default() -> Self {
        Self {
            auto_advance: true,
            repeat: Repeat::Off,
            shuffle: false,
        }
    }
}

/// What to do when the current file played to its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndAction {
    Replay,
    Play(usize),
    Stop,
}

fn is_playable(path: &Path) -> bool {
    probe_file(path).is_playable()
}

/// Nearest playable file after (`forward`) or before `current`, wrapping around if `wrap`.
/// Photos in between are skipped.
pub fn step(files: &[PathBuf], current: usize, forward: bool, wrap: bool) -> Option<usize> {
    let n = files.len() as isize;
    let dir = if forward { 1 } else { -1 };
    (1..n)
        .map(|k| current as isize + dir * k)
        .filter_map(|i| {
            if (0..n).contains(&i) || wrap {
                Some(i.rem_euclid(n) as usize)
            } else {
                None
            }
        })
        .find(|&i| is_playable(&files[i]))
}

/// A random playable file other than `current`.
pub fn random(files: &[PathBuf], current: usize) -> Option<usize> {
    let candidates: Vec<usize> = (0..files.len())
        .filter(|&i| i != current && is_playable(&files[i]))
        .collect();
    (!candidates.is_empty()).then(|| candidates[next_random() as usize % candidates.len()])
}

/// Previous / next track for the bar buttons and media keys (always wraps).
pub fn skip(
    files: &[PathBuf],
    current: usize,
    forward: bool,
    options: &QueueOptions,
) -> Option<usize> {
    if forward && options.shuffle {
        return random(files, current);
    }
    step(files, current, forward, true)
}

pub fn on_end(files: &[PathBuf], current: usize, options: &QueueOptions) -> EndAction {
    if options.repeat == Repeat::One {
        return EndAction::Replay;
    }
    if !options.auto_advance {
        return EndAction::Stop;
    }
    let next = if options.shuffle {
        random(files, current)
    } else {
        step(files, current, true, options.repeat == Repeat::All)
    };
    match next {
        Some(i) => EndAction::Play(i),
        // A single file with "repeat list" loops it.
        None if options.repeat == Repeat::All && is_playable(&files[current]) => EndAction::Replay,
        None => EndAction::Stop,
    }
}

/// xorshift64*, seeded from the clock; plenty for shuffling.
fn next_random() -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        x = nanos | 1;
    }
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    STATE.store(x, Ordering::Relaxed);
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Reads an M3U / M3U8 playlist: entries that exist as files, relative ones resolved against
/// the playlist's folder. `#` lines (EXTINF etc.) and URLs are skipped.
pub fn read_m3u(path: &Path) -> Vec<PathBuf> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let base = path.parent().unwrap_or(Path::new(""));
    parse_m3u(&bytes, base)
        .into_iter()
        .filter(|p| p.is_file())
        .collect()
}

fn parse_m3u(bytes: &[u8], base: &Path) -> Vec<PathBuf> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    // M3U8 is UTF-8; plain M3U is usually the ANSI code page, but UTF-8 is common too.
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => ansi_to_os_string(bytes)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.contains("://"))
        .map(|line| {
            let entry = Path::new(line.strip_prefix("file:///").unwrap_or(line));
            if entry.is_absolute() || entry.has_root() {
                entry.to_path_buf()
            } else {
                base.join(entry)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn step_skips_photos_and_respects_wrap() {
        let list = files(&["a.mp3", "b.jpg", "c.mp4", "d.png"]);
        assert_eq!(step(&list, 0, true, false), Some(2));
        assert_eq!(step(&list, 2, true, false), None);
        assert_eq!(step(&list, 2, true, true), Some(0));
        assert_eq!(step(&list, 2, false, false), Some(0));
        assert_eq!(step(&list, 0, false, false), None);
        assert_eq!(step(&list, 0, false, true), Some(2));
    }

    #[test]
    fn end_of_track_actions() {
        let list = files(&["a.mp3", "b.flac"]);
        let mut o = QueueOptions::default();
        assert_eq!(on_end(&list, 0, &o), EndAction::Play(1));
        assert_eq!(on_end(&list, 1, &o), EndAction::Stop);
        o.repeat = Repeat::All;
        assert_eq!(on_end(&list, 1, &o), EndAction::Play(0));
        assert_eq!(on_end(&files(&["only.mp3"]), 0, &o), EndAction::Replay);
        o.repeat = Repeat::One;
        assert_eq!(on_end(&list, 0, &o), EndAction::Replay);
        o = QueueOptions {
            auto_advance: false,
            ..Default::default()
        };
        assert_eq!(on_end(&list, 0, &o), EndAction::Stop);
        o = QueueOptions {
            shuffle: true,
            ..Default::default()
        };
        assert_eq!(on_end(&list, 0, &o), EndAction::Play(1));
    }

    #[test]
    fn m3u_parsing() {
        let text = "\u{FEFF}#EXTM3U\r\n#EXTINF:123,Artist - Song\r\nmusic\\a.mp3\r\n\r\nC:\\abs\\b.flac\r\nhttp://radio/stream\r\n";
        let base = Path::new(r"D:\lists");
        assert_eq!(
            parse_m3u(text.as_bytes(), base),
            [
                PathBuf::from(r"D:\lists\music\a.mp3"),
                PathBuf::from(r"C:\abs\b.flac")
            ]
        );
    }
}
