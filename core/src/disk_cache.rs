//! Disposable local SQLite cache of analysis and metadata, accessed on a RAM miss.
//! No worker thread outlives the DLL. Locks are never waited for and decoding runs outside them.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rusqlite::{named_params, Connection, Row};

use crate::cache::{AudioAnalysis, CachedMedia, FileKey};
use crate::hashing::{compute_aspect_ratio, ImageAnalysis};
use crate::video_frame::VideoAnalysis;

// Schema upgrades preserve existing analysis records; record versions invalidate stale results.
const SCHEMA_VERSION: i32 = 2;
// Bump when a cached analysis algorithm or payload representation changes.
const VERSION: i32 = 1;
const MAX_PAYLOAD: usize = 1024 * 1024;
const MAX_BYTES: i64 = 256 * 1024 * 1024;
const MAX_ENTRIES: i64 = 500_000;
const PRUNE_BATCH: i64 = 8192;
const RETRY_DELAY: Duration = Duration::from_secs(1);

// Unlike a borrowed &str, these names are resolved to known literals during deserialization.
pub(crate) type StaticName = &'static str;

enum State {
    Unopened,
    Open(Store),
    Disabled,
    Retry(Instant),
}

impl State {
    fn open_if_needed(&mut self, path: impl FnOnce() -> Option<PathBuf>, now: Instant) {
        match self {
            Self::Unopened => {}
            Self::Retry(after) if now >= *after => {}
            _ => return,
        }
        *self = match path() {
            None => Self::Disabled,
            Some(path) => match Store::open(&path) {
                Ok(store) => Self::Open(store),
                Err(_) => Self::Retry(now + RETRY_DELAY),
            },
        };
    }
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
    state.open_if_needed(cache_path, Instant::now());
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
        if !(0..=SCHEMA_VERSION).contains(&version) {
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
        if version < SCHEMA_VERSION {
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
                 ALTER TABLE entries ADD COLUMN payload TEXT;
                 PRAGMA user_version=2;
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
                    fingerprint, pcm_hash, sharpness, payload FROM entries
             WHERE path=?1 AND slot=?2 AND size=?3 AND modified=?4 AND version=?5
               AND (payload IS NULL OR length(CAST(payload AS BLOB)) <= ?6)",
            (
                &key.path,
                T::SLOT,
                key.size,
                &key.modified[..],
                VERSION,
                MAX_PAYLOAD as i64,
            ),
            T::from_row,
        )
    }

    fn write(&mut self, key: &Key, slot: i32, c: Columns<'_>) -> rusqlite::Result<()> {
        // Reject unbounded records before eviction (normal analysis strings are tiny).
        if c.payload.as_ref().map_or(0, String::len)
            + c.fingerprint.map_or(0, str::len)
            + c.pcm_hash.map_or(0, str::len)
            > MAX_PAYLOAD
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        if let Err(error) = self.insert(key, slot, &c) {
            if !matches!(&error, rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::DiskFull)
            {
                return Err(error);
            }
            // Also handles a larger replacement or another process filling our reserved room.
            self.make_room()?;
            self.insert(key, slot, &c)?;
        }
        self.writes = self.writes.wrapping_add(1);
        if self.writes.is_multiple_of(256) {
            self.prune(MAX_ENTRIES)?;
            self.make_room()?;
        }
        Ok(())
    }

