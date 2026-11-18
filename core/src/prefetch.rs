//! Bounded read-ahead for WDX hashes, fingerprints and sharpness only.
//! Directory enumeration never runs on TC's thread.
//! Workers are joined by ContentPluginUnloading before any DLL code can be unmapped.

use std::fs::ReadDir;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use crate::probe::{probe_file, MediaType};
use crate::wdx_api::{prefetch_value, Field, Source};

const LOOKAHEAD: usize = 32;
const MAX_SKIP: usize = 4096;

enum NextEntry {
    Match(PathBuf, Option<crate::cache::FileKey>),
    Exhausted,
    Paused,
}

struct Lane {
    parent: PathBuf,
    field: Field,
    kind: MediaType,
    extension: String,
    cancelled: AtomicBool,
    // Only workers touch the directory iterator. A slow NAS cannot block request().
    cursor: Mutex<Option<ReadDir>>,
}

struct Pending {
    lane: Arc<Lane>,
    allowance: usize,
    last: PathBuf,
    exhausted: bool,
}

#[derive(Default)]
struct State {
    lanes: Vec<Pending>,
    stopping: bool,
    next: usize,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

struct Pool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

fn worker_count() -> usize {
    // Read once per DLL lifetime, including when disabled: no INI access on each TC field call.
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        let available = thread::available_parallelism().map_or(1, usize::from);
        crate::config::prefetch_threads().min(available.saturating_sub(1))
    })
}

impl Pool {
    fn new(count: usize) -> Self {
        let shared = Arc::new(Shared::default());
        let mut threads = Vec::new();
        for i in 0..count {
            let shared = shared.clone();
            if let Ok(handle) = thread::Builder::new()
                .name(format!("mediares-prefetch-{i}"))
                .spawn(move || run(shared))
            {
                threads.push(handle);
            }
        }
        Self { shared, threads }
    }

    fn request(&self, path: &Path, field: Field, kind: MediaType) {
        let Some(parent) = path.parent() else { return };
        let Some(extension) = extension(path) else {
            return;
        };
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopping {
            return;
        }
        if state.lanes.first().is_some_and(|p| p.lane.parent != parent) {
            cancel_lanes(&mut state);
        }
        if let Some(pending) = state.lanes.iter_mut().find(|p| {
            p.lane.field.source() == field.source()
                && p.lane.kind == kind
                && p.lane.extension == extension
        }) {
            if pending.last != path && !pending.exhausted {
                pending.last = path.into();
                pending.allowance = LOOKAHEAD;
            }
        } else {
            state.lanes.push(Pending {
                lane: Arc::new(Lane {
                    parent: parent.into(),
                    field,
                    kind,
                    extension,
                    cancelled: AtomicBool::new(false),
                    cursor: Mutex::new(None),
                }),
                allowance: LOOKAHEAD,
                last: path.into(),
                exhausted: false,
            });
        }
        self.shared.wake.notify_all();
    }

    fn cancel(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        cancel_lanes(&mut state);
    }

    fn shutdown(mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopping = true;
            cancel_lanes(&mut state);
            self.shared.wake.notify_all();
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn cancel_lanes(state: &mut State) {
    for pending in state.lanes.drain(..) {
        pending.lane.cancelled.store(true, Ordering::Relaxed);
    }
}

fn run(shared: Arc<Shared>) {
    loop {
        let lane = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopping {
                    return;
                }
                let n = state.lanes.len();
                if let Some(i) = (0..n)
                    .map(|i| (state.next + i) % n)
                    .find(|&i| state.lanes[i].allowance > 0 && !state.lanes[i].exhausted)
                {
                    state.next = (i + 1) % n;
                    state.lanes[i].allowance -= 1;
                    break state.lanes[i].lane.clone();
                }
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        let cancelled = || lane.cancelled.load(Ordering::Relaxed);
        if cancelled() {
            continue;
        }
        let next = {
            let mut cursor = lane.cursor.lock().unwrap_or_else(|e| e.into_inner());
            if cancelled() {
                continue;
            }
            if cursor.is_none() {
                *cursor = std::fs::read_dir(&lane.parent).ok();
            }
            match cursor.as_mut() {
                None => NextEntry::Exhausted,
                Some(entries) => {
                    let mut skipped = 0usize;
                    loop {
                        if cancelled() {
                            break NextEntry::Paused;
                        }
                        let Some(entry) = entries.next() else {
                            break NextEntry::Exhausted;
                        };
                        let Ok(entry) = entry else {
                            continue;
                        };
                        let path = entry.path();
                        if extension(&path).as_deref() == Some(lane.extension.as_str()) {
                            let kind = probe_file(&path);
                            if kind == lane.kind && lane.field.applies_to(kind) {
                                let key = crate::cache::FileKey::from_dir_entry(&entry);
                                break NextEntry::Match(path, key);
                            }
                        }
                        skipped += 1;
                        if skipped >= MAX_SKIP {
                            break NextEntry::Paused;
                        }
                    }
                }
            }
        };
        match next {
            NextEntry::Exhausted => {
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(p) = state.lanes.iter_mut().find(|p| Arc::ptr_eq(&p.lane, &lane)) {
                    p.exhausted = true;
                }
            }
            NextEntry::Paused => {
                if !cancelled() {
                    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(p) = state.lanes.iter_mut().find(|p| Arc::ptr_eq(&p.lane, &lane)) {
                        if !p.exhausted && p.allowance < LOOKAHEAD {
                            p.allowance += 1;
                        }
                    }
                }
            }
            NextEntry::Match(path, key) => {
                // Decoder errors/panics must not kill a worker or leave a foreground waiter behind.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    prefetch_value(&path, key, lane.field, &cancelled);
                }));
            }
        }
    }
}

static POOL: OnceLock<Mutex<Option<Pool>>> = OnceLock::new();

fn extension(path: &Path) -> Option<String> {
    path.extension()?.to_str().map(str::to_ascii_lowercase)
}

pub(crate) fn request(path: &Path, field: Field, kind: MediaType) {
    // Metadata and decoded duration are cached on demand. Ordinary columns must not
    // start workers or open neighboring files, even when reading them can be slow.
    if !matches!(
        field.source(),
        Source::Analysis
            | Source::Sharpness
            | Source::AudioAnalysis
            | Source::AudioFastFingerprint
    ) {
        return;
    }
    let count = worker_count();
    if count == 0 || crate::wdx_api::is_stop_requested() {
        return;
    }
    let mut pool = POOL
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let pool = pool.get_or_insert_with(|| Pool::new(count));
    pool.request(path, field, kind);
}

pub(crate) fn cancel() {
    if let Some(pool) = POOL.get() {
        if let Some(pool) = pool.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            pool.cancel();
        }
    }
}

pub(crate) fn shutdown() {
    if let Some(pool) = POOL.get() {
        let pool = pool.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(pool) = pool {
            pool.shutdown();
        }
    }
}
