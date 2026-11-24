//! Minimal RAII SQLite wrapper backed by Windows' built-in `winsqlite3.dll`.
//! Loads the OS library lazily from `System32` on first database open and binds only
//! baseline SQLite 3.7+ entry points so Windows 10 (1607+) and Windows 11 work alike.

use std::ffi::{c_char, c_int, c_void, CString};
use std::marker::PhantomData;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use windows::core::{s, w};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

const SQLITE_OK: c_int = 0;
const SQLITE_FULL: c_int = 13;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_NULL: c_int = 5;

const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
const SQLITE_OPEN_FULLMUTEX: c_int = 0x0001_0000;

type Destructor = isize;
const SQLITE_TRANSIENT: Destructor = -1;

#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
struct sqlite3_stmt {
    _private: [u8; 0],
}

#[derive(Debug)]
pub enum Error {
    Unavailable,
    InvalidPath,
    InvalidQuery,
    QueryReturnedNoRows,
    TypeMismatch,
    Sqlite(c_int),
}

impl Error {
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Sqlite(code) if (*code & 0xff) == SQLITE_FULL)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Entry points resolved from `System32\winsqlite3.dll` (kept loaded for process lifetime).
struct Api {
    open_v2:
        unsafe extern "system" fn(*const c_char, *mut *mut sqlite3, c_int, *const c_char) -> c_int,
    close_v2: unsafe extern "system" fn(*mut sqlite3) -> c_int,
    busy_timeout: unsafe extern "system" fn(*mut sqlite3, c_int) -> c_int,
    exec: unsafe extern "system" fn(
        *mut sqlite3,
        *const c_char,
        Option<
            unsafe extern "system" fn(
                *mut c_void,
                c_int,
                *mut *mut c_char,
                *mut *mut c_char,
            ) -> c_int,
        >,
        *mut c_void,
        *mut *mut c_char,
    ) -> c_int,
    prepare_v2: unsafe extern "system" fn(
        *mut sqlite3,
        *const c_char,
        c_int,
        *mut *mut sqlite3_stmt,
        *mut *const c_char,
    ) -> c_int,
    step: unsafe extern "system" fn(*mut sqlite3_stmt) -> c_int,
    finalize: unsafe extern "system" fn(*mut sqlite3_stmt) -> c_int,
    reset: unsafe extern "system" fn(*mut sqlite3_stmt) -> c_int,
    clear_bindings: unsafe extern "system" fn(*mut sqlite3_stmt) -> c_int,
    bind_blob: unsafe extern "system" fn(
        *mut sqlite3_stmt,
        c_int,
        *const c_void,
        c_int,
        Destructor,
    ) -> c_int,
    bind_double: unsafe extern "system" fn(*mut sqlite3_stmt, c_int, f64) -> c_int,
    bind_int: unsafe extern "system" fn(*mut sqlite3_stmt, c_int, c_int) -> c_int,
    bind_int64: unsafe extern "system" fn(*mut sqlite3_stmt, c_int, i64) -> c_int,
    bind_null: unsafe extern "system" fn(*mut sqlite3_stmt, c_int) -> c_int,
    bind_text: unsafe extern "system" fn(
        *mut sqlite3_stmt,
        c_int,
        *const c_char,
        c_int,
        Destructor,
    ) -> c_int,
    column_type: unsafe extern "system" fn(*mut sqlite3_stmt, c_int) -> c_int,
    column_int64: unsafe extern "system" fn(*mut sqlite3_stmt, c_int) -> i64,
    column_double: unsafe extern "system" fn(*mut sqlite3_stmt, c_int) -> f64,
    column_bytes: unsafe extern "system" fn(*mut sqlite3_stmt, c_int) -> c_int,
    column_blob: unsafe extern "system" fn(*mut sqlite3_stmt, c_int) -> *const c_void,
    changes: unsafe extern "system" fn(*mut sqlite3) -> c_int,
    #[cfg(test)]
    get_autocommit: unsafe extern "system" fn(*mut sqlite3) -> c_int,
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe { load_api() })
        .as_ref()
        .ok_or(Error::Unavailable)
}

