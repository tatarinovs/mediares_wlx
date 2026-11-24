//! Play queue on top of the viewer's file list (the folder, or an M3U playlist): what plays next
//! when a track ends, previous/next track, repeat and shuffle.
//!
//! The plugin's own switching (auto-advance, the bar's ⏮/⏭) moves within this list; TC's own
//! navigation arrives through `ListLoadNext` and simply picks a file in it (or rescans).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mediares_core::ffi::ansi_to_os_string;
use mediares_core::probe::MediaGroup;

use crate::i18n::tr;
use crate::shortcut::MediaEntry;

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

fn is_playable_in(entry: &MediaEntry, group: Option<MediaGroup>) -> bool {
    entry.kind.is_playable() && group.is_none_or(|g| entry.group() == Some(g))
}

/// Nearest playable file after (`forward`) or before `current`, wrapping around if `wrap`.
/// Photos in between are skipped; when `group` is `Some`, only that group is considered.
pub fn step(
    files: &[MediaEntry],
    current: usize,
    forward: bool,
    wrap: bool,
    group: Option<MediaGroup>,
) -> Option<usize> {
    following(files, current, forward, wrap, group).next()
}

fn following(
    files: &[MediaEntry],
    current: usize,
    forward: bool,
    wrap: bool,
    group: Option<MediaGroup>,
) -> impl Iterator<Item = usize> + '_ {
    let n = files.len() as isize;
    let dir = if forward { 1 } else { -1 };
    (1..n)
        .map(move |k| current as isize + dir * k)
        .filter_map(move |i| {
            if (0..n).contains(&i) || wrap {
                Some(i.rem_euclid(n) as usize)
            } else {
                None
            }
        })
        .filter(move |&i| is_playable_in(&files[i], group))
}

/// Try each successor at most once; opening, rather than a directory probe, decides availability.
pub fn candidates(
    files: &[MediaEntry],
    current: usize,
    forward: bool,
    wrap: bool,
    shuffle: bool,
    group: Option<MediaGroup>,
) -> Vec<usize> {
    let mut indices: Vec<_> = following(files, current, forward, wrap, group).collect();
    if shuffle {
        for i in (1..indices.len()).rev() {
            indices.swap(i, next_random() as usize % (i + 1));
        }
    }
    indices
}

/// A random playable file other than `current`.
pub fn random(files: &[MediaEntry], current: usize, group: Option<MediaGroup>) -> Option<usize> {
    let candidates: Vec<usize> = (0..files.len())
        .filter(|&i| i != current && is_playable_in(&files[i], group))
        .collect();
    (!candidates.is_empty()).then(|| candidates[next_random() as usize % candidates.len()])
}

