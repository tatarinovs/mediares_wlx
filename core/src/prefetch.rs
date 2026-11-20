//! Bounded read-ahead for WDX hashes, fingerprints and sharpness only.
//! Directory enumeration never runs on TC's thread.
//! Workers are joined by ContentPluginUnloading before any DLL code can be unmapped.

use std::fs::ReadDir;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Instant;

mod adaptive;
use adaptive::Controller;

use crate::cache::{AudioAnalysis, AudioFastFingerprint, CachedMedia};
use crate::disk_cache::DiskValue;
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

#[derive(PartialEq, Eq)]
struct Workload {
    volume: PathBuf,
    source: Source,
    kind: MediaType,
    extension: String,
}

impl Workload {
    fn new(parent: &Path, source: Source, kind: MediaType, extension: String) -> Self {
        let volume = match parent.components().next() {
            Some(Component::Prefix(prefix)) => {
                let mut volume = prefix.as_os_str().to_os_string();
                volume.make_ascii_lowercase();
                volume.into()
            }
            _ => parent.into(),
        };
        Self {
            volume,
            source,
            kind,
            extension,
        }
    }
}

#[derive(Default)]
struct State {
    lanes: Vec<Pending>,
    stopping: bool,
    next: usize,
    active_workers: usize,
    generation: u64,
    controller: Option<Controller>,
    workload: Option<Workload>,
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
        #[cfg(feature = "profile-stats")]
        if let Some(n) = std::env::var("MEDIARES_PREFETCH_THREADS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        {
            return n;
        }
        let available = thread::available_parallelism().map_or(1, usize::from);
        crate::config::prefetch_threads().min(available.saturating_sub(1))
    })
}

impl Pool {
    fn new(count: usize) -> Self {
        let shared = Arc::new(Shared::default());
        let fixed = {
            #[cfg(feature = "profile-stats")]
            if std::env::var("MEDIARES_PREFETCH_THREADS").is_ok() {
                Some(count)
            } else {
                crate::config::fixed_prefetch_threads()
            }
            #[cfg(not(feature = "profile-stats"))]
            crate::config::fixed_prefetch_threads()
        };
        let controller = Controller::new(count, fixed.is_some());
        if controller.enabled() {
            *OBSERVER.get_or_init(Default::default).lock().unwrap() = Arc::downgrade(&shared);
        }
        shared.state.lock().unwrap().controller = Some(controller);
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

    fn request(&self, path: &Path, field: Field, kind: MediaType) -> Option<Sample> {
        let parent = path.parent()?;
        let extension = extension(path)?;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopping {
            return None;
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
                let wake = pending.allowance < LOOKAHEAD;
                pending.allowance = LOOKAHEAD;
                if wake {
                    self.shared.wake.notify_all();
                }
            }
        } else {
            let workload = Workload::new(parent, field.source(), kind, extension.clone());
            if state.workload.as_ref() != Some(&workload) {
                state.generation = state.generation.wrapping_add(1);
                if let Some(c) = &mut state.controller {
                    c.reset();
                }
                state.workload = Some(workload);
            }
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
            self.shared.wake.notify_all();
        }
        let controller = state.controller.as_mut().filter(|c| c.enabled())?;
        let now = Instant::now();
        controller.resume(now);
        Some(Sample {
            shared: self.shared.clone(),
            generation: state.generation,
            started: now,
        })
    }

    fn cancel(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        cancel_lanes(&mut state);
        if let Some(c) = &mut state.controller {
            c.interrupt(Instant::now());
        }
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
    state.generation = state.generation.wrapping_add(1);
    // The controller measured the combined load when there were multiple lanes.
    if state.lanes.len() > 1 {
        state.workload = None;
    }
    for pending in state.lanes.drain(..) {
        pending.lane.cancelled.store(true, Ordering::Relaxed);
    }
}

struct WorkerGuard<'a>(&'a Shared);

impl Drop for WorkerGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active_workers = state.active_workers.saturating_sub(1);
        if state.active_workers == 0 && !state.lanes.iter().any(|p| p.allowance > 0 && !p.exhausted)
        {
            if let Some(c) = &mut state.controller {
                c.idle();
            }
        }
        self.0.wake.notify_all();
    }
}

// Weak ownership keeps foreground instrumentation independent of POOL's lifecycle lock.
static OBSERVER: OnceLock<Mutex<std::sync::Weak<Shared>>> = OnceLock::new();

pub(crate) struct Sample {
    shared: Arc<Shared>,
    generation: u64,
    started: Instant,
}

pub(crate) fn begin_sample(path: &Path, slot: i32) -> Option<Sample> {
    let shared = OBSERVER.get()?.lock().ok()?.upgrade()?;
    let state = shared.state.lock().ok()?;
    if !state.controller.as_ref()?.enabled() {
        return None;
    }
    let extension = extension(path)?;
    if !state.lanes.iter().any(|p| {
        let source_slot = match p.lane.field.source() {
            Source::Analysis => CachedMedia::SLOT,
            Source::Sharpness => u32::SLOT,
            Source::AudioAnalysis => AudioAnalysis::SLOT,
            Source::AudioFastFingerprint => AudioFastFingerprint::SLOT,
            _ => return false,
        };
        source_slot == slot
            && path.parent() == Some(p.lane.parent.as_path())
            && extension == p.lane.extension
    }) {
        return None;
    }
    let generation = state.generation;
    drop(state);
    Some(Sample {
        shared,
        generation,
        started: Instant::now(),
    })
}

impl Sample {
    pub(crate) fn complete(self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation != self.generation {
            return;
        }
        let active = state.active_workers;
        if let Some(c) = &mut state.controller {
            c.complete(self.started, Instant::now(), active);
        }
        self.shared.wake.notify_all();
    }

    pub(crate) fn complete_foreground(self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation == self.generation {
            if let Some(c) = &mut state.controller {
                c.foreground(self.started, Instant::now());
            }
        }
    }
}

fn run(shared: Arc<Shared>) {
    loop {
        let (lane, _worker_guard) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopping {
                    return;
                }
                let n = state.lanes.len();
                if let Some(i) = (0..n).map(|i| (state.next + i) % n).find(|&i| {
                    state.lanes[i].allowance > 0
                        && !state.lanes[i].exhausted
                        && state.active_workers < state.controller.as_ref().map_or(0, |c| c.limit())
                }) {
                    state.next = (i + 1) % n;
                    state.lanes[i].allowance -= 1;
                    state.active_workers += 1;
                    break (state.lanes[i].lane.clone(), WorkerGuard(&shared));
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

pub(crate) fn request(path: &Path, field: Field, kind: MediaType) -> Option<Sample> {
    // Metadata and decoded duration are cached on demand. Ordinary columns must not
    // start workers or open neighboring files, even when reading them can be slow.
    if !matches!(
        field.source(),
        Source::Analysis | Source::Sharpness | Source::AudioAnalysis | Source::AudioFastFingerprint
    ) {
        return None;
    }
    let count = worker_count();
    if count == 0 || crate::wdx_api::is_stop_requested() {
        return None;
    }
    let mut pool = POOL
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let pool = pool.get_or_insert_with(|| Pool::new(count));
    pool.request(path, field, kind)
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
