//! Video playback via `IMFMediaEngine` (video + its audio track, A/V sync handled by MF). Also
//! the fallback for audio formats the pure-Rust decoder doesn't cover (WMA, Opus, AC3).
//!
//! The engine runs in frame-server mode: on each render tick we ask it for the current frame
//! (`OnVideoStreamTick`) and copy it with `TransferVideoFrame` into our own D3D11 swap chain on
//! the surface HWND. (The engine's windowed mode binds to the window's composition target and
//! breaks when the viewer is re-parented for fullscreen.) Engine events arrive on MF worker
//! threads and are forwarded to the viewer as [`WM_MEDIA_EVENT`] (`wparam` = event, `lparam` =
//! param 1).

use std::cell::RefCell;
use std::path::Path;

use mediares_core::mf_init::{ComScope, MfSession};
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
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISurface1, IDXGISwapChain1, DXGI_MWA_NO_ALT_ENTER,
    DXGI_MWA_NO_WINDOW_CHANGES, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG_GDI_COMPATIBLE, DXGI_SWAP_EFFECT, DXGI_SWAP_EFFECT_DISCARD,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFDXGIDeviceManager, IMFMediaEngine,
    IMFMediaEngineClassFactory, IMFMediaEngineEx, IMFMediaEngineNotify, IMFMediaEngineNotify_Impl,
    MFCreateAttributes, MFCreateDXGIDeviceManager, MFARGB, MF_MEDIA_ENGINE_CALLBACK,
    MF_MEDIA_ENGINE_DXGI_MANAGER, MF_MEDIA_ENGINE_SEEK_MODE_APPROXIMATE,
    MF_MEDIA_ENGINE_SEEK_MODE_NORMAL, MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::transport_bar::Transport;

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
pub struct Osd<'a> {
    pub text: &'a str,
    pub font: windows::Win32::Graphics::Gdi::HFONT,
    pub color: u32,
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
    // The media engine uses the device from its own threads.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    Ok(device)
}

const FALLBACK_FRAME_SEC: f64 = 0.04;

/// Field order matters: the engine is released before the output, MF and COM.
pub struct VideoPlayer {
    engine: IMFMediaEngine,
    engine_ex: Option<IMFMediaEngineEx>,
    output: RefCell<Output>,
    _manager: IMFDXGIDeviceManager,
    _mf: MfSession,
    _com: ComScope,
}

impl VideoPlayer {
    /// Creates an engine rendering into `surface` and notifying `events_to`.
    pub unsafe fn new(surface: HWND, events_to: HWND) -> windows::core::Result<Self> {
        let com = ComScope::new();
        let mf = MfSession::start().ok_or_else(|| windows::core::Error::from(E_FAIL))?;
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
        Ok(Self {
            engine,
            engine_ex,
            output: RefCell::new(output),
            _manager: manager,
            _mf: mf,
            _com: com,
        })
    }

    /// Opens `path` and starts playback (asynchronous; errors arrive as an ERROR event).
    pub unsafe fn open(&self, path: &Path) -> windows::core::Result<()> {
        self.output.borrow_mut().last_pts = None;
        self.engine
            .SetSource(&BSTR::from(path.as_os_str().to_string_lossy().as_ref()))?;
        self.engine.Play()
    }

    /// Render tick: presents a frame if the engine has a new one (or a redraw is pending, or the
    /// OSD text changed).
    pub unsafe fn render(&self, osd: Option<Osd<'_>>) {
        let Ok(mut out) = self.output.try_borrow_mut() else {
            return;
        };
        let Ok(pts) = self.engine.OnVideoStreamTick() else {
            return;
        };
        let osd_changed = out.last_osd.as_deref() != osd.as_ref().map(|o| o.text);
        if out.last_pts == Some(pts) && !out.dirty && !osd_changed {
            return;
        }
        let Ok(back_buffer) = out.swap_chain.GetBuffer::<ID3D11Texture2D>(0) else {
            return;
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
        if self
            .engine
            .TransferVideoFrame(&back_buffer, None, &dst, Some(&black))
            .is_ok()
        {
            drop(back_buffer);
            if let Some(osd) = &osd {
                draw_osd(&out.swap_chain, osd);
            }
            let _ = out.swap_chain.Present(0, DXGI_PRESENT(0));
            out.last_pts = Some(pts);
            out.dirty = false;
            out.last_osd = osd.map(|o| o.text.to_string());
        }
    }

    /// Resizes the swap chain to the surface's new client size.
    pub unsafe fn resize(&self, width: i32, height: i32) {
        let mut out = self.output.borrow_mut();
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
        let out = self.output.try_borrow().ok()?;
        let device = &out.device;
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

    /// Timestamp of the frame last put on screen.
    pub fn presented_pts(&self) -> Option<i64> {
        self.output.try_borrow().ok()?.last_pts
    }

    /// Pauses and seeks exactly one frame (`frame_rate` fps; 25 if unknown) back. Returns the
    /// target (sources without accurate seeking land elsewhere).
    pub unsafe fn step_back(&self, frame_rate: f64) -> f64 {
        let _ = self.engine.Pause();
        let frame = if frame_rate > 0.0 {
            1.0 / frame_rate
        } else {
            FALLBACK_FRAME_SEC
        };
        let target = (self.position() - frame).max(0.0);
        self.seek(target, false);
        target
    }

    /// Engine error code (`MF_MEDIA_ENGINE_ERR`), if playback failed.
    pub unsafe fn error_code(&self) -> Option<u16> {
        self.engine.GetError().ok().map(|e| e.GetErrorCode())
    }
}

// Engine calls are plain COM calls on the thread that created the player (the viewer's).
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
        let t = seconds.clamp(0.0, self.duration().max(0.0));
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
