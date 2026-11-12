//! Background spooling. Workers own their DC and never borrow a preview window or its data.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use windows::core::{BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_CANCELLED, ERROR_PRINT_CANCELLED, ERROR_SUCCESS,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{
    AbortDoc, EndDoc, EndPage, SetAbortProc, StartDocW, StartPage, DOCINFOW,
};

use super::layout::{nonempty, Options, Placement};
use super::printer::{Printer, Settings};
use crate::image_cache::{self, DecodedImage};

thread_local! {
    static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

unsafe extern "system" fn abort_proc(_: HDC, _: i32) -> BOOL {
    BOOL::from(CANCEL.with(|slot| {
        !slot
            .borrow()
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Printed,
    Cancelled,
    Failed,
}

pub(super) struct Request {
    pub settings: Settings,
    pub image: Arc<DecodedImage>,
    pub name: String,
    pub options: Options,
}

pub(super) struct Task {
    cancel: Arc<AtomicBool>,
    result: mpsc::Receiver<Outcome>,
}

impl Task {
    pub fn start(request: Request) -> std::io::Result<Self> {
        Self::spawn(request, None)
    }

    fn spawn(request: Request, output: Option<PathBuf>) -> std::io::Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (tx, result) = mpsc::channel();
        // Detaching is intentional: closing the preview requests cancellation without waiting
        // for a blocked driver. The worker owns all its inputs, including the source image.
        std::thread::Builder::new()
            .name("mediares-print".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    request.run(&worker_cancel, output.as_deref())
                }))
                .unwrap_or(Outcome::Failed);
                let _ = tx.send(outcome);
            })?;
        Ok(Self { cancel, result })
    }
    pub fn poll(&self) -> Option<Outcome> {
        match self.result.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Outcome::Failed),
        }
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Abort the document on every error path, including unwinding; release before deleting the DC.
struct Document(HDC);
impl Drop for Document {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                AbortDoc(self.0);
            }
        }
    }
}

impl Request {
    unsafe fn run(self, cancel: &Arc<AtomicBool>, output: Option<&Path>) -> Outcome {
        if cancel.load(Ordering::Relaxed) {
            return Outcome::Cancelled;
        }
        CANCEL.with(|slot| *slot.borrow_mut() = Some(cancel.clone()));
        let ok = self.spool(cancel, output);
        CANCEL.with(|slot| *slot.borrow_mut() = None);
        if cancel.load(Ordering::Relaxed) {
            Outcome::Cancelled
        } else if ok {
            Outcome::Printed
        } else {
            Outcome::Failed
        }
    }

    unsafe fn spool(self, cancel: &AtomicBool, output: Option<&Path>) -> bool {
        let Some((settings, copies, duplex)) = self.settings.copies(self.options.copies) else {
            return false;
        };
        let Some(printer) = Printer::from_settings(settings) else {
            return false;
        };
        let dc = printer.dc;
        let Some(placement) = printer
            .geometry()
            .and_then(|g| g.layout((self.image.width, self.image.height), self.options))
        else {
            return false;
        };
        let image = if placement.rotate {
            Arc::new(image_cache::rotated(&self.image, true))
        } else {
            self.image
        };
        if cancel.load(Ordering::Relaxed) || SetAbortProc(dc, Some(abort_proc)) <= 0 {
            return false;
        }
        let name = HSTRING::from(self.name.as_str());
        let output = output.map(|path| HSTRING::from(path.as_os_str()));
        let doc = DOCINFOW {
            cbSize: std::mem::size_of::<DOCINFOW>() as i32,
            lpszDocName: PCWSTR(name.as_ptr()),
            lpszOutput: output
                .as_ref()
                .map_or(PCWSTR::null(), |path| PCWSTR(path.as_ptr())),
            ..Default::default()
        };
        SetLastError(ERROR_SUCCESS);
        let started = StartDocW(dc, &doc);
        if started <= 0 {
            let error = GetLastError();
            if matches!(started, SP_USERABORT | SP_APPABORT)
                || error == ERROR_CANCELLED
                || error == ERROR_PRINT_CANCELLED
            {
                cancel.store(true, Ordering::Relaxed);
            }
            return false;
        }
        let mut document = Document(dc);
        for page in 0..page_count(copies, duplex) {
            if cancel.load(Ordering::Relaxed) || StartPage(dc) <= 0 {
                return false;
            }
            // In the software fallback every copy starts on a new sheet, including duplex jobs.
            if (!duplex || page % 2 == 0) && !draw_image(dc, &image, placement) {
                return false;
            }
            if cancel.load(Ordering::Relaxed) || EndPage(dc) <= 0 {
                return false;
            }
        }
        if cancel.load(Ordering::Relaxed) || EndDoc(dc) <= 0 {
            return false;
        }
        document.0 = HDC::default();
        true
    }
}

