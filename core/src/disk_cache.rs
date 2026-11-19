//! Disposable local SQLite cache of analysis and metadata, accessed on a RAM miss.
//! Decoding runs outside database locks; quick UI reads never wait for the connection.

use std::cell::Cell;
use std::fmt::Write as _;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::cache::{AudioAnalysis, AudioDuration, AudioFastFingerprint, CachedMedia, FileKey};
use crate::hashing::ImageAnalysis;
use crate::sqlite::{Connection, Error, Result, Row, Statement};
use crate::video_frame::VideoAnalysis;

// Schema upgrades preserve existing analysis records; record versions invalidate stale results.
const SCHEMA_VERSION: i32 = 2;
// Bump when a cached analysis algorithm or payload representation changes.
const VERSION: i32 = 1;
const MAX_PAYLOAD: usize = 1024 * 1024;
const MAX_BYTES: i64 = 256 * 1024 * 1024;
const MAX_ENTRIES: i64 = 500_000;
const PRUNE_BATCH: i64 = 8192;
const MAINTENANCE_INTERVAL: u32 = 4096;
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

thread_local! {
    // Only the host's background requests may scan/evict records. Viewer reads and quick
    // column calls have no maintenance work or SQLITE_FULL recovery on their critical path.
    static MAINTENANCE_ALLOWED: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn with_maintenance<R>(allowed: bool, f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            MAINTENANCE_ALLOWED.set(self.0);
        }
    }
    let _restore = Restore(MAINTENANCE_ALLOWED.replace(allowed));
    let result = f();
    if allowed {
        // Never open a database just to service a constant field or an unsupported file.
        if let Ok(mut state) = CACHE.try_lock() {
            if let State::Open(store) = &mut *state {
                let _ = store.maintain();
            }
        }
    }
    result
}

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

fn with_store<R>(f: impl FnOnce(&mut Store) -> Result<R>) -> Option<R> {
    // Serialize short background operations so parallel prefetch writes are not lost. Quick
    // UI reads still treat a busy connection as a miss. Never wait on an external DB lock.
    let mut state = if MAINTENANCE_ALLOWED.get() {
        CACHE.lock().ok()?
    } else {
        CACHE.try_lock().ok()?
    };
    state.open_if_needed(cache_path, Instant::now());
    match &mut *state {
        State::Open(store) => f(store).ok(),
        _ => None,
    }
}

pub(crate) fn read<T: DiskValue>(key: &FileKey) -> Option<(T, usize)> {
    let key = Key::new(key)?;
    with_store(|store| store.read_with_weight(&key))
}

/// Return the RAM weight from the same representation prepared for SQLite.
pub(crate) fn write<T: DiskValue>(key: &FileKey, value: &T) -> usize {
    // Unsupported/cancelled analyses must not survive a restart: codecs and permissions change.
    let Some(columns) = value.columns() else {
        return usize::MAX;
    };
    let bytes = columns.memory_bytes();
    if let Some(key) = Key::new(key) {
        with_store(|store| store.write(&key, T::SLOT, columns, MAINTENANCE_ALLOWED.get()));
    }
    bytes
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
    read_stmt: Statement,
    insert_stmt: Statement,
    db: Connection,
    writes: u32,
}

