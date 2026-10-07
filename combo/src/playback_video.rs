//! Video playback via `IMFMediaEngine` (video + its audio track, A/V sync handled by MF). Also
//! the fallback for audio formats the pure-Rust decoder doesn't cover (WMA, Opus, AC3).
//!
//! The engine runs in frame-server mode: a render thread asks it for the current frame
//! (`OnVideoStreamTick`) once per desktop composition and copies a new one with
//! `TransferVideoFrame` into our own D3D11 swap chain on the surface HWND. (The engine's windowed
//! mode binds to the window's composition target and breaks when the viewer is re-parented for
//! fullscreen.) Paced by a window timer instead, presents drifted against the display's refresh
//! and about one frame in twenty never reached the screen. Engine events arrive on MF worker
//! threads and are forwarded to the viewer as [`WM_MEDIA_EVENT`] (`wparam` = event, `lparam` =
//! param 1).

use std::cell::Cell;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mediares_core::keyframes::KeyPicture;
use mediares_core::mf_init::{mf_scope, ComScope};
use windows::core::{implement, Interface, BSTR};
use windows::Win32::Foundation::{E_FAIL, HMODULE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISurface1, IDXGISwapChain1, DXGI_MWA_NO_ALT_ENTER,
    DXGI_MWA_NO_WINDOW_CHANGES, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG_GDI_COMPATIBLE, DXGI_SWAP_EFFECT, DXGI_SWAP_EFFECT_DISCARD,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{
    FillRect, GetStockObject, SetBrushOrgEx, SetStretchBltMode, StretchDIBits, BLACK_BRUSH,
    DIB_RGB_COLORS, HALFTONE, HBRUSH, HFONT, SRCCOPY,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFDXGIDeviceManager, IMFMediaEngine,
    IMFMediaEngineClassFactory, IMFMediaEngineEx, IMFMediaEngineNotify, IMFMediaEngineNotify_Impl,
    MFCreateAttributes, MFCreateDXGIDeviceManager, MFMediaType_Audio, MFARGB,
    MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_DXGI_MANAGER, MF_MEDIA_ENGINE_SEEK_MODE_APPROXIMATE,
    MF_MEDIA_ENGINE_SEEK_MODE_NORMAL, MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, MF_MT_MAJOR_TYPE,
    MF_SD_LANGUAGE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VT_CLSID, VT_LPWSTR};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::transport_bar::{clamp_to_duration, Transport};

pub const WM_MEDIA_EVENT: u32 = WM_APP + 0x10;

#[implement(IMFMediaEngineNotify)]
struct EngineNotify {
    /// Viewer window, stored as an integer: HWND is not `Send`, the callback runs on MF threads.
    target: isize,
}

impl IMFMediaEngineNotify_Impl for EngineNotify_Impl {
    fn EventNotify(&self, event: u32, param1: usize, _param2: u32) -> windows::core::Result<()> {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.target as *mut _)),
                WM_MEDIA_EVENT,
                WPARAM(event as usize),
                LPARAM(param1 as isize),
            );
        }
        Ok(())
    }
}

/// D3D11 swap chain on the surface window that frames are transferred into.
struct Output {
    device: ID3D11Device,
    swap_chain: IDXGISwapChain1,
    size: (u32, u32),
    last_pts: Option<i64>,
    /// Redraw even without a new frame (after a resize).
    dirty: bool,
    /// OSD text on the last presented frame.
    last_osd: Option<String>,
}

/// Info line drawn into the video frame (GDI on the swap chain's back buffer).
#[derive(Clone, Copy)]
pub struct Osd<'a> {
    pub text: &'a str,
    pub font: HFONT,
    pub color: u32,
}

/// [`Osd`] handed over to the render thread.
#[derive(PartialEq)]
struct OwnedOsd {
    text: String,
    font: HFONT,
    color: u32,
}

impl OwnedOsd {
    fn borrowed(&self) -> Osd<'_> {
        Osd {
            text: &self.text,
            font: self.font,
            color: self.color,
        }
    }
}

