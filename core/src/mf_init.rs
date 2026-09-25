//! COM / Media Foundation lifetime management.
//!
//! COM is initialized per call and balanced on drop, so plugin calls never leave a host thread
//! in an apartment it did not choose. `MFStartup` is process-wide and happens once.

use std::sync::Mutex;

use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_NOSOCKET, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// Initializes COM on the current thread for the lifetime of the value.
/// If the thread already has a different apartment, COM is usable as-is and nothing is undone.
pub struct ComScope {
    initialized: bool,
}

impl ComScope {
    pub fn new() -> Self {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        ComScope { initialized }
    }
}

impl Default for ComScope {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

/// Keeps Media Foundation started for the lifetime of the value (MFStartup is reference-counted),
/// e.g. for a player that outlives a single call.
pub struct MfSession(());

impl MfSession {
    pub fn start() -> Option<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.ok()?;
        Some(MfSession(()))
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

static MF_STARTED: Mutex<bool> = Mutex::new(false);

/// Starts Media Foundation once per process. Returns false if it cannot be started.
pub fn ensure_mf_started() -> bool {
    let mut started = MF_STARTED.lock().unwrap_or_else(|e| e.into_inner());
    if !*started {
        *started = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.is_ok();
    }
    *started
}

/// Balances [`ensure_mf_started`]; called when the plugin is unloaded.
pub fn shutdown_mf() {
    let mut started = MF_STARTED.lock().unwrap_or_else(|e| e.into_inner());
    if *started {
        let _ = unsafe { MFShutdown() };
        *started = false;
    }
}