impl Store {
    fn open(path: &Path) -> Result<Self> {
        // No fallback to the DLL or media directory: those may be read-only or on a NAS.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| Error::InvalidPath)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::ZERO)?;
        let version = db.pragma_query_i64("user_version")?;
        if !(0..=SCHEMA_VERSION as i64).contains(&version) {
            return Err(Error::InvalidQuery);
        }
        // Bound the main database and SQLite's RAM. WAL is checkpointed every ~1 MiB and
        // truncated after checkpoints; only short transactions are used by this plugin.
        let page_size = db.pragma_query_i64("page_size")?;
        if page_size <= 0 {
            return Err(Error::InvalidQuery);
        }
        let pages = db.pragma_update_and_check_i64("max_page_count", MAX_BYTES / page_size)?;
        if pages > MAX_BYTES / page_size {
            return Err(Error::InvalidQuery);
        }
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA cache_size=-4096;
             PRAGMA mmap_size=0;
             PRAGMA wal_autocheckpoint=2048;
             PRAGMA journal_size_limit=8388608;",
        )?;
        if version < SCHEMA_VERSION as i64 {
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
        let read_stmt = db.prepare(
            "SELECT kind, width, height, duration, hash_a, hash_b, coarse,
                    fingerprint, pcm_hash, sharpness, payload FROM entries
             WHERE path=?1 AND slot=?2 AND size=?3 AND modified=?4 AND version=?5
               AND (payload IS NULL OR length(CAST(payload AS BLOB)) <= ?6)",
        )?;
        let insert_stmt = db.prepare(
            "INSERT OR REPLACE INTO entries
             (path, slot, size, modified, version, kind, width, height, duration,
              hash_a, hash_b, coarse, fingerprint, pcm_hash, sharpness, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )?;
        // First maintenance is due, but opening for a quick read must not scan the database.
        Ok(Self {
            read_stmt,
            insert_stmt,
            db,
            writes: MAINTENANCE_INTERVAL,
        })
    }

    #[cfg(test)]
    fn read<T: DiskValue>(&self, key: &Key) -> Result<T> {
        self.read_with_weight(key).map(|(value, _)| value)
    }

    fn read_with_weight<T: DiskValue>(&self, key: &Key) -> Result<(T, usize)> {
        let modified: &[u8] = &key.modified;
        self.read_stmt.query_row(
            &[
                &key.path,
                &T::SLOT,
                &key.size,
                &modified,
                &VERSION,
                &(MAX_PAYLOAD as i64),
            ],
            |row| {
                let payload_len = row.column_bytes(COL_PAYLOAD).unwrap_or(0);
                let value = T::from_row(row)?;
                Ok((value, Columns::memory_bytes_for_len(payload_len)))
            },
        )
    }

    fn write(&mut self, key: &Key, slot: i32, c: Columns<'_>, allow_eviction: bool) -> Result<()> {
        // Reject unbounded records before eviction (normal analysis strings are tiny).
        if c.payload.as_ref().map_or(0, String::len)
            + c.fingerprint.map_or(0, str::len)
            + c.pcm_hash.map_or(0, str::len)
            > MAX_PAYLOAD
        {
            return Err(Error::InvalidQuery);
        }
        if !allow_eviction && self.writes >= MAINTENANCE_INTERVAL {
            // max(rowid) is an indexed endpoint lookup, not a count scan. Once this
            // conservative bound is reached, leave new writes to a background request.
            let newest: i64 =
                self.db
                    .query_row("SELECT coalesce(max(rowid), 0) FROM entries", &[], |r| {
                        r.get(0)
                    })?;
            if newest >= MAX_ENTRIES {
                return Ok(());
            }
        }
        if let Err(error) = self.insert(key, slot, &c) {
            if !allow_eviction || !error.is_full() {
                return Err(error);
            }
            // Also handles a larger replacement or another process filling our reserved room.
            self.make_room()?;
            self.insert(key, slot, &c)?;
        }
        self.writes = self.writes.saturating_add(1);
        Ok(())
    }

    fn maintain(&mut self) -> Result<()> {
        if self.writes >= MAINTENANCE_INTERVAL {
            self.prune(MAX_ENTRIES)?;
            self.make_room()?;
            self.writes = 0;
        }
        Ok(())
    }

    fn insert(&self, key: &Key, slot: i32, c: &Columns<'_>) -> Result<()> {
        let modified: &[u8] = &key.modified;
        self.insert_stmt.execute(&[
            &key.path,
            &slot,
            &key.size,
            &modified,
            &VERSION,
            &c.kind,
            &c.width,
            &c.height,
            &c.duration,
            &c.hash_a,
            &c.hash_b,
            &c.coarse,
            &c.fingerprint,
            &c.pcm_hash,
            &c.sharpness,
            &c.payload,
        ])?;
        Ok(())
    }

    fn make_room(&self) -> Result<()> {
        let page_size = self.db.pragma_query_i64("page_size")?;
        let limit = self.db.pragma_query_i64("max_page_count")?;
        let reserve = (2 * MAX_PAYLOAD as i64).min(limit * page_size / 4);
        let delete_stmt = self.db.prepare(
            "DELETE FROM entries WHERE rowid IN
             (SELECT rowid FROM entries ORDER BY rowid LIMIT ?1)",
        )?;
        loop {
            let pages = self.db.pragma_query_i64("page_count")?;
            let free = self.db.pragma_query_i64("freelist_count")?;
            if (limit - pages + free) * page_size >= reserve {
                break;
            }
            let removed = delete_stmt.execute(&[&PRUNE_BATCH])?;
            if removed == 0 {
                break;
            }
        }
        Ok(())
    }

    fn prune(&self, limit: i64) -> Result<()> {
        let count: i64 = self
            .db
            .query_row("SELECT count(*) FROM entries", &[], |r| r.get(0))?;
        if count > limit {
            // FIFO, without a disk write on every hit. Deleted pages are reused, not VACUUMed.
            self.prune_oldest(count.saturating_sub(limit).max(PRUNE_BATCH))?;
        }
        Ok(())
    }

    fn prune_oldest(&self, count: i64) -> Result<()> {
        self.db.execute(
            "DELETE FROM entries WHERE rowid IN
             (SELECT rowid FROM entries ORDER BY rowid LIMIT ?1)",
            &[&count],
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

impl Columns<'_> {
    pub(crate) fn memory_bytes_for_len(payload_len: usize) -> usize {
        // JSON length plus allocator/container overhead. Small fixed fields cost little;
        // variable-length strings and vectors dominate unusually large metadata.
        payload_len
            .saturating_mul(3)
            .saturating_add(size_of::<Self>())
    }

    pub(crate) fn memory_bytes(&self) -> usize {
        Self::memory_bytes_for_len(self.payload.as_ref().map_or(0, String::len))
    }
}

const COL_KIND: usize = 0;
const COL_WIDTH: usize = 1;
const COL_HEIGHT: usize = 2;
const COL_DURATION: usize = 3;
const COL_HASH_A: usize = 4;
const COL_HASH_B: usize = 5;
const COL_COARSE: usize = 6;
const COL_FINGERPRINT: usize = 7;
const COL_PCM_HASH: usize = 8;
const COL_SHARPNESS: usize = 9;
const COL_PAYLOAD: usize = 10;

pub(crate) trait DiskValue: Sized {
    const SLOT: i32;
    fn from_row(row: &Row<'_>) -> Result<Self>;
    fn columns(&self) -> Option<Columns<'_>>;
}

impl DiskValue for CachedMedia {
    const SLOT: i32 = 0;

    fn from_row(r: &Row<'_>) -> Result<Self> {
        Ok(match r.get::<i32>(COL_KIND)? {
            1 => {
                let width = r.get(COL_WIDTH)?;
                let height = r.get(COL_HEIGHT)?;
                Self::Image(Arc::new(ImageAnalysis {
                    width,
                    height,
                    dhash: r.get::<i64>(COL_HASH_A)? as u64,
                    phash: r.get::<i64>(COL_HASH_B)? as u64,
                    coarse_hash: r.get(COL_COARSE)?,
                }))
            }
            2 => Self::Video(Arc::new(VideoAnalysis {
                width: r.get(COL_WIDTH)?,
                height: r.get(COL_HEIGHT)?,
                duration_sec: r.get(COL_DURATION)?,
                dhash_mid: r.get::<Option<i64>>(COL_HASH_A)?.map(|v| v as u64),
                fingerprint: r.get(COL_FINGERPRINT)?,
            })),
            _ => return Err(Error::InvalidQuery),
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
            // A decoder failure can leave valid dimensions/duration but missing hashes.
            // Keep that in RAM only, so another session can retry decoding the video.
            Self::Video(_) | Self::Unsupported => return None,
        })
    }
}

// Combined sequential audio analysis (fingerprint + PCM hash) shares slot 10; seek-based
// fast fingerprint uses slot 11 independently; decoded duration uses slot 12.
impl DiskValue for AudioAnalysis {
    const SLOT: i32 = 10;

    fn from_row(r: &Row<'_>) -> Result<Self> {
        if r.get::<i32>(COL_KIND)? != Self::SLOT {
            return Err(Error::TypeMismatch);
        }
        Ok(Self {
            duration_sec: r.get(COL_DURATION)?,
            pcm_hash: r.get(COL_PCM_HASH)?,
            fingerprint: r.get(COL_FINGERPRINT)?,
        })
    }

    fn columns(&self) -> Option<Columns<'_>> {
        Some(Columns {
            kind: Self::SLOT,
            duration: Some(self.duration_sec),
            fingerprint: self.fingerprint.as_deref(),
            pcm_hash: Some(&self.pcm_hash),
            ..Columns::default()
        })
    }
}

impl DiskValue for AudioFastFingerprint {
    const SLOT: i32 = 11;

    fn from_row(r: &Row<'_>) -> Result<Self> {
        if r.get::<i32>(COL_KIND)? != Self::SLOT {
            return Err(Error::TypeMismatch);
        }
        if r.get::<Option<String>>(COL_PCM_HASH)?.is_some() {
            return Err(Error::InvalidQuery);
        }
        Ok(Self {
            duration_sec: r.get(COL_DURATION)?,
            fingerprint: r.get(COL_FINGERPRINT)?,
        })
    }

    fn columns(&self) -> Option<Columns<'_>> {
        Some(Columns {
            kind: Self::SLOT,
            duration: Some(self.duration_sec),
            fingerprint: self.fingerprint.as_deref(),
            ..Columns::default()
        })
    }
}

impl DiskValue for AudioDuration {
    const SLOT: i32 = 12;

    fn from_row(r: &Row<'_>) -> Result<Self> {
        if r.get::<i32>(COL_KIND)? != Self::SLOT {
            return Err(Error::TypeMismatch);
        }
        r.get(COL_DURATION).map(Self)
    }

    fn columns(&self) -> Option<Columns<'_>> {
        Some(Columns {
            kind: Self::SLOT,
            duration: Some(self.0),
            ..Columns::default()
        })
    }
}

impl DiskValue for u32 {
    const SLOT: i32 = 1;