    fn insert(&self, key: &Key, slot: i32, c: &Columns<'_>) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO entries
             (path, slot, size, modified, version, kind, width, height, duration,
              hash_a, hash_b, coarse, fingerprint, pcm_hash, sharpness, payload)
             VALUES (:path, :slot, :size, :modified, :version, :kind, :width, :height,
                     :duration, :hash_a, :hash_b, :coarse, :fingerprint, :pcm_hash, :sharpness, :payload)",
            named_params! {
                ":path": &key.path, ":slot": slot, ":size": key.size,
                ":modified": &key.modified[..], ":version": VERSION, ":kind": c.kind,
                ":width": c.width, ":height": c.height, ":duration": c.duration,
                ":hash_a": c.hash_a, ":hash_b": c.hash_b, ":coarse": c.coarse,
                ":fingerprint": c.fingerprint, ":pcm_hash": c.pcm_hash, ":sharpness": c.sharpness,
                ":payload": &c.payload,
            },
        )?;
        Ok(())
    }

    fn make_room(&self) -> rusqlite::Result<()> {
        let page_size: i64 = self
            .db
            .pragma_query_value(None, "page_size", |r| r.get(0))?;
        let limit: i64 = self
            .db
            .pragma_query_value(None, "max_page_count", |r| r.get(0))?;
        let reserve = (2 * MAX_PAYLOAD as i64).min(limit * page_size / 4);
        loop {
            let pages: i64 = self
                .db
                .pragma_query_value(None, "page_count", |r| r.get(0))?;
            let free: i64 = self
                .db
                .pragma_query_value(None, "freelist_count", |r| r.get(0))?;
            if (limit - pages + free) * page_size >= reserve {
                break;
            }
            let removed = self.db.execute(
                "DELETE FROM entries WHERE rowid IN
                 (SELECT rowid FROM entries ORDER BY rowid LIMIT ?1)",
                [PRUNE_BATCH],
            )?;
            if removed == 0 {
                break;
            }
        }
        Ok(())
    }

    fn prune(&self, limit: i64) -> rusqlite::Result<()> {
        let count: i64 = self
            .db
            .query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?;
        if count > limit {
            // FIFO, without a disk write on every hit. Deleted pages are reused, not VACUUMed.
            self.prune_oldest(count.saturating_sub(limit).max(PRUNE_BATCH))?;
        }
        Ok(())
    }

    fn prune_oldest(&self, count: i64) -> rusqlite::Result<()> {
        self.db.execute(
            "DELETE FROM entries WHERE rowid IN
             (SELECT rowid FROM entries ORDER BY rowid LIMIT ?1)",
            [count],
        )?;
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
    payload: Option<String>,
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

// JSON keeps each metadata family together without growing a mostly-empty SQL schema.
// Only successful reads are written; a missing tag inside a successful record stays missing.
macro_rules! metadata {
    ($ty:ty, $slot:expr) => {
        impl DiskValue for $ty {
            const SLOT: i32 = $slot;

            fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
                let payload: String = row.get("payload")?;
                serde_json::from_str(&payload).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        row.as_ref().column_index("payload").unwrap_or(0),
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            }

            fn columns(&self) -> Option<Columns<'_>> {
                let payload = serde_json::to_string(self).ok()?;
                (payload.len() <= MAX_PAYLOAD).then_some(Columns {
                    kind: 5,
                    payload: Some(payload),
                    ..Columns::default()
                })
            }
        }
    };
}

metadata!(crate::exif::ExifInfo, 2);
#[cfg(feature = "tags")]
metadata!(crate::audio_tags::AudioTags, 3);
metadata!(crate::video_frame::VideoMeta, 4);
metadata!(crate::mf_audio::AudioStreamMeta, 5);
metadata!(crate::video_tags::VideoTags, 6);
metadata!((u32, u32), 7);
metadata!(crate::image_decode::PixelFormat, 8);
metadata!(Vec<crate::wic_decode::TiffPage>, 9);

// The public models use static names. Resolve only known literals; never leak a string read
// from disk to manufacture a 'static lifetime. Unknown names cause a normal cache miss.
fn static_name<'de, D: serde::Deserializer<'de>>(
    de: D,
    names: &[&'static str],
) -> Result<Option<&'static str>, D::Error> {
    use serde::Deserialize;
    Option::<String>::deserialize(de)?
        .map(|value| {
            names
                .iter()
                .copied()
                .find(|&name| name == value)
                .ok_or_else(|| serde::de::Error::custom("unknown cached name"))
        })
        .transpose()
}

pub(crate) fn dynamic_range<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<&'static str>, D::Error> {
    static_name(de, &["SDR", "HDR10", "HLG"])
}