pub fn on_end(
    files: &[MediaEntry],
    current: usize,
    options: &QueueOptions,
    group: Option<MediaGroup>,
) -> EndAction {
    if options.repeat == Repeat::One {
        return EndAction::Replay;
    }
    if !options.auto_advance {
        return EndAction::Stop;
    }
    let next = if options.shuffle {
        random(files, current, group)
    } else {
        step(files, current, true, options.repeat == Repeat::All, group)
    };
    match next {
        Some(i) => EndAction::Play(i),
        // A single file with "repeat list" loops it.
        None if options.repeat == Repeat::All
            && files.get(current).is_some_and(|f| is_playable_in(f, group)) =>
        {
            EndAction::Replay
        }
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

/// Reads an M3U / M3U8 playlist without probing every entry: relative paths resolved against
/// the playlist's folder, `file:///` URLs included. `#` lines (EXTINF etc.) and other URLs are
/// skipped.
fn read_m3u(path: &Path) -> Vec<PathBuf> {
    use std::io::Read;
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    let mut bytes = Vec::new();
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    if file.take(MAX_BYTES + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_BYTES {
        return Vec::new();
    }
    let base = path.parent().unwrap_or(Path::new(""));
    parse_m3u(&bytes, base)
}

/// Entries that the viewer can open automatically. A playlist may refer to files on
/// its own network share, but it cannot cause a connection to a different one.
pub fn media_entries(path: &Path) -> Vec<MediaEntry> {
    let mut allowed = crate::shortcut::same_origin_filter(path);
    read_m3u(path)
        .into_iter()
        .filter_map(|p| MediaEntry::resolve_allowed(p, &mut allowed))
        .filter(|entry| entry.kind.is_image_kind() || entry.kind.is_playable())
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
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            // file:///D:/x.mp3 is a local path, file://server/share/x.mp3 a network one.
            let line = match line.get(..7) {
                Some(scheme) if scheme.eq_ignore_ascii_case("file://") => {
                    let rest = &line[7..];
                    percent_decoded(
                        &rest
                            .strip_prefix('/')
                            .map_or(format!("//{rest}"), str::to_owned),
                    )
                }
                _ => line.to_string(),
            };
            if line.contains("://") {
                return None;
            }
            let entry = Path::new(&line);
            Some(if entry.is_absolute() {
                entry.to_path_buf()
            } else {
                base.join(entry)
            })
        })
        .collect()
}

/// A URL path with its `%XX` escapes decoded (as UTF-8); a malformed escape stays as written.
fn percent_decoded(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .filter(|_| bytes[i] == b'%')
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<MediaEntry> {
        names
            .iter()
            .filter_map(|p| MediaEntry::resolve(PathBuf::from(p)))
            .collect()
    }

    #[test]
    fn playlist_keeps_unavailable_entries_without_probing_them_and_bounds_input() {
        let path =
            std::env::temp_dir().join(format!("mediares-playlist-{}.m3u8", std::process::id()));
        let remote = r"\\unavailable-mediares-test\share\song.mp3";
        std::fs::write(&path, remote).unwrap();
        assert_eq!(read_m3u(&path), vec![PathBuf::from(remote)]);
        std::fs::File::create(&path)
            .unwrap()
            .set_len(16 * 1024 * 1024 + 1)
            .unwrap();
        assert!(read_m3u(&path).is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn step_skips_photos_and_respects_wrap() {
        let list = files(&["a.mp3", "b.jpg", "c.mp4", "d.png"]);
        assert_eq!(step(&list, 0, true, false, None), Some(2));
        assert_eq!(step(&list, 2, true, false, None), None);
        assert_eq!(step(&list, 2, true, true, None), Some(0));
        assert_eq!(step(&list, 2, false, false, None), Some(0));
        assert_eq!(step(&list, 0, false, false, None), None);
        assert_eq!(step(&list, 0, false, true, None), Some(2));
    }

    #[test]
    fn retry_candidates_preserve_direction_and_never_repeat_even_when_shuffled() {
        let list = files(&["a.mp3", "photo.jpg", "missing.wav", "b.mp3"]);
        assert_eq!(candidates(&list, 0, true, false, false, None), [2, 3]);
        assert_eq!(candidates(&list, 0, false, true, false, None), [3, 2]);
        assert!(candidates(&list, 3, true, false, false, None).is_empty());
        let mut shuffled = candidates(&list, 0, true, true, true, None);
        shuffled.sort_unstable();
        assert_eq!(shuffled, [2, 3]);
    }

    #[test]
    fn end_of_track_actions() {
        let list = files(&["a.mp3", "b.flac"]);
        let mut o = QueueOptions::default();
        assert_eq!(on_end(&list, 0, &o, None), EndAction::Play(1));
        assert_eq!(on_end(&list, 1, &o, None), EndAction::Stop);
        o.repeat = Repeat::All;
        assert_eq!(on_end(&list, 1, &o, None), EndAction::Play(0));
        assert_eq!(
            on_end(&files(&["only.mp3"]), 0, &o, None),
            EndAction::Replay
        );
        o.repeat = Repeat::One;
        assert_eq!(on_end(&list, 0, &o, None), EndAction::Replay);
        o = QueueOptions {
            auto_advance: false,
            ..Default::default()
        };
        assert_eq!(on_end(&list, 0, &o, None), EndAction::Stop);
        o = QueueOptions {
            shuffle: true,
            ..Default::default()
        };
        assert_eq!(on_end(&list, 0, &o, None), EndAction::Play(1));
    }

    #[test]
    fn group_filtering_separates_audio_and_video() {
        let list = files(&["a.mp3", "b.jpg", "c.mp4", "d.flac", "e.mkv"]);
        assert_eq!(
            step(&list, 0, true, false, Some(MediaGroup::Audio)),
            Some(3)
        );
        assert_eq!(
            candidates(&list, 0, true, false, false, Some(MediaGroup::Audio)),
            [3]
        );
        assert_eq!(
            candidates(&list, 2, true, true, false, Some(MediaGroup::Video)),
            [4]
        );
        assert_eq!(random(&list, 0, Some(MediaGroup::Audio)), Some(3));
        assert_eq!(random(&list, 2, Some(MediaGroup::Video)), Some(4));

        let single_audio = files(&["only.mp3", "clip.mp4"]);
        let mut o = QueueOptions {
            repeat: Repeat::All,
            ..Default::default()
        };
        assert_eq!(
            on_end(&single_audio, 0, &o, Some(MediaGroup::Audio)),
            EndAction::Replay
        );
        o.repeat = Repeat::Off;
        assert_eq!(
            on_end(&single_audio, 0, &o, Some(MediaGroup::Audio)),
            EndAction::Stop
        );
    }

    #[test]
    fn local_playlist_filters_external_network_entries() {
        let path =
            std::env::temp_dir().join(format!("mediares_playlist_{}.m3u", std::process::id()));
        std::fs::write(
            &path,
            r#"local.wav
\\external.invalid\share\track.wav
file://external.invalid/share/other.wav
"#,
        )
        .unwrap();
        let entries = media_entries(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target, path.with_file_name("local.wav"));
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
        // VLC and foobar2000 write file URLs.
        let urls = "FILE:///D:/%D0%9C%D1%83%D0%B7%D1%8B%D0%BA%D0%B0/a%20b.mp3\nfile:///C:/100%25.flac\nfile://nas/music/c.ogg\n";
        assert_eq!(
            parse_m3u(urls.as_bytes(), base),
            [
                PathBuf::from("D:/Музыка/a b.mp3"),
                PathBuf::from("C:/100%.flac"),
                PathBuf::from("//nas/music/c.ogg")
            ]
        );
        assert!(Path::new("//nas/music/c.ogg").has_root());
        assert_eq!(percent_decoded("a%2"), "a%2");
    }
}
