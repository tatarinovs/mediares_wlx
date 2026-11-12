//! Disposable local SQLite cache. Only expensive analysis and sharpness use it, on a RAM miss.
//! No worker thread outlives the DLL. Locks are never waited for and decoding runs outside them.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{named_params, Connection, Row};

use crate::cache::{AudioAnalysis, CachedMedia, FileKey};
use crate::hashing::{compute_aspect_ratio, ImageAnalysis};
use crate::video_frame::VideoAnalysis;

// Bump when either the stored representation or any cached analysis algorithm changes.
const VERSION: i32 = 1;
const MAX_BYTES: i64 = 128 * 1024 * 1024;
const MAX_ENTRIES: i64 = 100_000;
const PRUNE_BATCH: i64 = 8192;

enum State {
    Unopened,
    Open(Store),
    Disabled,
}

static CACHE: Mutex<State> = Mutex::new(State::Unopened);

fn cache_path() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("MEDIARES_CONTENT_CACHE") {
        if value.is_empty() || value == "0" || value.eq_ignore_ascii_case("off") {
            return None;
        }
        let path = PathBuf::from(value);
        return path.is_absolute().then_some(path);
    }
    let path = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("mediares")
        .join("content-cache-v1.sqlite3");
    path.is_absolute().then_some(path)
}

fn with_store<R>(f: impl FnOnce(&mut Store) -> rusqlite::Result<R>) -> Option<R> {
    // A busy SQLite connection is a cache miss, including another call in this DLL.
    let mut state = CACHE.try_lock().ok()?;
    if matches!(*state, State::Unopened) {
        *state = cache_path()
            .and_then(|path| Store::open(&path).ok())
            .map_or(State::Disabled, State::Open);
    }
    match &mut *state {
        State::Open(store) => f(store).ok(),
        _ => None,
    }
}

pub(crate) fn read<T: DiskValue>(key: &FileKey) -> Option<T> {
    let key = Key::new(key)?;
    with_store(|store| store.read(&key))
}

pub(crate) fn write<T: DiskValue>(key: &FileKey, value: &T) {
    // Unsupported/cancelled analyses must not survive a restart: codecs and permissions change.
    let Some(columns) = value.columns() else {
        return;
    };
    let Some(key) = Key::new(key) else { return };
    with_store(|store| store.write(&key, T::SLOT, columns));
}

pub(crate) fn close() {
    if let Ok(mut state) = CACHE.lock() {
        *state = State::Unopened;
    }
}

struct Key {
    path: Vec<u8>,
    size: i64,
    modified: [u8; 16],
}

impl Key {
    fn new(key: &FileKey) -> Option<Self> {
        // TC passes absolute paths. Refuse relative ones rather than mix different working dirs.
        if !key.path.is_absolute() {
            return None;
        }
        Some(Self {
            // Preserve Windows Unicode paths without lossy UTF-8 conversion or collation rules.
            path: key
                .path
                .as_os_str()
                .encode_wide()
                .flat_map(u16::to_le_bytes)
                .collect(),
            size: key.size.try_into().ok()?,
            modified: key
                .modified?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos()
                .to_le_bytes(),
        })
    }
}

struct Store {
    db: Connection,
    writes: u32,
}