impl Output {
    unsafe fn new(surface: HWND) -> windows::core::Result<Self> {
        let device = create_device()?;
        let factory: IDXGIFactory2 = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()?;
        let make = |effect: DXGI_SWAP_EFFECT, buffers: u32| {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: 1,
                Height: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: buffers,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: effect,
                // Lets GDI draw the OSD onto the back buffer.
                Flags: DXGI_SWAP_CHAIN_FLAG_GDI_COMPATIBLE.0 as u32,
                ..Default::default()
            };
            factory.CreateSwapChainForHwnd(&device, surface, &desc, None, None)
        };
        // Flip model is preferred; the legacy blt model is a fallback for old drivers.
        let swap_chain = make(DXGI_SWAP_EFFECT_FLIP_DISCARD, 2)
            .or_else(|_| make(DXGI_SWAP_EFFECT_DISCARD, 1))?;
        let _ = factory
            .MakeWindowAssociation(surface, DXGI_MWA_NO_ALT_ENTER | DXGI_MWA_NO_WINDOW_CHANGES);
        Ok(Self {
            device,
            swap_chain,
            size: (1, 1),
            last_pts: None,
            dirty: true,
            last_osd: None,
        })
    }
}

unsafe fn draw_osd(swap_chain: &IDXGISwapChain1, osd: &Osd<'_>) {
    let Ok(surface) = swap_chain.GetBuffer::<IDXGISurface1>(0) else {
        return;
    };
    // `false`: keep the frame that was just transferred.
    let Ok(dc) = surface.GetDC(false) else { return };
    crate::image_view::draw_osd_text(dc, osd.text, osd.font, osd.color);
    let _ = surface.ReleaseDC(None);
}