unsafe fn load_api() -> Option<Api> {
    let module = LoadLibraryExW(w!("winsqlite3.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
    macro_rules! entry {
        ($name:literal) => {{
            let f = GetProcAddress(module, s!($name))?;
            std::mem::transmute_copy(&f)
        }};
    }
    Some(Api {
        open_v2: entry!("sqlite3_open_v2"),
        close_v2: entry!("sqlite3_close_v2"),
        busy_timeout: entry!("sqlite3_busy_timeout"),
        exec: entry!("sqlite3_exec"),
        prepare_v2: entry!("sqlite3_prepare_v2"),
        step: entry!("sqlite3_step"),
        finalize: entry!("sqlite3_finalize"),
        reset: entry!("sqlite3_reset"),
        clear_bindings: entry!("sqlite3_clear_bindings"),
        bind_blob: entry!("sqlite3_bind_blob"),
        bind_double: entry!("sqlite3_bind_double"),
        bind_int: entry!("sqlite3_bind_int"),
        bind_int64: entry!("sqlite3_bind_int64"),
        bind_null: entry!("sqlite3_bind_null"),
        bind_text: entry!("sqlite3_bind_text"),
        column_type: entry!("sqlite3_column_type"),
        column_int64: entry!("sqlite3_column_int64"),
        column_double: entry!("sqlite3_column_double"),
        column_bytes: entry!("sqlite3_column_bytes"),
        column_blob: entry!("sqlite3_column_blob"),
        changes: entry!("sqlite3_changes"),
        #[cfg(test)]
        get_autocommit: entry!("sqlite3_get_autocommit"),
    })
}

fn check(rc: c_int) -> Result<()> {
    if rc == SQLITE_OK {
        Ok(())
    } else {
        Err(Error::Sqlite(rc))
    }
}

struct ConnInner {
    api: &'static Api,
    db: *mut sqlite3,
}

// SQLite is opened in Serialized mode (SQLITE_OPEN_FULLMUTEX), making cross-thread handoff sound.
unsafe impl Send for ConnInner {}
unsafe impl Sync for ConnInner {}

impl Drop for ConnInner {
    fn drop(&mut self) {
        if !self.db.is_null() {
            unsafe { (self.api.close_v2)(self.db) };
        }
    }
}

/// Owned SQLite database connection backed by `winsqlite3.dll`.
pub struct Connection {
    inner: Arc<ConnInner>,
    _not_sync: PhantomData<*mut ()>,
}

unsafe impl Send for Connection {}

impl Connection {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let api = api()?;
        let path_str = path.as_ref().to_str().ok_or(Error::InvalidPath)?;
        let c_path = CString::new(path_str).map_err(|_| Error::InvalidPath)?;
        let mut db = std::ptr::null_mut();
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX;
        let rc = unsafe { (api.open_v2)(c_path.as_ptr(), &mut db, flags, std::ptr::null()) };
        if rc != SQLITE_OK {
            if !db.is_null() {
                unsafe { (api.close_v2)(db) };
            }
            return Err(Error::Sqlite(rc));
        }
        Ok(Self {
            inner: Arc::new(ConnInner { api, db }),
            _not_sync: PhantomData,
        })
    }

    pub fn busy_timeout(&self, timeout: Duration) -> Result<()> {
        let ms = c_int::try_from(timeout.as_millis()).unwrap_or(c_int::MAX);
        check(unsafe { (self.inner.api.busy_timeout)(self.inner.db, ms) })
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let c_sql = CString::new(sql).map_err(|_| Error::InvalidQuery)?;
        check(unsafe {
            (self.inner.api.exec)(
                self.inner.db,
                c_sql.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        })
    }

    pub fn prepare(&self, sql: &str) -> Result<Statement> {
        let len = c_int::try_from(sql.len()).map_err(|_| Error::InvalidQuery)?;
        let mut stmt = std::ptr::null_mut();
        check(unsafe {
            (self.inner.api.prepare_v2)(
                self.inner.db,
                sql.as_ptr().cast(),
                len,
                &mut stmt,
                std::ptr::null_mut(),
            )
        })?;
        if stmt.is_null() {
            return Err(Error::InvalidQuery);
        }
        Ok(Statement {
            conn: Arc::clone(&self.inner),
            stmt,
            _not_sync: PhantomData,
        })
    }

    pub fn execute(&self, sql: &str, params: &[&dyn ToSql]) -> Result<usize> {
        self.prepare(sql)?.execute(params)
    }

    pub fn query_row<T>(
        &self,
        sql: &str,
        params: &[&dyn ToSql],
        f: impl FnOnce(&Row<'_>) -> Result<T>,
    ) -> Result<T> {
        self.prepare(sql)?.query_row(params, f)
    }

    pub fn query_row_optional<T>(
        &self,
        sql: &str,
        params: &[&dyn ToSql],
        f: impl FnOnce(&Row<'_>) -> Result<T>,
    ) -> Result<Option<T>> {
        match self.query_row(sql, params, f) {
            Ok(v) => Ok(Some(v)),
            Err(Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn pragma_query_i64(&self, name: &str) -> Result<i64> {
        self.query_row(&format!("PRAGMA {name}"), &[], |r| r.get(0))
    }

    pub fn pragma_update_i64(&self, name: &str, value: i64) -> Result<()> {
        self.execute_batch(&format!("PRAGMA {name}={value};"))
    }

    pub fn pragma_update_and_check_i64(&self, name: &str, value: i64) -> Result<i64> {
        self.query_row(&format!("PRAGMA {name}={value}"), &[], |r| r.get(0))
    }

    pub fn transaction_immediate(&mut self) -> Result<Transaction<'_>> {
        self.execute_batch("BEGIN IMMEDIATE;")?;
        Ok(Transaction {
            conn: self,
            committed: false,
        })
    }

    #[cfg(test)]
    pub fn is_autocommit(&self) -> bool {
        unsafe { (self.inner.api.get_autocommit)(self.inner.db) != 0 }
    }
}

/// Immediate SQLite write transaction that rolls back automatically unless committed.
pub struct Transaction<'conn> {
    conn: &'conn Connection,
    committed: bool,
}

impl Transaction<'_> {
    pub fn execute(&self, sql: &str, params: &[&dyn ToSql]) -> Result<usize> {
        self.conn.execute(sql, params)
    }

    pub fn query_row<T>(
        &self,
        sql: &str,
        params: &[&dyn ToSql],
        f: impl FnOnce(&Row<'_>) -> Result<T>,
    ) -> Result<T> {
        self.conn.query_row(sql, params, f)
    }

    pub fn commit(mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT;")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.conn.execute_batch("ROLLBACK;");
        }
    }
}

/// Reusable prepared SQLite statement keeping its parent connection alive via `Arc`.
pub struct Statement {
    conn: Arc<ConnInner>,
    stmt: *mut sqlite3_stmt,
    _not_sync: PhantomData<*mut ()>,
}

unsafe impl Send for Statement {}

impl Drop for Statement {
    fn drop(&mut self) {
        if !self.stmt.is_null() {
            unsafe { (self.conn.api.finalize)(self.stmt) };
        }
    }
}

struct ResetGuard<'a>(&'a Statement);

impl Drop for ResetGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.0.conn.api.reset)(self.0.stmt);
            (self.0.conn.api.clear_bindings)(self.0.stmt);
        }
    }
}