impl Store {
    fn open(path: &Path) -> rusqlite::Result<Self> {
        // No fallback to the DLL or media directory: those may be read-only or on a NAS.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|_| rusqlite::Error::InvalidPath(path.into()))?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::ZERO)?;
        let version: i32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != 0 && version != VERSION {
            return Err(rusqlite::Error::InvalidQuery);
        }
        // Bound the main database and SQLite's RAM. WAL is checkpointed every ~1 MiB and
        // truncated after checkpoints; only short transactions are used by this plugin.
        let page_size: i64 = db.pragma_query_value(None, "page_size", |row| row.get(0))?;
        let pages: i64 =
            db.pragma_update_and_check(None, "max_page_count", MAX_BYTES / page_size, |r| {
                r.get(0)
            })?;
        if pages > MAX_BYTES / page_size {
            return Err(rusqlite::Error::InvalidQuery);
        }
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA cache_size=-1024;
             PRAGMA mmap_size=0;
             PRAGMA wal_autocheckpoint=256;
             PRAGMA journal_size_limit=1048576;",
        )?;
        if version == 0 {
            db.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS entries (
                    path BLOB NOT NULL, slot INTEGER NOT NULL,
                    size INTEGER NOT NULL, modified BLOB NOT NULL, version INTEGER NOT NULL,
                    kind INTEGER NOT NULL, width INTEGER, height INTEGER, duration INTEGER,
                    hash_a INTEGER, hash_b INTEGER, coarse INTEGER,
                    fingerprint TEXT, pcm_hash TEXT, sharpness INTEGER,
                    PRIMARY KEY(path, slot)
                 );
                 PRAGMA user_version=1;
                 COMMIT;",
            )?;
        }
        let store = Self { db, writes: 0 };
        store.prune(MAX_ENTRIES)?;
        Ok(store)
    }

    fn read<T: DiskValue>(&self, key: &Key) -> rusqlite::Result<T> {
        self.db.query_row(
            "SELECT kind, width, height, duration, hash_a, hash_b, coarse,
                    fingerprint, pcm_hash, sharpness FROM entries
             WHERE path=?1 AND slot=?2 AND size=?3 AND modified=?4 AND version=?5",
            (&key.path, T::SLOT, key.size, &key.modified[..], VERSION),
            T::from_row,
        )
    }

    fn write(&mut self, key: &Key, slot: i32, c: Columns<'_>) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO entries
             (path, slot, size, modified, version, kind, width, height, duration,
              hash_a, hash_b, coarse, fingerprint, pcm_hash, sharpness)
             VALUES (:path, :slot, :size, :modified, :version, :kind, :width, :height,
                     :duration, :hash_a, :hash_b, :coarse, :fingerprint, :pcm_hash, :sharpness)",
            named_params! {
                ":path": &key.path, ":slot": slot, ":size": key.size,
                ":modified": &key.modified[..], ":version": VERSION, ":kind": c.kind,
                ":width": c.width, ":height": c.height, ":duration": c.duration,
                ":hash_a": c.hash_a, ":hash_b": c.hash_b, ":coarse": c.coarse,
                ":fingerprint": c.fingerprint, ":pcm_hash": c.pcm_hash, ":sharpness": c.sharpness,
            },
        )?;
        self.writes = self.writes.wrapping_add(1);
        if self.writes.is_multiple_of(256) {
            self.prune(MAX_ENTRIES)?;
        }
        Ok(())
    }

    fn prune(&self, limit: i64) -> rusqlite::Result<()> {
        let count: i64 = self
            .db
            .query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?;
        if count > limit {
            // FIFO, without a disk write on every hit. Deleted pages are reused, not VACUUMed.
            self.db.execute(
                "DELETE FROM entries WHERE rowid IN
                 (SELECT rowid FROM entries ORDER BY rowid LIMIT ?1)",
                [count.saturating_sub(limit).max(PRUNE_BATCH)],
            )?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Columns<'a> {
    kind: i32,
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<u32>,
    // Signed SQLite integers preserve all 64 hash bits through the casts below.
    hash_a: Option<i64>,
    hash_b: Option<i64>,
    coarse: Option<u32>,
    fingerprint: Option<&'a str>,
    pcm_hash: Option<&'a str>,
    sharpness: Option<u32>,
}

pub(crate) trait DiskValue: Sized {
    const SLOT: i32;
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self>;
    fn columns(&self) -> Option<Columns<'_>>;
}

impl DiskValue for CachedMedia {
    const SLOT: i32 = 0;

    fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(match r.get::<_, i32>("kind")? {
            1 => {
                let width = r.get("width")?;
                let height = r.get("height")?;
                Self::Image(Arc::new(ImageAnalysis {
                    width,
                    height,
                    aspect_ratio: compute_aspect_ratio(width, height),
                    dhash: r.get::<_, i64>("hash_a")? as u64,
                    phash: r.get::<_, i64>("hash_b")? as u64,
                    coarse_hash: r.get("coarse")?,
                }))
            }
            2 => Self::Video(Arc::new(VideoAnalysis {
                width: r.get("width")?,
                height: r.get("height")?,
                duration_sec: r.get("duration")?,
                dhash_mid: r.get::<_, Option<i64>>("hash_a")?.map(|v| v as u64),
                fingerprint: r.get("fingerprint")?,
            })),
            3 => Self::Audio(Arc::new(AudioAnalysis {
                duration_sec: r.get("duration")?,
                pcm_hash: r.get("pcm_hash")?,
                fingerprint: r.get("fingerprint")?,
            })),
            _ => return Err(rusqlite::Error::InvalidQuery),
        })
    }

    fn columns(&self) -> Option<Columns<'_>> {
        Some(match self {
            Self::Image(i) => Columns {
                kind: 1,
                width: Some(i.width),
                height: Some(i.height),
                hash_a: Some(i.dhash as i64),
                hash_b: Some(i.phash as i64),
                coarse: Some(i.coarse_hash),
                ..Columns::default()
            },
            Self::Video(v) if v.dhash_mid.is_some() && v.fingerprint.is_some() => Columns {
                kind: 2,
                width: Some(v.width),
                height: Some(v.height),
                duration: Some(v.duration_sec),
                hash_a: v.dhash_mid.map(|h| h as i64),
                fingerprint: v.fingerprint.as_deref(),
                ..Columns::default()
            },
            Self::Audio(a) => Columns {
                kind: 3,
                duration: Some(a.duration_sec),
                pcm_hash: Some(&a.pcm_hash),
                fingerprint: a.fingerprint.as_deref(),
                ..Columns::default()
            },
            // A decoder failure can leave valid dimensions/duration but missing hashes.
            // Keep that in RAM only, so another session can retry decoding the video.
            Self::Video(_) | Self::Unsupported => return None,
        })
    }
}

