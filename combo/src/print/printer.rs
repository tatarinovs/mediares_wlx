//! Own printer resources and commit settings only after the driver accepts them.

use std::mem::size_of;
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Printing::*;
use windows::Win32::Storage::Xps::{
    DeviceCapabilitiesW, DC_COPIES, DC_PAPERNAMES, DC_PAPERS, DC_PAPERSIZE,
};

use super::layout::Geometry;

struct PrinterHandle(PRINTER_HANDLE);
impl PrinterHandle {
    unsafe fn open(name: &HSTRING) -> Option<Self> {
        let mut handle = PRINTER_HANDLE::default();
        OpenPrinterW(PCWSTR(name.as_ptr()), &mut handle, None).ok()?;
        Some(Self(handle))
    }
}
impl Drop for PrinterHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = ClosePrinter(self.0);
        }
    }
}

/// Aligned, initialized storage for both the public DEVMODE and the driver's private bytes.
#[derive(Clone)]
struct Mode(Vec<u64>);
impl Mode {
    fn new(bytes: usize) -> Option<Self> {
        (bytes >= size_of::<DEVMODEW>()).then(|| Self(vec![0; bytes.div_ceil(size_of::<u64>())]))
    }
    fn ptr(&self) -> *const DEVMODEW {
        self.0.as_ptr().cast()
    }
    fn ptr_mut(&mut self) -> *mut DEVMODEW {
        self.0.as_mut_ptr().cast()
    }
    fn get(&self) -> &DEVMODEW {
        unsafe { &*self.ptr() }
    }
    fn get_mut(&mut self) -> &mut DEVMODEW {
        unsafe { &mut *self.ptr_mut() }
    }
    fn valid(&self) -> bool {
        let mode = self.get();
        mode.dmSize as usize >= size_of::<DEVMODEW>()
            && mode.dmSize as usize + mode.dmDriverExtra as usize <= self.0.len() * size_of::<u64>()
    }
    unsafe fn normalize(&mut self, name: &HSTRING, handle: &PrinterHandle) -> bool {
        let ptr = self.ptr_mut();
        DocumentPropertiesW(
            None,
            handle.0,
            PCWSTR(name.as_ptr()),
            Some(ptr),
            Some(ptr),
            (DM_IN_BUFFER | DM_OUT_BUFFER).0,
        ) == 1
            && self.valid()
    }
    fn set_copies(&mut self, copies: u32) {
        let mode = self.get_mut();
        mode.dmFields |= DM_COPIES;
        mode.Anonymous1.Anonymous1.dmCopies = copies as i16;
    }
}

#[derive(Clone)]
pub(super) struct Settings {
    pub name: String,
    mode: Mode,
}

pub(super) struct Printer {
    pub name: String,
    pub dc: HDC,
    mode: Mode,
}
impl Drop for Printer {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.dc);
        }
    }
}

#[derive(Clone)]
pub(super) struct PaperChoice {
    pub id: u16,
    pub label: String,
}

impl Printer {
    pub unsafe fn create(name: &str) -> Option<Self> {
        let text = HSTRING::from(name);
        let handle = PrinterHandle::open(&text)?;
        let needed = DocumentPropertiesW(None, handle.0, PCWSTR(text.as_ptr()), None, None, 0);
        if needed <= 0 {
            return None;
        }
        let mut mode = Mode::new(needed as usize)?;
        if DocumentPropertiesW(
            None,
            handle.0,
            PCWSTR(text.as_ptr()),
            Some(mode.ptr_mut()),
            None,
            DM_OUT_BUFFER.0,
        ) != 1
            || !mode.valid()
        {
            return None;
        }
        mode.set_copies(1);
        if !mode.normalize(&text, &handle) {
            return None;
        }
        Self::from_settings(Settings {
            name: name.to_string(),
            mode,
        })
    }

    /// The DC is always created on, used on and destroyed on its owning thread.
    pub unsafe fn from_settings(settings: Settings) -> Option<Self> {
        if !settings.mode.valid() {
            return None;
        }
        let text = HSTRING::from(settings.name.as_str());
        let dc = CreateDCW(
            w!("WINSPOOL"),
            PCWSTR(text.as_ptr()),
            PCWSTR::null(),
            Some(settings.mode.ptr()),
        );
        if dc.is_invalid() {
            return None;
        }
        let printer = Self {
            name: settings.name,
            dc,
            mode: settings.mode,
        };
        printer.geometry()?;
        Some(printer)
    }

