//! Video positions in `%LOCALAPPDATA%\mediares\mediares.sqlite3`, separate from disposable
//! content analysis. No import of the old text history.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::config;

const FILE_NAME: &str = "mediares.sqlite3";
const MAX_ENTRIES: usize = 200;
pub const DEFAULT_MIN_DURATION_SEC: f64 = 300.0;
const EDGE_SEC: f64 = 30.0;

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_millis(50))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS resume (
            path BLOB PRIMARY KEY NOT NULL,
            position REAL NOT NULL CHECK(position >= 0)
         );",
    )?;
    Ok(db)
}

/// Normalize separators and case as for viewer navigation. Keep Windows UTF-16 losslessly
/// even for an unusual name containing an unpaired surrogate.
fn key(video: &Path) -> Vec<u8> {
    let normalized: PathBuf = video.components().collect();
    let lowercase = normalized.to_str().map(str::to_lowercase);
    lowercase
        .as_deref()
        .map(std::ffi::OsStr::new)
        .unwrap_or(normalized.as_os_str())
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

pub fn load(video: &Path) -> Option<f64> {
    let db = open(&config::local_path(FILE_NAME)).ok()?;
    load_from(&db, video).ok().flatten()
}

fn load_from(db: &Connection, video: &Path) -> rusqlite::Result<Option<f64>> {
    db.query_row(
        "SELECT position FROM resume WHERE path=?1",
        [key(video)],
        |r| r.get(0),
    )
    .optional()
    .map(|position: Option<f64>| position.filter(|p| p.is_finite() && *p >= 0.0))
}

/// One transaction protects updates from other viewer windows and TC processes. Positions
/// near either edge are forgotten; short videos do not create history entries.
pub fn store(video: &Path, position: f64, duration: f64, min_duration: f64) {
    if !video.is_absolute() || !duration.is_finite() || duration < min_duration {
        return;
    }
    let keep = (EDGE_SEC..duration - EDGE_SEC)
        .contains(&position)
        .then_some(position);
    if let Ok(mut db) = open(&config::local_path(FILE_NAME)) {
        let _ = store_in(&mut db, video, keep);
    }
}

fn store_in(db: &mut Connection, video: &Path, position: Option<f64>) -> rusqlite::Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let path = key(video);
    if let Some(position) = position.filter(|p| p.is_finite() && *p >= 0.0) {
        tx.execute(
            "INSERT OR REPLACE INTO resume(path,position) VALUES (?1,?2)",
            (&path, position),
        )?;
        tx.execute(
            "DELETE FROM resume WHERE rowid IN
             (SELECT rowid FROM resume ORDER BY rowid DESC LIMIT -1 OFFSET ?1)",
            [MAX_ENTRIES as i64],
        )?;
    } else {
        tx.execute("DELETE FROM resume WHERE path=?1", [&path])?;
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_updates_case_insensitively_and_trims_oldest() {
        let mut db = open(Path::new(":memory:")).unwrap();
        let a = Path::new(r"D:\Видео\A.mp4");
        store_in(&mut db, a, Some(100.0)).unwrap();
        let other_case = Path::new("d:/видео/a.MP4");
        assert_eq!(load_from(&db, other_case).unwrap(), Some(100.0));
        store_in(&mut db, other_case, Some(150.0)).unwrap();
        assert_eq!(load_from(&db, a).unwrap(), Some(150.0));
        for i in 0..MAX_ENTRIES {
            store_in(&mut db, Path::new(&format!(r"D:\{i}.mp4")), Some(50.0)).unwrap();
        }
        assert_eq!(load_from(&db, a).unwrap(), None);
        let last = Path::new(r"D:\199.mp4");
        assert_eq!(load_from(&db, last).unwrap(), Some(50.0));
        store_in(&mut db, last, None).unwrap();
        assert_eq!(load_from(&db, last).unwrap(), None);
    }

    #[test]
    fn history_survives_reopening_and_serializes_writers() {
        let file =
            std::env::temp_dir().join(format!("mediares-resume-{}.sqlite3", std::process::id()));
        let video = Path::new(r"D:\Видео\🦊.mp4");
        {
            let mut first = open(&file).unwrap();
            let mut second = open(&file).unwrap();
            let lock = first
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert!(store_in(&mut second, video, Some(75.0)).is_err());
            lock.commit().unwrap();
            store_in(&mut second, video, Some(75.0)).unwrap();
        }
        {
            let db = open(&file).unwrap();
            assert_eq!(load_from(&db, video).unwrap(), Some(75.0));
        }
        std::fs::remove_file(file).unwrap();
    }
}