    fn from_row(r: &Row<'_>) -> Result<Self> {
        r.get(COL_SHARPNESS)
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
trait JsonCodec: Sized {
    fn to_json(&self) -> Option<String>;
    fn from_json(s: &str) -> Option<Self>;
}

macro_rules! metadata {
    ($ty:ty, $slot:expr) => {
        impl DiskValue for $ty {
            const SLOT: i32 = $slot;

            fn from_row(row: &Row<'_>) -> Result<Self> {
                let payload: String = row.get(COL_PAYLOAD)?;
                <$ty as JsonCodec>::from_json(&payload).ok_or(Error::InvalidQuery)
            }

            fn columns(&self) -> Option<Columns<'_>> {
                let payload = <$ty as JsonCodec>::to_json(self)?;
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

fn write_json_str(out: &mut String, s: &str) {
    out.push('"');
    let mut last = 0;
    for (i, ch) in s.char_indices() {
        let esc = match ch {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            c if (c as u32) < 0x20 => {
                out.push_str(&s[last..i]);
                let _ = write!(out, "\\u{:04x}", c as u32);
                last = i + ch.len_utf8();
                continue;
            }
            _ => continue,
        };
        out.push_str(&s[last..i]);
        out.push_str(esc);
        last = i + ch.len_utf8();
    }
    out.push_str(&s[last..]);
    out.push('"');
}

struct ObjWriter {
    buf: String,
    first: bool,
}

impl ObjWriter {
    fn new() -> Self {
        Self {
            buf: String::from("{"),
            first: true,
        }
    }

    fn key(&mut self, k: &str) {
        if !self.first {
            self.buf.push(',');
        }
        self.first = false;
        write_json_str(&mut self.buf, k);
        self.buf.push(':');
    }

    fn str_opt(&mut self, k: &str, v: Option<&str>) {
        self.key(k);
        match v {
            Some(s) => write_json_str(&mut self.buf, s),
            None => self.buf.push_str("null"),
        }
    }

    fn u64_opt(&mut self, k: &str, v: Option<u64>) {
        self.key(k);
        match v {
            Some(n) => {
                let _ = write!(self.buf, "{n}");
            }
            None => self.buf.push_str("null"),
        }
    }

    fn u32_opt(&mut self, k: &str, v: Option<u32>) {
        self.u64_opt(k, v.map(u64::from));
    }

    fn u16_opt(&mut self, k: &str, v: Option<u16>) {
        self.u64_opt(k, v.map(u64::from));
    }

    fn u8_opt(&mut self, k: &str, v: Option<u8>) {
        self.u64_opt(k, v.map(u64::from));
    }

    fn u32_val(&mut self, k: &str, v: u32) {
        self.u64_opt(k, Some(u64::from(v)));
    }

    fn bool_opt(&mut self, k: &str, v: Option<bool>) {
        self.key(k);
        match v {
            Some(b) => self.buf.push_str(if b { "true" } else { "false" }),
            None => self.buf.push_str("null"),
        }
    }

    fn bool_val(&mut self, k: &str, v: bool) {
        self.bool_opt(k, Some(v));
    }

    fn f64_val(&mut self, k: &str, v: f64) -> bool {
        if !v.is_finite() {
            return false;
        }
        self.key(k);
        let _ = write!(self.buf, "{v:?}");
        true
    }

    fn f64_opt(&mut self, k: &str, v: Option<f64>) -> bool {
        match v {
            Some(n) => self.f64_val(k, n),
            None => {
                self.key(k);
                self.buf.push_str("null");
                true
            }
        }
    }

    fn f32_opt(&mut self, k: &str, v: Option<f32>) -> bool {
        match v {
            Some(n) => {
                if !n.is_finite() {
                    return false;
                }
                self.key(k);
                let _ = write!(self.buf, "{n:?}");
                true
            }
            None => {
                self.key(k);
                self.buf.push_str("null");
                true
            }
        }
    }

    fn finish(mut self) -> String {
        self.buf.push('}');
        self.buf
    }
}

struct JsonParser<'a> {
    s: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            s,
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn try_eat(&mut self, byte: u8) -> bool {
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        self.try_eat(byte).then_some(())
    }

    fn eat_lit(&mut self, lit: &[u8]) -> bool {
        self.skip_ws();
        if self.bytes.get(self.pos..self.pos + lit.len()) == Some(lit) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    fn finish(&mut self) -> Option<()> {
        self.skip_ws();
        (self.pos == self.bytes.len()).then_some(())
    }

    fn parse_hex4(&mut self) -> Option<u16> {
        let hex = self.s.get(self.pos..self.pos + 4)?;
        let v = u16::from_str_radix(hex, 16).ok()?;
        self.pos += 4;
        Some(v)
    }

    fn parse_str(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let start = self.pos;
        let mut i = start;
        while i < self.bytes.len() {
            let b = self.bytes[i];
            if b == b'"' {
                let plain = self.s.get(start..i)?.to_owned();
                self.pos = i + 1;
                return Some(plain);
            }
            if b == b'\\' || b < 0x20 {
                break;
            }
            i += 1;
        }
        let mut out = String::from(self.s.get(start..i)?);
        self.pos = i;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Some(out);
            }
            if b < 0x20 {
                return None;
            }
            if b == b'\\' {
                self.pos += 1;
                let esc = *self.bytes.get(self.pos)?;
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code) {
                            if self.bytes.get(self.pos..self.pos + 2) != Some(b"\\u") {
                                return None;
                            }
                            self.pos += 2;
                            let low = self.parse_hex4()?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return None;
                            }
                            let cp = 0x10000
                                + (((u32::from(code) - 0xD800) << 10) | (u32::from(low) - 0xDC00));
                            out.push(char::from_u32(cp)?);
                        } else {
                            out.push(char::from_u32(u32::from(code))?);
                        }
                    }
                    _ => return None,
                }
            } else {
                let ch = self.s.get(self.pos..)?.chars().next()?;
                out.push(ch);
                self.pos += ch.len_utf8();
            }
        }
        None
    }