/// Draws `picture` letterboxed (as the engine does its frames), with the OSD.
unsafe fn present_picture(out: &mut Output, picture: &KeyPicture, osd: Option<Osd<'_>>) {
    let Ok(surface) = out.swap_chain.GetBuffer::<IDXGISurface1>(0) else {
        return;
    };
    // `true`: everything is drawn anew.
    let Ok(dc) = surface.GetDC(true) else { return };
    let (cw, ch) = (out.size.0 as i32, out.size.1 as i32);
    let all = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    FillRect(dc, &all, HBRUSH(GetStockObject(BLACK_BRUSH).0));
    let (pw, ph) = (picture.width as f64, picture.height as f64);
    let scale = (cw as f64 / pw).min(ch as f64 / ph);
    let (dw, dh) = ((pw * scale).round() as i32, (ph * scale).round() as i32);
    let bmi = crate::gdi::bitmap_info(picture.width, -(picture.height as i32));
    SetStretchBltMode(dc, HALFTONE);
    let _ = SetBrushOrgEx(dc, 0, 0, None);
    StretchDIBits(
        dc,
        (cw - dw) / 2,
        (ch - dh) / 2,
        dw,
        dh,
        0,
        0,
        picture.width as i32,
        picture.height as i32,
        Some(picture.bgra.as_ptr() as *const _),
        &bmi,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
    if let Some(osd) = &osd {
        crate::image_view::draw_osd_text(dc, osd.text, osd.font, osd.color);
    }
    let _ = surface.ReleaseDC(None);
    drop(surface);
    let _ = out.swap_chain.Present(0, DXGI_PRESENT(0));
    out.dirty = false;
    out.last_osd = osd.map(|o| o.text.to_string());
}

unsafe fn create_device() -> windows::core::Result<ID3D11Device> {
    let flags = D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    let try_driver = |driver: D3D_DRIVER_TYPE| {
        let mut device = None;
        D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            flags,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .and_then(|_| device.ok_or_else(|| E_FAIL.into()))
    };
    let device =
        try_driver(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| try_driver(D3D_DRIVER_TYPE_WARP))?;
    // The media engine and our render thread use the device from their own threads.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    Ok(device)
}

/// Frame length when the stream doesn't state its rate (25 fps).
const FALLBACK_FRAME_SEC: f64 = 0.04;

/// Length of one frame at `frame_rate` fps (25 if unknown).
pub fn frame_sec(frame_rate: f64) -> f64 {
    if frame_rate > 0.0 {
        1.0 / frame_rate
    } else {
        FALLBACK_FRAME_SEC
    }
}

/// Once a seek is done, an exact one waits this long at most for its frame (sources without
/// accurate seeking may never deliver it).
const HOLD_TIMEOUT: Duration = Duration::from_millis(300);

/// `OnVideoStreamTick` time while there is no frame (seeking).
const NO_FRAME: i64 = i64::MIN;

/// Frames aren't presented until the seek is done and one at least as late as `earliest`
/// arrives.
#[derive(Clone, Copy)]
struct Hold {
    /// 100 ns units, as `OnVideoStreamTick` reports.
    earliest: i64,
    since: Instant,
}

/// See [`VideoPlayer::show_picture`].
struct Preview {
    picture: KeyPicture,
    /// [`VideoPlayer::end_preview`] was called: the picture only stands in until the engine has a
    /// frame to show.
    ending: bool,
}

/// The output and what decides what goes on it, shared by the viewer and the render thread.
struct Screen {
    out: Output,
    /// After an exact seek: it decodes from the key frame before the target, and the engine
    /// hands out those frames on the way; showing them flashes an older picture first.
    hold: Option<Hold>,
    /// A picture decoded outside the engine, shown instead of its frames (see
    /// [`VideoPlayer::show_picture`]).
    preview: Option<Preview>,
    /// The OSD to draw on the frames.
    osd: Option<OwnedOsd>,
}

/// What the render thread needs. The engine is used from both threads: in frame-server mode it
/// is meant to be (Microsoft's own sample renders on a thread of its own); the D3D device is
/// multithread-protected.
struct Shared {
    engine: IMFMediaEngine,
    screen: Mutex<Screen>,
    stop: AtomicBool,
}

// SAFETY: the engine is free-threaded, and everything else is used under the `screen` lock.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Screen> {
        self.screen.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Composition ticks shorter than this mean the desktop isn't composing (a minimized window,
/// some remote sessions): `DwmFlush` returns at once.
const MIN_COMPOSITION_TICK: Duration = Duration::from_millis(2);
/// The pace without composition.
const FALLBACK_TICK: Duration = Duration::from_millis(10);

/// Presents the engine's frames, once per desktop composition, until told to stop.
fn render_loop(shared: Arc<Shared>) {
    let _com = ComScope::new();
    let mut quick = 0;
    while !shared.stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let composed = unsafe { DwmFlush() }.is_ok();
        quick = if composed && started.elapsed() >= MIN_COMPOSITION_TICK {
            0
        } else {
            quick + 1
        };
        // One quick return happens when called right before a composition.
        if quick > 2 {
            std::thread::sleep(FALLBACK_TICK);
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let mut screen = shared.lock();
        unsafe { screen.tick(&shared.engine) };
    }
}

impl Screen {
    /// Presents the engine's frame if there is a new one (or a redraw is pending, or the OSD text
    /// changed). A preview stands in while there is none to show.
    unsafe fn tick(&mut self, engine: &IMFMediaEngine) {
        let osd_changed = self.out.last_osd.as_deref() != self.osd.as_ref().map(|o| &*o.text);
        let (previewing, ending) = match &self.preview {
            Some(p) => (!p.ending, p.ending),
            None => (false, false),
        };
        // The engine's frames are pulled under a preview too: that is what carries its seeks
        // through (one may be under way to where an earlier preview settled).
        let frame = self
            .presentable_frame(engine, self.out.dirty && !previewing && !ending)
            .filter(|_| !previewing);
        let osd = self.osd.as_ref().map(OwnedOsd::borrowed);
        match frame {
            Some(pts) => {
                if (self.out.last_pts != Some(pts) || self.out.dirty || osd_changed)
                    && present_frame(engine, &mut self.out, pts, osd)
                {
                    self.preview = None;
                }
            }
            None => {
                if let Some(preview) = &self.preview {
                    if self.out.dirty || osd_changed {
                        present_picture(&mut self.out, &preview.picture, osd);
                    }
                }
            }
        }
    }

    /// The time of the engine's current frame if it may be shown. While seeking there is none:
    /// presenting would flash a blank picture, and callers would take it for the seek's frame;
    /// only a `redraw` (resize) shows the one the engine still holds after an approximate seek.
    /// After an exact seek, nothing before the target's frame (see [`Hold`]).
    unsafe fn presentable_frame(&mut self, engine: &IMFMediaEngine, redraw: bool) -> Option<i64> {
        let pts = engine.OnVideoStreamTick().ok()?;
        if engine.IsSeeking().as_bool() {
            if let Some(hold) = &mut self.hold {
                // The timeout runs from the end of the seek: slow sources take seconds.
                hold.since = Instant::now();
                return None;
            }
            return (redraw && pts != NO_FRAME).then_some(pts);
        }
        if let Some(hold) = self.hold {
            if pts < hold.earliest && hold.since.elapsed() < HOLD_TIMEOUT {
                return None;
            }
            self.hold = None;
        }
        Some(pts)
    }
}

/// Transfers the engine's frame to the screen; false if it failed.
unsafe fn present_frame(
    engine: &IMFMediaEngine,
    out: &mut Output,
    pts: i64,
    osd: Option<Osd<'_>>,
) -> bool {
    let Ok(back_buffer) = out.swap_chain.GetBuffer::<ID3D11Texture2D>(0) else {
        return false;
    };
    let dst = RECT {
        left: 0,
        top: 0,
        right: out.size.0 as i32,
        bottom: out.size.1 as i32,
    };
    let black = MFARGB {
        rgbBlue: 0,
        rgbGreen: 0,
        rgbRed: 0,
        rgbAlpha: 255,
    };
    if engine
        .TransferVideoFrame(&back_buffer, None, &dst, Some(&black))
        .is_err()
    {
        return false;
    }
    drop(back_buffer);
    if let Some(osd) = &osd {
        draw_osd(&out.swap_chain, osd);
    }
    let _ = out.swap_chain.Present(0, DXGI_PRESENT(0));
    out.last_pts = Some(pts);
    out.dirty = false;
    out.last_osd = osd.map(|o| o.text.to_string());
    true
}

/// Field order matters: the engine is released before the device manager and COM.
pub struct VideoPlayer {
    engine: IMFMediaEngine,
    engine_ex: Option<IMFMediaEngineEx>,
    /// Frame length of the current file (see [`Self::set_frame_rate`]).
    frame: Cell<f64>,
    shared: Arc<Shared>,
    renderer: Option<JoinHandle<()>>,
    _manager: IMFDXGIDeviceManager,
    _com: ComScope,
}

impl VideoPlayer {
    /// Creates an engine rendering into `surface` and notifying `events_to`.
    pub unsafe fn new(surface: HWND, events_to: HWND) -> windows::core::Result<Self> {
        let com = mf_scope().ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        let output = Output::new(surface)?;

        let mut reset_token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
        let manager = manager.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        manager.ResetDevice(&output.device, reset_token)?;

        let factory: IMFMediaEngineClassFactory =
            CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
        let notify: IMFMediaEngineNotify = EngineNotify {
            target: events_to.0 as isize,
        }
        .into();

        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attributes, 3)?;
        let attributes = attributes.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        attributes.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
        attributes.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager)?;
        attributes.SetUINT32(
            &MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT,
            DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32,
        )?;

        let engine = factory.CreateInstance(0, &attributes)?;
        let engine_ex = engine.cast::<IMFMediaEngineEx>().ok();
        let shared = Arc::new(Shared {
            engine: engine.clone(),
            screen: Mutex::new(Screen {
                out: output,
                hold: None,
                preview: None,
                osd: None,
            }),
            stop: AtomicBool::new(false),
        });
        let renderer = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("mediares-video-render".into())
                .spawn(move || render_loop(shared))
                .map_err(|_| windows::core::Error::from(E_FAIL))?
        };
        Ok(Self {
            engine,
            engine_ex,
            frame: Cell::new(FALLBACK_FRAME_SEC),
            shared,
            renderer: Some(renderer),
            _manager: manager,
            _com: com,
        })
    }

    fn screen(&self) -> MutexGuard<'_, Screen> {
        self.shared.lock()
    }

    /// Opens `path` and starts playback (asynchronous; errors arrive as an ERROR event).
    pub unsafe fn open(&self, path: &Path) -> windows::core::Result<()> {
        {
            let mut screen = self.screen();
            screen.out.last_pts = None;
            screen.hold = None;
            screen.preview = None;
        }
        self.engine
            .SetSource(&BSTR::from(path.as_os_str().to_string_lossy().as_ref()))?;
        self.engine.Play()
    }

    /// Shows `picture` instead of the engine's frames until [`Self::end_preview`]: the viewer
    /// shows key frames it decoded itself where the engine's seeks take seconds.
    pub fn show_picture(&self, picture: KeyPicture) {
        let mut screen = self.screen();
        screen.preview = Some(Preview {
            picture,
            ending: false,
        });
        screen.out.dirty = true;
    }

    /// Back to the engine's frames. The picture stays on screen until the engine has one (it may
    /// not even have been drawn yet: a click on the timeline ends its preview at once).
    pub fn end_preview(&self) {
        if let Some(preview) = self.screen().preview.as_mut() {
            preview.ending = true;
        }
    }

    /// The viewer's render tick: the OSD for the frames from now on (the render thread draws
    /// them).
    pub unsafe fn render(&self, osd: Option<Osd<'_>>) {
        let osd = osd.map(|o| OwnedOsd {
            text: o.text.to_string(),
            font: o.font,
            color: o.color,
        });
        let mut screen = self.screen();
        if screen.osd != osd {
            screen.osd = osd;
        }
    }

    /// Resizes the swap chain to the surface's new client size.
    pub unsafe fn resize(&self, width: i32, height: i32) {
        let mut screen = self.screen();
        let out = &mut screen.out;
        let size = (width.max(1) as u32, height.max(1) as u32);
        if size != out.size
            && out
                .swap_chain
                .ResizeBuffers(
                    0,
                    size.0,
                    size.1,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG_GDI_COMPATIBLE,
                )
                .is_ok()
        {
            out.size = size;
        }
        out.dirty = true;
    }

    /// Native frame size once metadata is loaded.
    pub unsafe fn native_size(&self) -> Option<(u32, u32)> {
        let (mut w, mut h) = (0u32, 0u32);
        self.engine
            .GetNativeVideoSize(Some(&mut w), Some(&mut h))
            .ok()?;
        (w > 0 && h > 0).then_some((w, h))
    }

    /// The current frame at its native size, as top-down BGRA rows.
    pub unsafe fn capture_frame(&self) -> Option<(u32, u32, Vec<u8>)> {
        let (width, height) = self.native_size()?;
        let screen = self.screen();
        let device = &screen.out.device;
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let mut target: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut target))
            .ok()?;
        let target = target?;
        let dst = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let black = MFARGB {
            rgbBlue: 0,
            rgbGreen: 0,
            rgbRed: 0,
            rgbAlpha: 255,
        };
        self.engine
            .TransferVideoFrame(&target, None, &dst, Some(&black))
            .ok()?;

        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .ok()?;
        let staging = staging?;
        let context = device.GetImmediateContext().ok()?;
        context.CopyResource(&staging, &target);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .ok()?;
        let row = width as usize * 4;
        let mut pixels = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let line = std::slice::from_raw_parts(
                (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                row,
            );
            pixels.extend_from_slice(line);
        }
        context.Unmap(&staging, 0);
        // The alpha channel is undefined for video: make it opaque.
        pixels.chunks_exact_mut(4).for_each(|px| px[3] = 255);
        Some((width, height, pixels))
    }

    /// The speed for this and following files.
    pub unsafe fn set_rate(&self, rate: f64) {
        let _ = self.engine.SetDefaultPlaybackRate(rate);
        self.set_current_rate(rate);
    }

    /// The speed until the next file (or the next `set_rate`).
    pub unsafe fn set_current_rate(&self, rate: f64) {
        let _ = self.engine.SetPlaybackRate(rate);
    }

    pub unsafe fn is_paused(&self) -> bool {
        self.engine.IsPaused().as_bool() || self.engine.IsEnded().as_bool()
    }

    /// A seek is still in progress.
    pub unsafe fn is_seeking(&self) -> bool {
        self.engine.IsSeeking().as_bool()
    }

    /// Timestamp of the frame last put on screen.
    pub fn presented_pts(&self) -> Option<i64> {
        self.screen().out.last_pts
    }

    /// Frame rate of the file being opened (0 if unknown).
    pub fn set_frame_rate(&self, frame_rate: f64) {
        self.frame.set(frame_sec(frame_rate));
    }

    /// Engine error code (`MF_MEDIA_ENGINE_ERR`), if playback failed.
    pub unsafe fn error_code(&self) -> Option<u16> {
        self.engine.GetError().ok().map(|e| e.GetErrorCode())
    }

    /// Switches to the next audio track of the file: its number (from 1), the count and its
    /// language. `None` when there is only one.
    pub unsafe fn cycle_audio_track(&self) -> Option<(usize, usize, Option<String>)> {
        let ex = self.engine_ex.as_ref()?;
        let streams = ex.GetNumberOfStreams().ok()?;
        let audio: Vec<u32> = (0..streams)
            .filter(|&i| stream_major_type(ex, i) == Some(MFMediaType_Audio))
            .collect();
        if audio.len() < 2 {
            return None;
        }
        let current = audio
            .iter()
            .position(|&i| ex.GetStreamSelection(i).is_ok_and(|on| on.as_bool()))
            .unwrap_or(0);
        let next = (current + 1) % audio.len();
        for (k, &i) in audio.iter().enumerate() {
            let _ = ex.SetStreamSelection(i, k == next);
        }
        ex.ApplyStreamSelections().ok()?;
        Some((next + 1, audio.len(), stream_language(ex, audio[next])))
    }
}