impl Statement {
    fn bind_all(&self, params: &[&dyn ToSql]) -> Result<()> {
        for (i, param) in params.iter().enumerate() {
            let idx = c_int::try_from(i + 1).map_err(|_| Error::InvalidQuery)?;
            param.bind_to(self, idx)?;
        }
        Ok(())
    }

    pub fn execute(&self, params: &[&dyn ToSql]) -> Result<usize> {
        let _guard = ResetGuard(self);
        self.bind_all(params)?;
        let rc = unsafe { (self.conn.api.step)(self.stmt) };
        if rc != SQLITE_DONE && rc != SQLITE_ROW {
            return Err(Error::Sqlite(rc));
        }
        let n = unsafe { (self.conn.api.changes)(self.conn.db) };
        Ok(n.max(0) as usize)
    }

    pub fn query_row<T>(
        &self,
        params: &[&dyn ToSql],
        f: impl FnOnce(&Row<'_>) -> Result<T>,
    ) -> Result<T> {
        let _guard = ResetGuard(self);
        self.bind_all(params)?;
        let rc = unsafe { (self.conn.api.step)(self.stmt) };
        match rc {
            SQLITE_ROW => f(&Row {
                api: self.conn.api,
                stmt: self.stmt,
                _marker: PhantomData,
            }),
            SQLITE_DONE => Err(Error::QueryReturnedNoRows),
            _ => Err(Error::Sqlite(rc)),
        }
    }

    #[cfg(test)]
    pub fn query_map<T>(
        &self,
        params: &[&dyn ToSql],
        mut f: impl FnMut(&Row<'_>) -> Result<T>,
    ) -> Result<Vec<T>> {
        let _guard = ResetGuard(self);
        self.bind_all(params)?;
        let mut out = Vec::new();
        loop {
            match unsafe { (self.conn.api.step)(self.stmt) } {
                SQLITE_ROW => out.push(f(&Row {
                    api: self.conn.api,
                    stmt: self.stmt,
                    _marker: PhantomData,
                })?),
                SQLITE_DONE => return Ok(out),
                rc => return Err(Error::Sqlite(rc)),
            }
        }
    }
}

/// Zero-allocation view of the current SQLite result row.
pub struct Row<'stmt> {
    api: &'static Api,
    stmt: *mut sqlite3_stmt,
    _marker: PhantomData<&'stmt sqlite3_stmt>,
}

impl Row<'_> {
    pub fn get<T: FromSql>(&self, col: usize) -> Result<T> {
        let idx = c_int::try_from(col).map_err(|_| Error::InvalidQuery)?;
        T::from_col(self, idx)
    }