    fn parse_bool(&mut self) -> Option<bool> {
        if self.eat_lit(b"true") {
            Some(true)
        } else if self.eat_lit(b"false") {
            Some(false)
        } else {
            None
        }
    }

    fn parse_num_str(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.pos;
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                self.pos += 1;
            } else {
                break;
            }
        }
        (self.pos > start).then(|| &self.s[start..self.pos])
    }

    fn parse_u64(&mut self) -> Option<u64> {
        self.parse_num_str()?.parse().ok()
    }

    fn parse_u32(&mut self) -> Option<u32> {
        self.parse_num_str()?.parse().ok()
    }

    fn parse_u16(&mut self) -> Option<u16> {
        self.parse_num_str()?.parse().ok()
    }

    fn parse_u8(&mut self) -> Option<u8> {
        self.parse_num_str()?.parse().ok()
    }

    fn parse_f64(&mut self) -> Option<f64> {
        self.parse_num_str()?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
    }

    fn parse_f32(&mut self) -> Option<f32> {
        self.parse_num_str()?
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
    }

    fn parse_opt<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<Option<T>> {
        if self.eat_lit(b"null") {
            Some(None)
        } else {
            f(self).map(Some)
        }
    }

    fn parse_arr(&mut self, mut on_elem: impl FnMut(&mut Self) -> Option<()>) -> Option<()> {
        self.expect(b'[')?;
        self.skip_ws();
        if self.try_eat(b']') {
            return Some(());
        }
        loop {
            on_elem(self)?;
            self.skip_ws();
            if self.try_eat(b']') {
                return Some(());
            }
            self.expect(b',')?;
        }
    }

    fn parse_obj(&mut self, mut on_field: impl FnMut(&str, &mut Self) -> Option<()>) -> Option<()> {
        self.expect(b'{')?;
        self.skip_ws();
        if self.try_eat(b'}') {
            return Some(());
        }
        loop {
            let key = self.parse_str()?;
            self.expect(b':')?;
            on_field(&key, self)?;
            self.skip_ws();
            if self.try_eat(b'}') {
                return Some(());
            }
            self.expect(b',')?;
        }
    }

    fn skip_value(&mut self) -> Option<()> {
        self.skip_value_depth(32)
    }

    fn skip_value_depth(&mut self, depth: u8) -> Option<()> {
        if depth == 0 {
            return None;
        }
        self.skip_ws();
        match *self.bytes.get(self.pos)? {
            b'"' => {
                self.parse_str()?;
                Some(())
            }
            b'{' => self.parse_obj(|_, p| p.skip_value_depth(depth - 1)),
            b'[' => self.parse_arr(|p| p.skip_value_depth(depth - 1)),
            b't' => self.eat_lit(b"true").then_some(()),
            b'f' => self.eat_lit(b"false").then_some(()),
            b'n' => self.eat_lit(b"null").then_some(()),
            b'-' | b'0'..=b'9' => self.parse_num_str().map(|_| ()),
            _ => None,
        }
    }
}

