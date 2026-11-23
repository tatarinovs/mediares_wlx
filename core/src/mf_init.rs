//! COM / Media Foundation lifetime management.
//!
//! COM is initialized per call and balanced on drop, so plugin calls never leave a host thread
//! in an apartment it did not choose. `MFStartup` is process-wide and happens once.

use std::path::Path;
use std::sync::Mutex;

use windows::core::{GUID, HSTRING};
use windows::Win32::Media::MediaFoundation::{
    IMFByteStream, MFCreateMFByteStreamOnStream, MFStartup, MFSTARTUP_NOSOCKET, MF_VERSION,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED, STGM_READ, STGM_SHARE_DENY_NONE,
};
use windows::Win32::System::Variant::{VT_CLSID, VT_I8, VT_LPWSTR, VT_UI2, VT_UI4, VT_UI8};
use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;

/// Opens a local file for reading while allowing another process to keep it open for writing.
/// The calling thread must have initialized COM and Media Foundation.
pub fn open_shared_stream(path: &Path) -> windows::core::Result<IMFByteStream> {
    let name = HSTRING::from(path);
    unsafe {
        let stream =
            SHCreateStreamOnFileEx(&name, (STGM_READ | STGM_SHARE_DENY_NONE).0, 0, false, None)?;
        MFCreateMFByteStreamOnStream(&stream)
    }
}

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

/// A `PROPVARIANT` a COM call filled in, cleared on drop: the raw type has no destructor, and the
/// GUIDs, strings and arrays it may hold were allocated by COM.
#[derive(Default)]
pub struct PropVariant(pub PROPVARIANT);

impl PropVariant {
    /// A 16-bit unsigned value (a TIFF SHORT).
    pub fn u16(&self) -> Option<u16> {
        let inner = unsafe { &self.0.Anonymous.Anonymous };
        (inner.vt == VT_UI2).then(|| unsafe { inner.Anonymous.uiVal })
    }

    /// An unsigned value of up to 32 bits (a TIFF SHORT or LONG).
    pub fn u32(&self) -> Option<u32> {
        let inner = unsafe { &self.0.Anonymous.Anonymous };
        match inner.vt {
            VT_UI4 => Some(unsafe { inner.Anonymous.ulVal }),
            _ => self.u16().map(u32::from),
        }
    }

    /// A 64-bit value, signed or not (Media Foundation times and durations).
    pub fn u64(&self) -> Option<u64> {
        let inner = unsafe { &self.0.Anonymous.Anonymous };
        match inner.vt {
            VT_UI8 => Some(unsafe { inner.Anonymous.uhVal }),
            VT_I8 => u64::try_from(unsafe { inner.Anonymous.hVal }).ok(),
            _ => None,
        }
    }

    pub fn guid(&self) -> Option<GUID> {
        let inner = unsafe { &self.0.Anonymous.Anonymous };
        if inner.vt != VT_CLSID {
            return None;
        }
        unsafe { inner.Anonymous.puuid.as_ref().copied() }
    }

    pub fn string(&self) -> Option<String> {
        let inner = unsafe { &self.0.Anonymous.Anonymous };
        if inner.vt != VT_LPWSTR {
            return None;
        }
        let ptr = unsafe { inner.Anonymous.pwszVal };
        if ptr.is_null() {
            return None;
        }
        unsafe { ptr.to_string() }.ok()
    }
}

impl Drop for PropVariant {
    fn drop(&mut self) {
        let _ = unsafe { PropVariantClear(&mut self.0) };
    }
}

/// COM on this thread plus Media Foundation, for as long as the value lives; `None` if Media
/// Foundation is unavailable.
pub fn mf_scope() -> Option<ComScope> {
    let com = ComScope::new();
    ensure_mf_started().then_some(com)
}

static MF_STARTED: Mutex<bool> = Mutex::new(false);

/// Starts Media Foundation once per process. Returns false if it cannot be started.
///
/// Never shut down: `MFShutdown` while another host thread is inside Media Foundation (TC's
/// delayed-field thread when TC exits) leaves that thread blocked forever, and the host process
/// with it. At process exit there is nothing left to release.
pub fn ensure_mf_started() -> bool {
    let mut started = MF_STARTED.lock().unwrap_or_else(|e| e.into_inner());
    if !*started {
        *started = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.is_ok();
    }
    *started
}