    pub fn settings(&self) -> Settings {
        Settings {
            name: self.name.clone(),
            mode: self.mode.clone(),
        }
    }

    unsafe fn apply(&mut self, mut mode: Mode, handle: &PrinterHandle) -> bool {
        let name = HSTRING::from(self.name.as_str());
        if !mode.normalize(&name, handle) {
            return false;
        }
        let Some(next) = Self::from_settings(Settings {
            name: self.name.clone(),
            mode,
        }) else {
            return false;
        };
        // The old settings and DC survive every failure, including CreateDC.
        *self = next;
        true
    }

    pub unsafe fn orient(&mut self, landscape: bool) -> bool {
        let name = HSTRING::from(self.name.as_str());
        let Some(handle) = PrinterHandle::open(&name) else {
            return false;
        };
        let mut mode = self.mode.clone();
        let public = mode.get_mut();
        public.dmFields |= DM_ORIENTATION;
        public.Anonymous1.Anonymous1.dmOrientation = if landscape {
            DMORIENT_LANDSCAPE as i16
        } else {
            DMORIENT_PORTRAIT as i16
        };
        self.apply(mode, &handle)
    }

    pub unsafe fn set_paper(&mut self, paper: u16) -> bool {
        let name = HSTRING::from(self.name.as_str());
        let Some(handle) = PrinterHandle::open(&name) else {
            return false;
        };
        let mut mode = self.mode.clone();
        let public = mode.get_mut();
        public.dmFields |= DM_PAPERSIZE;
        public.dmFields &= !(DM_PAPERWIDTH | DM_PAPERLENGTH | DM_FORMNAME);
        public.Anonymous1.Anonymous1.dmPaperSize = paper as i16;
        self.apply(mode, &handle)
    }

    pub unsafe fn properties(&mut self, owner: HWND) -> Result<bool, ()> {
        let name = HSTRING::from(self.name.as_str());
        let handle = PrinterHandle::open(&name).ok_or(())?;
        let mut mode = self.mode.clone();
        let ptr = mode.ptr_mut();
        let result = DocumentPropertiesW(
            Some(owner),
            handle.0,
            PCWSTR(name.as_ptr()),
            Some(ptr),
            Some(ptr),
            (DM_IN_PROMPT | DM_IN_BUFFER | DM_OUT_BUFFER).0,
        );
        if result == 2 {
            return Ok(false);
        }
        if result != 1 || !mode.valid() {
            return Err(());
        }
        // Copies belong to our own control; keep preview and print settings identical.
        mode.set_copies(1);
        self.apply(mode, &handle).then_some(true).ok_or(())
    }

    pub unsafe fn geometry(&self) -> Option<Geometry> {
        let caps = |cap| GetDeviceCaps(Some(self.dc), cap);
        let g = Geometry {
            physical: (caps(PHYSICALWIDTH), caps(PHYSICALHEIGHT)),
            offset: (caps(PHYSICALOFFSETX), caps(PHYSICALOFFSETY)),
            printable: (caps(HORZRES), caps(VERTRES)),
            dpi: (caps(LOGPIXELSX), caps(LOGPIXELSY)),
        };
        g.valid().then_some(g)
    }

    pub fn paper_id(&self) -> Option<u16> {
        let public = self.mode.get();
        public
            .dmFields
            .contains(DM_PAPERSIZE)
            .then_some(unsafe { public.Anonymous1.Anonymous1.dmPaperSize as u16 })
    }