    pub fn column_bytes(&self, col: usize) -> Option<usize> {
        let idx = c_int::try_from(col).ok()?;
        let kind = unsafe { (self.api.column_type)(self.stmt, idx) };
        if kind == SQLITE_NULL {
            return None;
        }
        let len = unsafe { (self.api.column_bytes)(self.stmt, idx) };
        usize::try_from(len).ok()
    }
}

pub trait ToSql {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()>;
}

fn non_null_bytes(bytes: &[u8]) -> Result<(*const c_void, c_int)> {
    let len = c_int::try_from(bytes.len()).map_err(|_| Error::InvalidQuery)?;
    // Pass a non-null pointer for empty slices so SQLite stores empty text/blob, not NULL.
    let ptr = if bytes.is_empty() {
        std::ptr::NonNull::<u8>::dangling().as_ptr().cast()
    } else {
        bytes.as_ptr().cast()
    };
    Ok((ptr, len))
}

impl ToSql for [u8] {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()> {
        let (ptr, len) = non_null_bytes(self)?;
        check(unsafe { (stmt.conn.api.bind_blob)(stmt.stmt, idx, ptr, len, SQLITE_TRANSIENT) })
    }
}

impl ToSql for Vec<u8> {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()> {
        self.as_slice().bind_to(stmt, idx)
    }
}

impl ToSql for str {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()> {
        let (ptr, len) = non_null_bytes(self.as_bytes())?;
        check(unsafe {
            (stmt.conn.api.bind_text)(stmt.stmt, idx, ptr.cast(), len, SQLITE_TRANSIENT)
        })
    }
}

impl ToSql for String {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()> {
        self.as_str().bind_to(stmt, idx)
    }
}

impl ToSql for i32 {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()> {
        check(unsafe { (stmt.conn.api.bind_int)(stmt.stmt, idx, *self) })
    }
}

impl ToSql for i64 {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()> {
        check(unsafe { (stmt.conn.api.bind_int64)(stmt.stmt, idx, *self) })
    }
}

impl ToSql for u32 {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()> {
        i64::from(*self).bind_to(stmt, idx)
    }
}

impl ToSql for f64 {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()> {
        check(unsafe { (stmt.conn.api.bind_double)(stmt.stmt, idx, *self) })
    }
}

impl<T: ToSql + ?Sized> ToSql for &T {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()> {
        (**self).bind_to(stmt, idx)
    }
}