unsafe fn stream_major_type(ex: &IMFMediaEngineEx, stream: u32) -> Option<windows::core::GUID> {
    let value = ex.GetStreamAttribute(stream, &MF_MT_MAJOR_TYPE).ok()?;
    let inner = &value.Anonymous.Anonymous;
    (inner.vt == VT_CLSID && !inner.Anonymous.puuid.is_null()).then(|| *inner.Anonymous.puuid)
}

/// The stream's language tag ("eng", "rus"...), if the container names it.
unsafe fn stream_language(ex: &IMFMediaEngineEx, stream: u32) -> Option<String> {
    let value = ex.GetStreamAttribute(stream, &MF_SD_LANGUAGE).ok()?;
    let inner = &value.Anonymous.Anonymous;
    if inner.vt != VT_LPWSTR || inner.Anonymous.pwszVal.is_null() {
        return None;
    }
    let text = inner.Anonymous.pwszVal.to_string().ok()?;
    Some(text).filter(|t| !t.trim().is_empty())
}

// Playback control: plain engine calls on the viewer's thread.
impl Transport for VideoPlayer {
    fn is_playing(&self) -> bool {
        unsafe { !self.engine.IsPaused().as_bool() && !self.engine.IsEnded().as_bool() }
    }

    fn play(&self) {
        unsafe {
            let _ = self.engine.Play();
        }
    }