    pub unsafe fn papers(&self) -> Vec<PaperChoice> {
        let text = HSTRING::from(self.name.as_str());
        let name = PCWSTR(text.as_ptr());
        let mode = Some(self.mode.ptr());
        let count = DeviceCapabilitiesW(name, PCWSTR::null(), DC_PAPERS, None, mode);
        if !(1..=1024).contains(&count)
            || DeviceCapabilitiesW(name, PCWSTR::null(), DC_PAPERNAMES, None, mode) != count
            || DeviceCapabilitiesW(name, PCWSTR::null(), DC_PAPERSIZE, None, mode) != count
        {
            return Vec::new();
        }
        let n = count as usize;
        let mut ids = vec![0u16; n];
        let mut names = vec![0u16; n * 64];
        let mut sizes = vec![POINT::default(); n];
        if DeviceCapabilitiesW(
            name,
            PCWSTR::null(),
            DC_PAPERS,
            Some(PWSTR(ids.as_mut_ptr())),
            mode,
        ) != count
            || DeviceCapabilitiesW(
                name,
                PCWSTR::null(),
                DC_PAPERNAMES,
                Some(PWSTR(names.as_mut_ptr())),
                mode,
            ) != count
            || DeviceCapabilitiesW(
                name,
                PCWSTR::null(),
                DC_PAPERSIZE,
                Some(PWSTR(sizes.as_mut_ptr().cast())),
                mode,
            ) != count
        {
            return Vec::new();
        }
        (0..n)
            .map(|i| {
                let raw = &names[i * 64..(i + 1) * 64];
                let end = raw.iter().position(|&c| c == 0).unwrap_or(64);
                let mut label = String::from_utf16_lossy(&raw[..end]).trim().to_string();
                if label.is_empty() {
                    label = format!("#{}", ids[i]);
                }
                if sizes[i].x > 0 && sizes[i].y > 0 {
                    label.push_str(&format!(
                        " ({} × {} mm)",
                        sizes[i].x as f64 / 10.0,
                        sizes[i].y as f64 / 10.0
                    ));
                }
                PaperChoice { id: ids[i], label }
            })
            .collect()
    }
}

impl Settings {
    /// Prefer real copies. If unsupported, software pages need blank backs between duplex copies.
    pub unsafe fn copies(mut self, copies: u32) -> Option<(Self, u32, bool)> {
        if !(1..=99).contains(&copies) {
            return None;
        }
        let name = HSTRING::from(self.name.as_str());
        let handle = PrinterHandle::open(&name)?;
        let supported = DeviceCapabilitiesW(
            PCWSTR(name.as_ptr()),
            PCWSTR::null(),
            DC_COPIES,
            None,
            Some(self.mode.ptr()),
        );
        let native_copies = if supported >= copies as i32 {
            copies
        } else {
            1
        };
        self.mode.set_copies(native_copies);
        if !self.mode.normalize(&name, &handle) {
            return None;
        }
        let public = self.mode.get();
        let accepted = native_copies == copies
            && public.dmFields.contains(DM_COPIES)
            && public.Anonymous1.Anonymous1.dmCopies == copies as i16;
        if !accepted {
            self.mode.set_copies(1);
            if !self.mode.normalize(&name, &handle) {
                return None;
            }
        }
        let public = self.mode.get();
        let duplex = public.dmFields.contains(DM_DUPLEX) && public.dmDuplex != DMDUP_SIMPLEX;
        Some((self, if accepted { 1 } else { copies }, duplex))
    }
}

pub(super) unsafe fn enumerate() -> Vec<String> {
    let mut needed = 0;
    let mut returned = 0;
    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let _ = EnumPrintersW(flags, PCWSTR::null(), 4, None, &mut needed, &mut returned);
    if needed == 0 {
        return Vec::new();
    }
    // EnumPrinters stores both structures and pointed-to strings in this aligned allocation.
    let mut words = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    let buffer = std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), needed as usize);
    if EnumPrintersW(
        flags,
        PCWSTR::null(),
        4,
        Some(buffer),
        &mut needed,
        &mut returned,
    )
    .is_err()
        || returned as usize > buffer.len() / size_of::<PRINTER_INFO_4W>()
    {
        return Vec::new();
    }
    std::slice::from_raw_parts(words.as_ptr().cast::<PRINTER_INFO_4W>(), returned as usize)
        .iter()
        .filter_map(|info| info.pPrinterName.to_string().ok())
        .filter(|name| !name.is_empty())
        .collect()
}

pub(super) unsafe fn default_name() -> Option<String> {
    let mut len = 0;
    let _ = GetDefaultPrinterW(None, &mut len);
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    if !GetDefaultPrinterW(Some(PWSTR(buf.as_mut_ptr())), &mut len).as_bool() {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}