fn page_count(copies: u32, duplex: bool) -> u32 {
    if duplex {
        copies * 2 - 1
    } else {
        copies
    }
}

/// Draw the already laid out rectangle exactly, including on devices with non-square pixels.
pub(super) unsafe fn draw_image(dc: HDC, image: &DecodedImage, placement: Placement) -> bool {
    if !nonempty(placement.image)
        || !nonempty(placement.clip)
        || image.width == 0
        || image.height == 0
        || image.width > i32::MAX as u32
        || image.height > i32::MAX as u32
        || placement.image.right as i64 - placement.image.left as i64 > i32::MAX as i64
        || placement.image.bottom as i64 - placement.image.top as i64 > i32::MAX as i64
    {
        return false;
    }
    let saved = SaveDC(dc);
    if saved == 0 {
        return false;
    }
    let clip = placement.clip;
    let clipped = IntersectClipRect(dc, clip.left, clip.top, clip.right, clip.bottom).0 != ERROR;
    SetStretchBltMode(dc, HALFTONE);
    let _ = SetBrushOrgEx(dc, 0, 0, None);
    let rect = placement.image;
    let bmi = crate::gdi::bitmap_info(image.width, -(image.height as i32));
    let result = if clipped {
        StretchDIBits(
            dc,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            0,
            0,
            image.width as i32,
            image.height as i32,
            Some(image.bgra.as_ptr().cast()),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        )
    } else {
        0
    };
    let restored = RestoreDC(dc, saved).as_bool();
    result != 0 && result != GDI_ERROR && restored
}

#[cfg(test)]
mod tests {
    use super::super::layout::SizeMode;
    use super::*;
    use windows::Win32::Foundation::RECT;
    fn image() -> Arc<DecodedImage> {
        Arc::new(DecodedImage {
            width: 100,
            height: 100,
            full: (100, 100),
            bgra: [0, 128, 255, 255].repeat(10000),
            is_preview: false,
            exif: None,
            pages: 1,
        })
    }
    #[test]
    fn duplex_software_copies_have_blank_backs_between_copies() {
        assert_eq!(page_count(1, true), 1);
        assert_eq!(page_count(2, true), 3);
        assert_eq!(page_count(3, false), 3);
    }
    #[test]
    fn drawing_errors_are_reported() {
        let rect = RECT {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        assert!(!unsafe {
            draw_image(
                HDC::default(),
                &image(),
                Placement {
                    image: rect,
                    clip: rect,
                    rotate: false,
                },
            )
        });
    }
    #[test]
    #[ignore = "requires Microsoft Print to PDF; creates PDF artifacts in target/print-check"]
    fn pdf_prints_real_jobs_in_both_orientations_and_checks_cancellation() {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/print-check");
        std::fs::create_dir_all(&dir).unwrap();
        for landscape in [false, true] {
            unsafe {
                let mut printer = Printer::create("Microsoft Print to PDF").expect("PDF printer");
                assert!(printer.orient(landscape));
                let opt = Options {
                    margins: RECT {
                        left: 1000,
                        top: 1000,
                        right: 1000,
                        bottom: 1000,
                    },
                    copies: 2,
                    mode: SizeMode::Custom,
                    size_mm: (100.0, 100.0),
                    crop: false,
                    auto_rotate: true,
                    alignment: 4,
                };
                let request = || Request {
                    settings: printer.settings(),
                    image: image(),
                    name: "Mediares print verification".into(),
                    options: opt,
                };
                let output = dir.join(if landscape {
                    "landscape.pdf"
                } else {
                    "portrait.pdf"
                });
                let task = Task::spawn(request(), Some(output.clone())).unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                let outcome = loop {
                    if let Some(outcome) = task.poll() {
                        break outcome;
                    }
                    assert!(std::time::Instant::now() < deadline, "PDF driver timed out");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                };
                assert_eq!(outcome, Outcome::Printed);
                let bytes = std::fs::read(&output).unwrap();
                assert!(bytes.starts_with(b"%PDF-"));
                assert!(bytes.len() > 1000);
                println!("PDF {} ({} bytes)", output.display(), bytes.len());
                let cancelled = dir.join("cancelled.pdf");
                assert_eq!(
                    request().run(&Arc::new(AtomicBool::new(true)), Some(&cancelled)),
                    Outcome::Cancelled
                );
                assert!(!cancelled.exists());
            }
        }
    }
}