    fn pause(&self) {
        unsafe {
            let _ = self.engine.Pause();
        }
    }

    fn position(&self) -> f64 {
        finite(unsafe { self.engine.GetCurrentTime() })
    }

    fn duration(&self) -> f64 {
        finite(unsafe { self.engine.GetDuration() })
    }

    fn seek(&self, seconds: f64, approximate: bool) {
        let t = clamp_to_duration(seconds, self.duration());
        // The frame showing at `t` starts up to a frame earlier; the one before it, and the
        // frames decoded on the way from the key frame, start earlier still.
        self.screen().hold = (!approximate).then(|| Hold {
            earliest: ((t - self.frame.get() + 0.001) * 1e7) as i64,
            since: Instant::now(),
        });
        unsafe {
            match &self.engine_ex {
                Some(ex) => {
                    let mode = if approximate {
                        MF_MEDIA_ENGINE_SEEK_MODE_APPROXIMATE
                    } else {
                        MF_MEDIA_ENGINE_SEEK_MODE_NORMAL
                    };
                    let _ = ex.SetCurrentTimeEx(t, mode);
                }
                None => {
                    let _ = self.engine.SetCurrentTime(t);
                }
            }
        }
    }

    fn volume(&self) -> f64 {
        unsafe { self.engine.GetVolume() }
    }

    fn set_volume(&self, volume: f64) {
        unsafe {
            let _ = self.engine.SetVolume(volume.clamp(0.0, 1.0));
        }
    }

    fn is_muted(&self) -> bool {
        unsafe { self.engine.GetMuted().as_bool() }
    }

    fn set_muted(&self, muted: bool) {
        unsafe {
            let _ = self.engine.SetMuted(muted);
        }
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        // The render thread stops within a composition tick, before the engine shuts down.
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(renderer) = self.renderer.take() {
            let _ = renderer.join();
        }
        // Breaks the engine's internal reference cycles and stops events.
        unsafe {
            let _ = self.engine.Shutdown();
        }
    }
}

fn finite(v: f64) -> f64 {
    if v.is_finite() {
        v.max(0.0)
    } else {
        0.0
    }
}