// The public models use static names. Resolve only known literals; never leak a string read
// from disk to manufacture a 'static lifetime. Unknown names cause a normal cache miss.
fn static_lookup(value: &str, names: &[&'static str]) -> Option<&'static str> {
    names.iter().copied().find(|&name| name == value)
}

#[cfg(feature = "tags")]
const AUDIO_CODECS: &[&str] = &[
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
];

impl JsonCodec for crate::exif::ExifInfo {
    fn to_json(&self) -> Option<String> {
        let mut w = ObjWriter::new();
        w.str_opt("make", self.make.as_deref());
        w.str_opt("model", self.model.as_deref());
        w.str_opt("date_time", self.date_time.as_deref());
        w.str_opt("date_time_original", self.date_time_original.as_deref());
        w.str_opt("exposure_time", self.exposure_time.as_deref());
        if !w.f64_opt("f_number", self.f_number) {
            return None;
        }
        w.u32_opt("iso", self.iso);
        if !w.f64_opt("focal_length", self.focal_length) {
            return None;
        }
        w.u32_opt("focal_length_35mm", self.focal_length_35mm);
        w.str_opt("lens_model", self.lens_model.as_deref());
        w.bool_opt("flash_fired", self.flash_fired);
        w.u16_opt("orientation", self.orientation);
        w.u32_opt("width", self.width);
        w.u32_opt("height", self.height);
        w.str_opt("software", self.software.as_deref());
        if !w.f64_opt("gps_latitude", self.gps_latitude)
            || !w.f64_opt("gps_longitude", self.gps_longitude)
        {
            return None;
        }
        Some(w.finish())
    }

    fn from_json(s: &str) -> Option<Self> {
        let mut v = Self::default();
        let mut p = JsonParser::new(s);
        p.parse_obj(|k, p| {
            match k {
                "make" => v.make = p.parse_opt(JsonParser::parse_str)?,
                "model" => v.model = p.parse_opt(JsonParser::parse_str)?,
                "date_time" => v.date_time = p.parse_opt(JsonParser::parse_str)?,
                "date_time_original" => {
                    v.date_time_original = p.parse_opt(JsonParser::parse_str)?;
                }
                "exposure_time" => v.exposure_time = p.parse_opt(JsonParser::parse_str)?,
                "f_number" => v.f_number = p.parse_opt(JsonParser::parse_f64)?,
                "iso" => v.iso = p.parse_opt(JsonParser::parse_u32)?,
                "focal_length" => v.focal_length = p.parse_opt(JsonParser::parse_f64)?,
                "focal_length_35mm" => v.focal_length_35mm = p.parse_opt(JsonParser::parse_u32)?,
                "lens_model" => v.lens_model = p.parse_opt(JsonParser::parse_str)?,
                "flash_fired" => v.flash_fired = p.parse_opt(JsonParser::parse_bool)?,
                "orientation" => v.orientation = p.parse_opt(JsonParser::parse_u16)?,
                "width" => v.width = p.parse_opt(JsonParser::parse_u32)?,
                "height" => v.height = p.parse_opt(JsonParser::parse_u32)?,
                "software" => v.software = p.parse_opt(JsonParser::parse_str)?,
                "gps_latitude" => v.gps_latitude = p.parse_opt(JsonParser::parse_f64)?,
                "gps_longitude" => v.gps_longitude = p.parse_opt(JsonParser::parse_f64)?,
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        p.finish()?;
        Some(v)
    }
}

#[cfg(feature = "tags")]
impl JsonCodec for crate::audio_tags::AudioTags {
    fn to_json(&self) -> Option<String> {
        let mut w = ObjWriter::new();
        w.str_opt("title", self.title.as_deref());
        w.str_opt("artist", self.artist.as_deref());
        w.str_opt("album", self.album.as_deref());
        w.str_opt("album_artist", self.album_artist.as_deref());
        w.str_opt("genre", self.genre.as_deref());
        w.str_opt("comment", self.comment.as_deref());
        w.u32_opt("year", self.year);
        w.u32_opt("track", self.track);
        w.u32_opt("disc", self.disc);
        if !w.f64_val("duration_sec", self.duration_sec) {
            return None;
        }
        w.u32_opt("bitrate_kbps", self.bitrate_kbps);
        w.u32_opt("sample_rate", self.sample_rate);
        w.u8_opt("bit_depth", self.bit_depth);
        w.u8_opt("channels", self.channels);
        w.str_opt("composer", self.composer.as_deref());
        w.u32_opt("track_total", self.track_total);
        w.u32_opt("disc_total", self.disc_total);
        w.str_opt("codec", self.codec);
        w.bool_opt("lossless", self.lossless);
        w.bool_val("has_cover", self.has_cover);
        if !w.f32_opt("replay_gain_db", self.replay_gain_db)
            || !w.f32_opt("replay_peak", self.replay_peak)
        {
            return None;
        }
        Some(w.finish())
    }

    fn from_json(s: &str) -> Option<Self> {
        let mut v = Self::default();
        let mut p = JsonParser::new(s);
        p.parse_obj(|k, p| {
            match k {
                "title" => v.title = p.parse_opt(JsonParser::parse_str)?,
                "artist" => v.artist = p.parse_opt(JsonParser::parse_str)?,
                "album" => v.album = p.parse_opt(JsonParser::parse_str)?,
                "album_artist" => v.album_artist = p.parse_opt(JsonParser::parse_str)?,
                "genre" => v.genre = p.parse_opt(JsonParser::parse_str)?,
                "comment" => v.comment = p.parse_opt(JsonParser::parse_str)?,
                "year" => v.year = p.parse_opt(JsonParser::parse_u32)?,
                "track" => v.track = p.parse_opt(JsonParser::parse_u32)?,
                "disc" => v.disc = p.parse_opt(JsonParser::parse_u32)?,
                "duration_sec" => v.duration_sec = p.parse_f64()?,
                "bitrate_kbps" => v.bitrate_kbps = p.parse_opt(JsonParser::parse_u32)?,
                "sample_rate" => v.sample_rate = p.parse_opt(JsonParser::parse_u32)?,
                "bit_depth" => v.bit_depth = p.parse_opt(JsonParser::parse_u8)?,
                "channels" => v.channels = p.parse_opt(JsonParser::parse_u8)?,
                "composer" => v.composer = p.parse_opt(JsonParser::parse_str)?,
                "track_total" => v.track_total = p.parse_opt(JsonParser::parse_u32)?,
                "disc_total" => v.disc_total = p.parse_opt(JsonParser::parse_u32)?,
                "codec" => {
                    v.codec = match p.parse_opt(JsonParser::parse_str)? {
                        Some(name) => Some(static_lookup(&name, AUDIO_CODECS)?),
                        None => None,
                    };
                }
                "lossless" => v.lossless = p.parse_opt(JsonParser::parse_bool)?,
                "has_cover" => v.has_cover = p.parse_bool()?,
                "replay_gain_db" => v.replay_gain_db = p.parse_opt(JsonParser::parse_f32)?,
                "replay_peak" => v.replay_peak = p.parse_opt(JsonParser::parse_f32)?,
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        p.finish()?;
        Some(v)
    }
}

impl JsonCodec for crate::video_frame::VideoMeta {
    fn to_json(&self) -> Option<String> {
        let mut w = ObjWriter::new();
        w.u32_val("width", self.width);
        w.u32_val("height", self.height);
        if !w.f64_val("duration_sec", self.duration_sec)
            || !w.f64_val("frame_rate", self.frame_rate)
        {
            return None;
        }
        w.str_opt("codec", self.codec.as_deref());
        w.u32_opt("bitrate_kbps", self.bitrate_kbps);
        w.str_opt("audio_codec", self.audio_codec.as_deref());
        w.u32_opt("audio_channels", self.audio_channels);
        w.u32_opt("audio_sample_rate", self.audio_sample_rate);
        w.u32_opt("rotation", self.rotation);
        w.str_opt("dynamic_range", self.dynamic_range);
        Some(w.finish())
    }

    fn from_json(s: &str) -> Option<Self> {
        let mut v = Self::default();
        let mut p = JsonParser::new(s);
        p.parse_obj(|k, p| {
            match k {
                "width" => v.width = p.parse_u32()?,
                "height" => v.height = p.parse_u32()?,
                "duration_sec" => v.duration_sec = p.parse_f64()?,
                "frame_rate" => v.frame_rate = p.parse_f64()?,
                "codec" => v.codec = p.parse_opt(JsonParser::parse_str)?,
                "bitrate_kbps" => v.bitrate_kbps = p.parse_opt(JsonParser::parse_u32)?,
                "audio_codec" => v.audio_codec = p.parse_opt(JsonParser::parse_str)?,
                "audio_channels" => v.audio_channels = p.parse_opt(JsonParser::parse_u32)?,
                "audio_sample_rate" => v.audio_sample_rate = p.parse_opt(JsonParser::parse_u32)?,
                "rotation" => v.rotation = p.parse_opt(JsonParser::parse_u32)?,
                "dynamic_range" => {
                    v.dynamic_range = match p.parse_opt(JsonParser::parse_str)? {
                        Some(name) => Some(static_lookup(&name, &["SDR", "HDR10", "HLG"])?),
                        None => None,
                    };
                }
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        p.finish()?;
        Some(v)
    }
}

impl JsonCodec for crate::mf_audio::AudioStreamMeta {
    fn to_json(&self) -> Option<String> {
        let mut w = ObjWriter::new();
        w.str_opt("codec", self.codec.as_deref());
        w.bool_opt("lossless", self.lossless);
        if !w.f64_val("duration_sec", self.duration_sec) {
            return None;
        }
        w.u32_opt("bitrate_kbps", self.bitrate_kbps);
        w.u32_opt("sample_rate", self.sample_rate);
        w.u32_opt("channels", self.channels);
        w.u32_opt("bit_depth", self.bit_depth);
        Some(w.finish())
    }

    fn from_json(s: &str) -> Option<Self> {
        let mut v = Self::default();
        let mut p = JsonParser::new(s);
        p.parse_obj(|k, p| {
            match k {
                "codec" => v.codec = p.parse_opt(JsonParser::parse_str)?,
                "lossless" => v.lossless = p.parse_opt(JsonParser::parse_bool)?,
                "duration_sec" => v.duration_sec = p.parse_f64()?,
                "bitrate_kbps" => v.bitrate_kbps = p.parse_opt(JsonParser::parse_u32)?,
                "sample_rate" => v.sample_rate = p.parse_opt(JsonParser::parse_u32)?,
                "channels" => v.channels = p.parse_opt(JsonParser::parse_u32)?,
                "bit_depth" => v.bit_depth = p.parse_opt(JsonParser::parse_u32)?,
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        p.finish()?;
        Some(v)
    }
}

impl JsonCodec for crate::video_tags::VideoTags {
    fn to_json(&self) -> Option<String> {
        let mut w = ObjWriter::new();
        w.str_opt("title", self.title.as_deref());
        w.str_opt("artist", self.artist.as_deref());
        w.str_opt("director", self.director.as_deref());
        w.str_opt("date", self.date.as_deref());
        w.str_opt("genre", self.genre.as_deref());
        w.str_opt("comment", self.comment.as_deref());
        w.u64_opt("transfer", self.transfer);
        w.key("chapters");
        w.buf.push('[');
        for (i, (time, name)) in self.chapters.iter().enumerate() {
            if !time.is_finite() {
                return None;
            }
            if i > 0 {
                w.buf.push(',');
            }
            let _ = write!(w.buf, "[{time:?},");
            write_json_str(&mut w.buf, name);
            w.buf.push(']');
        }
        w.buf.push(']');
        Some(w.finish())
    }

    fn from_json(s: &str) -> Option<Self> {
        let mut v = Self::default();
        let mut p = JsonParser::new(s);
        p.parse_obj(|k, p| {
            match k {
                "title" => v.title = p.parse_opt(JsonParser::parse_str)?,
                "artist" => v.artist = p.parse_opt(JsonParser::parse_str)?,
                "director" => v.director = p.parse_opt(JsonParser::parse_str)?,
                "date" => v.date = p.parse_opt(JsonParser::parse_str)?,
                "genre" => v.genre = p.parse_opt(JsonParser::parse_str)?,
                "comment" => v.comment = p.parse_opt(JsonParser::parse_str)?,
                "transfer" => v.transfer = p.parse_opt(JsonParser::parse_u64)?,
                "chapters" => {
                    let mut chapters = Vec::new();
                    p.parse_arr(|p| {
                        p.expect(b'[')?;
                        let time = p.parse_f64()?;
                        p.expect(b',')?;
                        let name = p.parse_str()?;
                        p.expect(b']')?;
                        chapters.push((time, name));
                        Some(())
                    })?;
                    v.chapters = chapters;
                }
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        p.finish()?;
        Some(v)
    }
}

impl JsonCodec for (u32, u32) {
    fn to_json(&self) -> Option<String> {
        Some(format!("[{},{}]", self.0, self.1))
    }

    fn from_json(s: &str) -> Option<Self> {
        let mut p = JsonParser::new(s);
        p.expect(b'[')?;
        let w = p.parse_u32()?;
        p.expect(b',')?;
        let h = p.parse_u32()?;
        p.expect(b']')?;
        p.finish()?;
        Some((w, h))
    }
}

impl JsonCodec for crate::image_decode::PixelFormat {
    fn to_json(&self) -> Option<String> {
        let mut w = ObjWriter::new();
        w.u32_val("bits_per_channel", self.bits_per_channel);
        w.u32_val("channels", self.channels);
        w.bool_val("alpha", self.alpha);
        Some(w.finish())
    }

    fn from_json(s: &str) -> Option<Self> {
        let (mut bits, mut ch, mut alpha) = (0, 0, false);
        let (mut seen_bits, mut seen_ch, mut seen_alpha) = (false, false, false);
        let mut p = JsonParser::new(s);
        p.parse_obj(|k, p| {
            match k {
                "bits_per_channel" => {
                    bits = p.parse_u32()?;
                    seen_bits = true;
                }
                "channels" => {
                    ch = p.parse_u32()?;
                    seen_ch = true;
                }
                "alpha" => {
                    alpha = p.parse_bool()?;
                    seen_alpha = true;
                }
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        p.finish()?;
        (seen_bits && seen_ch && seen_alpha).then_some(Self {
            bits_per_channel: bits,
            channels: ch,
            alpha,
        })
    }
}

impl JsonCodec for Vec<crate::wic_decode::TiffPage> {
    fn to_json(&self) -> Option<String> {
        let mut buf = String::from("[");
        for (i, page) in self.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            let mut w = ObjWriter::new();
            w.u32_val("frame", page.frame);
            w.u16_opt("orientation", page.orientation);
            buf.push_str(&w.finish());
        }
        buf.push(']');
        Some(buf)
    }

    fn from_json(s: &str) -> Option<Self> {
        let mut pages = Vec::new();
        let mut p = JsonParser::new(s);
        p.parse_arr(|p| {
            let (mut frame, mut orientation, mut seen_frame) = (0, None, false);
            p.parse_obj(|k, p| {
                match k {
                    "frame" => {
                        frame = p.parse_u32()?;
                        seen_frame = true;
                    }
                    "orientation" => orientation = p.parse_opt(JsonParser::parse_u16)?,
                    _ => p.skip_value()?,
                }
                Some(())
            })?;
            if !seen_frame {
                return None;
            }
            pages.push(crate::wic_decode::TiffPage { frame, orientation });
            Some(())
        })?;
        p.finish()?;
        Some(pages)
    }
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
            std::fs::write(dir.join("С„РѕС‚Рѕ рџ¦Љ.jpg"), b"fixture").unwrap();
            Self(dir)
        }

        fn path(&self) -> PathBuf {
            self.0.join("cache.sqlite3")
        }
        fn key(&self) -> Key {
            Key::new(&FileKey::stat(&self.0.join("С„РѕС‚Рѕ рџ¦Љ.jpg")).unwrap()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn put<T: DiskValue>(store: &mut Store, key: &Key, value: &T) {
        store
            .write(key, T::SLOT, value.columns().unwrap(), true)
            .unwrap();
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
                make: Some("РљР°РјРµСЂР° рџ¦Љ".into()),
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
                title: Some("РќР°Р·РІР°РЅРёРµ".into()),
                artist: Some("Artist".into()),
                director: Some("Director".into()),
                date: Some("2026-11-12".into()),
                genre: None,
                comment: Some("Text\nline".into()),
                transfer: Some(u64::MAX),
                chapters: vec![(0.0, "РЎС‚Р°СЂС‚".into()), (60.125, "".into())],
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
            title: Some("РџРµСЃРЅСЏ".into()),
            artist: Some("РђСЂС‚РёСЃС‚".into()),
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
            .query_map(&[], |r| r.get(0))
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
            .query_map(&[], |r| r.get(0))
            .unwrap();
        assert_eq!(rows_before, rows_after);
        assert_eq!(
            store.db.pragma_query_i64("user_version").unwrap(),
            SCHEMA_VERSION as i64
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
                    &[&payload, &ExifInfo::SLOT],
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
    fn audio_fields_persist_independently_including_silent_fingerprints() {
        let fixture = Fixture::new();
        let key = fixture.key();
        for fingerprint in [Some("180s_12_34_56".into()), None] {
            let expected = AudioAnalysis {
                duration_sec: 180,
                pcm_hash: "ffffffffffffffff".into(),
                fingerprint: fingerprint.clone(),
            };
            let fast = AudioFastFingerprint {
                duration_sec: 180,
                fingerprint,
            };
            let mut store = Store::open(&fixture.path()).unwrap();
            store.db.execute("DELETE FROM entries", &[]).unwrap();
            put(&mut store, &key, &expected);
            assert!(store.read::<AudioFastFingerprint>(&key).is_err());
            assert!(store.read::<AudioDuration>(&key).is_err());
            put(&mut store, &key, &fast);
            put(&mut store, &key, &AudioDuration(180));
            drop(store);
            let store = Store::open(&fixture.path()).unwrap();
            assert_eq!(store.read::<AudioAnalysis>(&key).unwrap(), expected);
            assert_eq!(store.read::<AudioFastFingerprint>(&key).unwrap(), fast);
            assert_eq!(
                store.read::<AudioDuration>(&key).unwrap(),
                AudioDuration(180)
            );
            assert_eq!(
                store
                    .db
                    .query_row(
                        "SELECT count(*) FROM entries WHERE (slot=10 AND pcm_hash IS NULL)
                     OR (slot=11 AND pcm_hash IS NOT NULL)",
                        &[],
                        |r| r.get::<i64>(0),
                    )
                    .unwrap(),
                0,
            );
        }
    }

    #[test]
    fn results_survive_reopening_with_all_hash_bits_and_nullable_fields() {
        let fixture = Fixture::new();
        let key = fixture.key();
        let values = [
            CachedMedia::Image(Arc::new(ImageAnalysis {
                width: 6000,
                height: 4000,
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
            .query_row("SELECT count(*) FROM entries", &[], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        store
            .db
            .execute("UPDATE entries SET version=99", &[])
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
            .write(&key, u32::SLOT, 43u32.columns().unwrap(), true)
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
    fn quick_writes_do_not_evict_and_background_writes_can_recover() {
        let fixture = Fixture::new();
        let mut store = Store::open(&fixture.path()).unwrap();
        store.db.pragma_update_i64("max_page_count", 512).unwrap();
        let mut key = fixture.key();
        let tags = crate::exif::ExifInfo {
            make: Some("x".repeat(32 * 1024)),
            ..Default::default()
        };
        let mut failed = false;
        for i in 0u32..150 {
            key.path = i.to_le_bytes().to_vec();
            if store
                .write(
                    &key,
                    <crate::exif::ExifInfo as DiskValue>::SLOT,
                    tags.columns().unwrap(),
                    false,
                )
                .is_err()
            {
                failed = true;
                break;
            }
        }
        assert!(failed);
        let mut oldest = fixture.key();
        oldest.path = 0u32.to_le_bytes().to_vec();
        assert_eq!(store.read::<crate::exif::ExifInfo>(&oldest).unwrap(), tags);
        // A background call can evict and retain the result that did not fit on the UI path.
        put(&mut store, &key, &tags);
        assert_eq!(store.read::<crate::exif::ExifInfo>(&key).unwrap(), tags);
    }

    #[test]
    fn quick_writes_defer_at_the_conservative_row_bound_without_scanning() {
        let fixture = Fixture::new();
        let mut store = Store::open(&fixture.path()).unwrap();
        let key = fixture.key();
        put(&mut store, &key, &42u32);
        // Sparse rowids can overestimate the count: defer rather than scan from a quick call.
        store
            .db
            .execute("UPDATE entries SET rowid=?1", &[&MAX_ENTRIES])
            .unwrap();
        let mut next = fixture.key();
        next.path.push(1);
        store
            .write(&next, u32::SLOT, 43u32.columns().unwrap(), false)
            .unwrap();
        assert!(store.read::<u32>(&next).is_err());
        assert_eq!(store.read::<u32>(&key).unwrap(), 42);
        put(&mut store, &next, &43u32);
        store.maintain().unwrap();
        assert_eq!(store.read::<u32>(&next).unwrap(), 43);
        assert_eq!(store.writes, 0);
    }

    #[test]
    fn byte_limit_evicts_records_before_the_row_limit_and_keeps_accepting_writes() {
        let fixture = Fixture::new();
        let mut store = Store::open(&fixture.path()).unwrap();
        store.db.pragma_update_i64("max_page_count", 512).unwrap(); // 2 MiB test limit.
        let mut key = fixture.key();
        let tags = crate::exif::ExifInfo {
            make: Some("x".repeat(32 * 1024)),
            ..Default::default()
        };
        // SQLITE_FULL recovery must work even before the next scheduled maintenance.
        for i in 0u32..150 {
            key.path = i.to_le_bytes().to_vec();
            put(&mut store, &key, &tags);
        }
        assert_eq!(store.read::<crate::exif::ExifInfo>(&key).unwrap(), tags);
        let count: i64 = store
            .db
            .query_row("SELECT count(*) FROM entries", &[], |r| r.get(0))
            .unwrap();
        assert!(count < 150);
        let pages = store.db.pragma_query_i64("page_count").unwrap();
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
        db.pragma_update_i64("user_version", 99).unwrap();
        assert!(Store::open(&unknown).is_err());
        assert_eq!(db.pragma_query_i64("user_version").unwrap(), 99);
        assert!(Store::open(&fixture.0).is_err());
        assert!(Store::open(&fixture.0.join("С„РѕС‚Рѕ рџ¦Љ.jpg/cache.sqlite3")).is_err());
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
            .query_row("SELECT count(*) FROM entries", &[], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 4);
        let pages = store.db.pragma_query_i64("page_count").unwrap();
        store
            .db
            .pragma_update_and_check_i64("max_page_count", pages)
            .unwrap();
        let huge = "x".repeat((pages * 4096) as usize);
        let value = AudioAnalysis {
            duration_sec: 1,
            pcm_hash: huge,
            fingerprint: None,
        };
        assert!(store
            .write(&key, AudioAnalysis::SLOT, value.columns().unwrap(), true)
            .is_err());
        assert_eq!(store.read::<u32>(&key).unwrap(), 8999);
    }

    #[test]
    fn missing_timestamps_and_relative_paths_are_not_persisted() {
        let fixture = Fixture::new();
        let mut key = FileKey::stat(&fixture.0.join("С„РѕС‚Рѕ рџ¦Љ.jpg")).unwrap();
        key.modified = None;
        assert!(Key::new(&key).is_none());
        key.modified = Some(UNIX_EPOCH);
        key.path = "relative.jpg".into();
        assert!(Key::new(&key).is_none());
    }
}