impl DiskValue for u32 {
    const SLOT: i32 = 1;

    fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        r.get("sharpness")
    }

    fn columns(&self) -> Option<Columns<'_>> {
        Some(Columns {
            kind: 4,
            sharpness: Some(*self),
            ..Columns::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Instant, SystemTime};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir =
                std::env::temp_dir().join(format!("mediares-disk-{}-{stamp}", std::process::id()));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("фото 🦊.jpg"), b"fixture").unwrap();
            Self(dir)
        }

        fn path(&self) -> PathBuf {
            self.0.join("cache.sqlite3")
        }
        fn key(&self) -> Key {
            Key::new(&FileKey::stat(&self.0.join("фото 🦊.jpg")).unwrap()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn put<T: DiskValue>(store: &mut Store, key: &Key, value: &T) {
        store.write(key, T::SLOT, value.columns().unwrap()).unwrap();
    }

    #[test]
    fn results_survive_reopening_with_all_hash_bits_and_nullable_fields() {
        let fixture = Fixture::new();
        let key = fixture.key();
        let values = [
            CachedMedia::Image(Arc::new(ImageAnalysis {
                width: 6000,
                height: 4000,
                aspect_ratio: "3:2".into(),
                dhash: u64::MAX,
                phash: 1 << 63,
                coarse_hash: u32::MAX,
            })),
            CachedMedia::Video(Arc::new(VideoAnalysis {
                width: 1920,
                height: 1080,
                duration_sec: 3600,
                dhash_mid: Some(u64::MAX),
                fingerprint: Some("video-fingerprint".into()),
            })),
            CachedMedia::Audio(Arc::new(AudioAnalysis {
                duration_sec: 180,
                pcm_hash: "pcm-hash".into(),
                fingerprint: Some("audio-fp".into()),
            })),
            CachedMedia::Audio(Arc::new(AudioAnalysis {
                duration_sec: 1,
                pcm_hash: "silence".into(),
                fingerprint: None,
            })),
        ];
        for expected in values {
            let mut store = Store::open(&fixture.path()).unwrap();
            put(&mut store, &key, &expected);
            put(&mut store, &key, &u32::MAX);
            drop(store);
            let reopened = Store::open(&fixture.path()).unwrap();
            let actual: CachedMedia = reopened.read(&key).unwrap();
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
            assert_eq!(reopened.read::<u32>(&key).unwrap(), u32::MAX);
        }
        assert!(CachedMedia::Unsupported.columns().is_none());
        let partial_video = CachedMedia::Video(Arc::new(VideoAnalysis {
            width: 640,
            height: 480,
            duration_sec: 10,
            dhash_mid: None,
            fingerprint: None,
        }));
        assert!(partial_video.columns().is_none());
    }

    #[test]
    fn file_edits_and_algorithm_changes_miss_and_replace_the_old_record() {
        let fixture = Fixture::new();
        let mut key = fixture.key();
        let mut store = Store::open(&fixture.path()).unwrap();
        put(&mut store, &key, &42u32);
        key.size += 1;
        assert!(store.read::<u32>(&key).is_err());
        key.size -= 1;
        key.modified[0] ^= 1;
        assert!(store.read::<u32>(&key).is_err());
        put(&mut store, &key, &43u32);
        assert_eq!(store.read::<u32>(&key).unwrap(), 43);
        let count: i64 = store
            .db
            .query_row("SELECT count(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        store
            .db
            .execute("UPDATE entries SET version=99", [])
            .unwrap();
        assert!(store.read::<u32>(&key).is_err());
        put(&mut store, &key, &44u32);
        assert_eq!(store.read::<u32>(&key).unwrap(), 44);
    }

    #[test]
    fn a_busy_writer_does_not_block_reads_or_wait_for_the_lock() {
        let fixture = Fixture::new();
        let key = fixture.key();
        let mut store = Store::open(&fixture.path()).unwrap();
        put(&mut store, &key, &42u32);
        let other = Connection::open(fixture.path()).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let start = Instant::now();
        assert!(store
            .write(&key, u32::SLOT, 43u32.columns().unwrap())
            .is_err());
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(store.read::<u32>(&key).unwrap(), 42);
        other.execute_batch("ROLLBACK").unwrap();
        put(&mut store, &key, &43u32);
        assert_eq!(store.read::<u32>(&key).unwrap(), 43);
    }

    #[test]
    fn corrupt_unknown_and_unwritable_databases_are_left_alone() {
        let fixture = Fixture::new();
        std::fs::write(fixture.path(), b"not a SQLite database").unwrap();
        assert!(Store::open(&fixture.path()).is_err());
        assert_eq!(
            std::fs::read(fixture.path()).unwrap(),
            b"not a SQLite database"
        );
        let unknown = fixture.0.join("future.sqlite3");
        let db = Connection::open(&unknown).unwrap();
        db.pragma_update(None, "user_version", 99).unwrap();
        assert!(Store::open(&unknown).is_err());
        assert_eq!(
            db.pragma_query_value::<i32, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            99
        );
        assert!(Store::open(&fixture.0).is_err());
        assert!(Store::open(&fixture.0.join("фото 🦊.jpg/cache.sqlite3")).is_err());
    }

    #[test]
    fn pruning_keeps_new_records_and_full_database_keeps_existing_values() {
        let fixture = Fixture::new();
        let mut key = fixture.key();
        let mut store = Store::open(&fixture.path()).unwrap();
        store.db.execute_batch("BEGIN").unwrap();
        for i in 0u32..9000 {
            key.path = i.to_le_bytes().to_vec();
            put(&mut store, &key, &i);
        }
        store.db.execute_batch("COMMIT").unwrap();
        store.prune(4).unwrap();
        assert_eq!(store.read::<u32>(&key).unwrap(), 8999);
        let count: i64 = store
            .db
            .query_row("SELECT count(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 4);
        let pages: i64 = store
            .db
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        store
            .db
            .pragma_update_and_check(None, "max_page_count", pages, |r| r.get::<_, i64>(0))
            .unwrap();
        let huge = "x".repeat((pages * 4096) as usize);
        let value = CachedMedia::Audio(Arc::new(AudioAnalysis {
            duration_sec: 1,
            pcm_hash: huge,
            fingerprint: None,
        }));
        assert!(store
            .write(&key, CachedMedia::SLOT, value.columns().unwrap())
            .is_err());
        assert_eq!(store.read::<u32>(&key).unwrap(), 8999);
    }

    #[test]
    fn missing_timestamps_and_relative_paths_are_not_persisted() {
        let fixture = Fixture::new();
        let mut key = FileKey::stat(&fixture.0.join("фото 🦊.jpg")).unwrap();
        key.modified = None;
        assert!(Key::new(&key).is_none());
        key.modified = Some(UNIX_EPOCH);
        key.path = "relative.jpg".into();
        assert!(Key::new(&key).is_none());
    }
}