#[cfg(feature = "tags")]
pub(crate) fn audio_codec<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<&'static str>, D::Error> {
    // Names returned by audio_tags::codec_name. New names safely miss until added here.
    static_name(
        de,
        &[
            "PCM float",
            "PCM",
            "ADPCM",
            "MP1",
            "MP2",
            "MP3",
            "AAC",
            "Vorbis",
            "Opus",
            "Speex",
            "Musepack",
            "AC-3",
            "E-AC-3",
            "DTS",
            "WMA",
            "FLAC",
            "WavPack",
            "Monkey's Audio",
            "ALAC",
            "TTA",
            "TrueHD",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_frame::VideoMeta;
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

    fn roundtrip<T: DiskValue + PartialEq + std::fmt::Debug>(fixture: &Fixture, value: &T) {
        let mut store = Store::open(&fixture.path()).unwrap();
        put(&mut store, &fixture.key(), value);
        drop(store);
        let reopened = Store::open(&fixture.path()).unwrap();
        assert_eq!(&reopened.read::<T>(&fixture.key()).unwrap(), value);
    }

    #[test]
    fn metadata_survives_reopening_with_unicode_optional_fields_and_floats() {
        let fixture = Fixture::new();
        roundtrip(
            &fixture,
            &crate::exif::ExifInfo {
                make: Some("Камера 🦊".into()),
                model: Some("Model".into()),
                date_time: Some("2020:01:02 03:04:05".into()),
                date_time_original: None,
                exposure_time: Some("1/250".into()),
                f_number: Some(2.8),
                iso: Some(400),
                focal_length: Some(35.5),
                focal_length_35mm: Some(50),
                lens_model: Some("Lens".into()),
                flash_fired: Some(false),
                orientation: Some(6),
                width: Some(6000),
                height: Some(4000),
                software: Some("Editor".into()),
                gps_latitude: Some(-33.865143),
                gps_longitude: Some(151.209900),
            },
        );
        roundtrip(
            &fixture,
            &VideoMeta {
                width: 3840,
                height: 2160,
                duration_sec: 123.45678901234567,
                frame_rate: 30000.0 / 1001.0,
                codec: Some("HEVC".into()),
                bitrate_kbps: Some(50000),
                audio_codec: Some("AAC".into()),
                audio_channels: Some(6),
                audio_sample_rate: Some(48000),
                rotation: Some(90),
                dynamic_range: Some("HDR10"),
            },
        );
        roundtrip(
            &fixture,
            &crate::mf_audio::AudioStreamMeta {
                codec: Some("WMA Lossless".into()),
                lossless: Some(true),
                duration_sec: 52.125,
                bitrate_kbps: Some(1500),
                sample_rate: Some(96000),
                channels: Some(2),
                bit_depth: Some(24),
            },
        );
        roundtrip(
            &fixture,
            &crate::video_tags::VideoTags {
                title: Some("Название".into()),
                artist: Some("Artist".into()),
                director: Some("Director".into()),
                date: Some("2026-11-12".into()),
                genre: None,
                comment: Some("Text\nline".into()),
                transfer: Some(u64::MAX),
                chapters: vec![(0.0, "Старт".into()), (60.125, "".into())],
            },
        );
        roundtrip(&fixture, &(6000u32, 4000u32));
        roundtrip(
            &fixture,
            &crate::image_decode::PixelFormat {
                bits_per_channel: 16,
                channels: 4,
                alpha: true,
            },
        );
        roundtrip(
            &fixture,
            &vec![
                crate::wic_decode::TiffPage {
                    frame: 0,
                    orientation: None,
                },
                crate::wic_decode::TiffPage {
                    frame: 3,
                    orientation: Some(8),
                },
            ],
        );
        // A successful record whose fields are all absent is different from a failed read.
        roundtrip(&fixture, &crate::exif::ExifInfo::default());
        roundtrip(&fixture, &VideoMeta::default());
    }

    #[cfg(feature = "tags")]
    #[test]
    fn audio_metadata_keeps_tags_and_replaygain_but_excludes_cover_and_lyrics() {
        use crate::audio_tags::AudioTags;
        let fixture = Fixture::new();
        let mut tags = AudioTags {
            title: Some("Песня".into()),
            artist: Some("Артист".into()),
            album: Some("Album".into()),
            album_artist: Some("Group".into()),
            genre: Some("Rock".into()),
            comment: Some("Comment".into()),
            year: Some(2026),
            track: Some(2),
            disc: Some(1),
            duration_sec: 180.125,
            bitrate_kbps: Some(960),
            sample_rate: Some(48000),
            bit_depth: Some(24),
            channels: Some(2),
            composer: Some("Composer".into()),
            track_total: Some(12),
            disc_total: Some(2),
            codec: Some("FLAC"),
            lossless: Some(true),
            has_cover: true,
            cover: Some(vec![1, 2, 3]),
            lyrics: Some("private lyrics".into()),
            replay_gain_db: Some(-6.54),
            replay_peak: Some(0.987654),
        };
        let mut store = Store::open(&fixture.path()).unwrap();
        put(&mut store, &fixture.key(), &tags);
        let payload = tags.columns().unwrap().payload.unwrap();
        assert!(!payload.contains("\"cover\"") && !payload.contains("lyrics"));
        // has_cover is deliberately retained, unlike the image bytes.
        assert!(payload.contains("has_cover"));
        tags.cover = None;
        tags.lyrics = None;
        assert_eq!(store.read::<AudioTags>(&fixture.key()).unwrap(), tags);
        drop(store);
        roundtrip(&fixture, &tags);
        roundtrip(&fixture, &AudioTags::default());
    }

    #[test]
    fn schema_upgrade_preserves_existing_analysis_and_sharpness() {
        let fixture = Fixture::new();
        let mut store = Store::open(&fixture.path()).unwrap();
        let analysis = CachedMedia::Image(Arc::new(ImageAnalysis {
            width: 2,
            height: 1,
            aspect_ratio: "2:1".into(),
            dhash: u64::MAX,
            phash: 1 << 63,
            coarse_hash: 123,
        }));
        put(&mut store, &fixture.key(), &analysis);
        put(&mut store, &fixture.key(), &42u32);
        let rows_before: Vec<i64> = store
            .db
            .prepare("SELECT rowid FROM entries ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // This is exactly the previous schema: metadata payloads did not exist yet.
        store
            .db
            .execute_batch("ALTER TABLE entries DROP COLUMN payload; PRAGMA user_version=1;")
            .unwrap();
        drop(store);
        let store = Store::open(&fixture.path()).unwrap();
        assert_eq!(store.read::<u32>(&fixture.key()).unwrap(), 42);
        assert_eq!(
            format!("{:?}", store.read::<CachedMedia>(&fixture.key()).unwrap()),
            format!("{analysis:?}")
        );
        let rows_after: Vec<i64> = store
            .db
            .prepare("SELECT rowid FROM entries ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows_before, rows_after);
        assert_eq!(
            store
                .db
                .pragma_query_value::<i32, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn invalid_and_oversized_metadata_misses_without_losing_other_entries() {
        use crate::exif::ExifInfo;
        let fixture = Fixture::new();
        let mut store = Store::open(&fixture.path()).unwrap();
        put(&mut store, &fixture.key(), &ExifInfo::default());
        put(&mut store, &fixture.key(), &42u32);
        for payload in ["not JSON".to_owned(), "x".repeat(MAX_PAYLOAD + 1)] {
            store
                .db
                .execute(
                    "UPDATE entries SET payload=?1 WHERE slot=?2",
                    (&payload, ExifInfo::SLOT),
                )
                .unwrap();
            assert!(store.read::<ExifInfo>(&fixture.key()).is_err());
            assert_eq!(store.read::<u32>(&fixture.key()).unwrap(), 42);
        }
        let huge = ExifInfo {
            software: Some("x".repeat(MAX_PAYLOAD)),
            ..ExifInfo::default()
        };
        assert!(huge.columns().is_none());
        put(&mut store, &fixture.key(), &ExifInfo::default());
        assert_eq!(
            store.read::<ExifInfo>(&fixture.key()).unwrap(),
            ExifInfo::default()
        );
        let unknown = VideoMeta {
            dynamic_range: Some("future HDR"),
            ..VideoMeta::default()
        };
        put(&mut store, &fixture.key(), &unknown);
        assert!(store.read::<VideoMeta>(&fixture.key()).is_err());
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
    fn a_busy_first_open_retries_after_backoff() {
        let fixture = Fixture::new();
        let blocker = Connection::open(fixture.path()).unwrap();
        blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let now = Instant::now();
        let mut state = State::Unopened;
        state.open_if_needed(|| Some(fixture.path()), now);
        assert!(matches!(state, State::Retry(_)));
        blocker.execute_batch("ROLLBACK").unwrap();
        state.open_if_needed(|| panic!("must respect backoff"), now);
        state.open_if_needed(|| Some(fixture.path()), now + RETRY_DELAY);
        assert!(matches!(state, State::Open(_)));
        let mut disabled = State::Unopened;
        disabled.open_if_needed(|| None, now);
        disabled.open_if_needed(|| panic!("explicitly disabled"), now + RETRY_DELAY);
    }

    #[test]
    fn byte_limit_evicts_records_before_the_row_limit_and_keeps_accepting_writes() {
        let fixture = Fixture::new();
        let mut store = Store::open(&fixture.path()).unwrap();
        store.db.pragma_update(None, "max_page_count", 512).unwrap(); // 2 MiB test limit.
        let mut key = fixture.key();
        let tags = crate::exif::ExifInfo {
            make: Some("x".repeat(32 * 1024)),
            ..Default::default()
        };
        // Fewer than 256 writes: this exercises SQLITE_FULL recovery before periodic pruning.
        for i in 0u32..150 {
            key.path = i.to_le_bytes().to_vec();
            put(&mut store, &key, &tags);
        }
        assert_eq!(store.read::<crate::exif::ExifInfo>(&key).unwrap(), tags);
        let count: i64 = store
            .db
            .query_row("SELECT count(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert!(count < 150);
        let pages: i64 = store
            .db
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        assert!(pages <= 512);
        assert!(store.db.is_autocommit()); // Recovery must leave the connection usable.
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
