//! Where long videos were left off: `mediares_resume.txt` next to `mediares.ini`, one
//! "<seconds>\t<path>" line per video, most recent first.

use std::path::{Path, PathBuf};

use crate::config;

const FILE_NAME: &str = "mediares_resume.txt";
const MAX_ENTRIES: usize = 200;
/// Shorter videos always start from the beginning.
pub const MIN_DURATION_SEC: f64 = 300.0;
/// Stopping this close to the start or the end doesn't count as "left off".
const EDGE_SEC: f64 = 30.0;

type Entry = (f64, PathBuf);

/// The position to continue `video` from, if one was remembered.
pub fn load(video: &Path) -> Option<f64> {
    let text = std::fs::read_to_string(config::data_file(FILE_NAME)).ok()?;
    parse(&text)
        .into_iter()
        .find(|(_, p)| same(p, video))
        .map(|(t, _)| t)
}

/// Remembers (or forgets, near the start or end) where `video` was left.
pub fn store(video: &Path, position: f64, duration: f64) {
    if duration < MIN_DURATION_SEC {
        return;
    }
    let file = config::data_file(FILE_NAME);
    let entries = std::fs::read_to_string(&file)
        .map(|t| parse(&t))
        .unwrap_or_default();
    let keep = (EDGE_SEC..duration - EDGE_SEC)
        .contains(&position)
        .then_some(position);
    let updated = update(entries, video, keep);
    let text: String = updated
        .iter()
        .map(|(t, p)| format!("{:.1}\t{}\n", t, p.display()))
        .collect();
    let _ = std::fs::write(file, text);
}

fn parse(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            let (t, p) = line.split_once('\t')?;
            Some((t.trim().parse().ok()?, PathBuf::from(p)))
        })
        .collect()
}

fn update(mut entries: Vec<Entry>, video: &Path, position: Option<f64>) -> Vec<Entry> {
    entries.retain(|(_, p)| !same(p, video));
    if let Some(t) = position {
        entries.insert(0, (t, video.to_path_buf()));
    }
    entries.truncate(MAX_ENTRIES);
    entries
}

/// NTFS names are case-insensitive, Cyrillic included.
fn same(a: &Path, b: &Path) -> bool {
    crate::state::same_path(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_first_and_replaced() {
        let a = PathBuf::from(r"D:\Видео\A.mp4");
        let b = PathBuf::from(r"D:\Видео\B.mp4");
        let list = update(Vec::new(), &a, Some(100.0));
        let list = update(list, &b, Some(200.0));
        let list = update(list, Path::new(r"d:\видео\a.MP4"), Some(150.0));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0, 150.0);
        assert_eq!(list[1].1, b);
        assert!(update(list, &a, None).iter().all(|(_, p)| !same(p, &a)));
    }

    #[test]
    fn parses_saved_lines() {
        let list = parse("12.5\tC:\\x.mkv\nbroken line\n7\tC:\\y.mp4\n");
        assert_eq!(
            list,
            vec![
                (12.5, PathBuf::from("C:\\x.mkv")),
                (7.0, PathBuf::from("C:\\y.mp4"))
            ]
        );
    }
}