impl<T: ToSql> ToSql for Option<T> {
    fn bind_to(&self, stmt: &Statement, idx: c_int) -> Result<()> {
        match self {
            Some(v) => v.bind_to(stmt, idx),
            None => check(unsafe { (stmt.conn.api.bind_null)(stmt.stmt, idx) }),
        }
    }
}

pub trait FromSql: Sized {
    fn from_col(row: &Row<'_>, col: c_int) -> Result<Self>;
}

impl FromSql for i64 {
    fn from_col(row: &Row<'_>, col: c_int) -> Result<Self> {
        if unsafe { (row.api.column_type)(row.stmt, col) } != SQLITE_INTEGER {
            return Err(Error::TypeMismatch);
        }
        Ok(unsafe { (row.api.column_int64)(row.stmt, col) })
    }
}

impl FromSql for i32 {
    fn from_col(row: &Row<'_>, col: c_int) -> Result<Self> {
        Self::try_from(i64::from_col(row, col)?).map_err(|_| Error::TypeMismatch)
    }
}

impl FromSql for u32 {
    fn from_col(row: &Row<'_>, col: c_int) -> Result<Self> {
        Self::try_from(i64::from_col(row, col)?).map_err(|_| Error::TypeMismatch)
    }
}

impl FromSql for f64 {
    fn from_col(row: &Row<'_>, col: c_int) -> Result<Self> {
        match unsafe { (row.api.column_type)(row.stmt, col) } {
            SQLITE_FLOAT => Ok(unsafe { (row.api.column_double)(row.stmt, col) }),
            SQLITE_INTEGER => Ok(unsafe { (row.api.column_int64)(row.stmt, col) } as f64),
            _ => Err(Error::TypeMismatch),
        }
    }
}

impl FromSql for String {
    fn from_col(row: &Row<'_>, col: c_int) -> Result<Self> {
        if unsafe { (row.api.column_type)(row.stmt, col) } != SQLITE_TEXT {
            return Err(Error::TypeMismatch);
        }
        let ptr = unsafe { (row.api.column_blob)(row.stmt, col) }.cast::<u8>();
        let len = unsafe { (row.api.column_bytes)(row.stmt, col) };
        if len <= 0 {
            return Ok(String::new());
        }
        if ptr.is_null() {
            return Err(Error::TypeMismatch);
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| Error::TypeMismatch)
    }
}

impl<T: FromSql> FromSql for Option<T> {
    fn from_col(row: &Row<'_>, col: c_int) -> Result<Self> {
        if unsafe { (row.api.column_type)(row.stmt, col) } == SQLITE_NULL {
            return Ok(None);
        }
        T::from_col(row, col).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_reuse_empty_and_transaction_rollback() {
        let mut db = Connection::open(":memory:").unwrap();
        db.execute_batch("CREATE TABLE t (k BLOB PRIMARY KEY, s TEXT, n INTEGER, f REAL);")
            .unwrap();
        let insert = db
            .prepare("INSERT OR REPLACE INTO t (k, s, n, f) VALUES (?1, ?2, ?3, ?4)")
            .unwrap();
        let select = db
            .prepare("SELECT k, s, n, f FROM t WHERE rowid=?1")
            .unwrap();

        insert
            .execute(&[&b"".as_slice(), &"", &i64::MIN, &1.25f64])
            .unwrap();
        let (s, n, f): (String, i64, f64) = select
            .query_row(&[&1i64], |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap();
        assert_eq!(s, "");
        assert_eq!(n, i64::MIN);
        assert_eq!(f, 1.25);

        {
            let tx = db.transaction_immediate().unwrap();
            assert!(!tx.conn.is_autocommit());
            tx.execute(
                "INSERT INTO t (k, s, n, f) VALUES (?1, ?2, ?3, ?4)",
                &[&b"x".as_slice(), &None::<String>, &42i32, &0.0f64],
            )
            .unwrap();
        }
        assert!(db.is_autocommit());
        assert!(matches!(
            select.query_row(&[&2i64], |r| r.get::<i64>(2)),
            Err(Error::QueryReturnedNoRows)
        ));
    }
}
