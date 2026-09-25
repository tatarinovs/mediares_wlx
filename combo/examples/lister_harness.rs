//! Dev harness: hosts the Lister plugin in a plain window (as Total Commander would), runs a
//! scripted scenario and saves screen captures as PNG.
//!
//! ```text
//! cargo run -p mediares_combo --example lister_harness -- <file> <out_dir> [steps...]
//! steps: wait:<ms>  shot:<name>  key:<vk hex>  click:<x>,<y>  dblclick:<x>,<y>
//!        resize:<w>,<h>  wheel:<delta>  next:<file>  rects
//! ```
//! Screenshots cover the host window, or the whole monitor once the viewer went fullscreen.
//! With `HARNESS_QUICKVIEW=1` the plugin is hosted in a child panel, like TC's Quick View (Ctrl+Q).

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mediares_combo::{ListCloseWindow, ListLoadNextW, ListLoadW};
use mediares_core::image::{ImageBuffer, Rgba};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::*;

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_SIZE {
        // Like TC: keep the plugin window filling the client area.
        if let Ok(child) = GetWindow(hwnd, GW_CHILD) {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let _ = MoveWindow(child, 0, 0, rc.right, rc.bottom, true);
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn pump(ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    let mut msg = MSG::default();
    while Instant::now() < until {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

unsafe fn capture(rc: RECT, out: &Path) {
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, w, h);
    let old = SelectObject(dc, bmp.into());
    let _ = BitBlt(dc, 0, 0, w, h, Some(screen), rc.left, rc.top, SRCCOPY);
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    GetDIBits(dc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bmi, DIB_RGB_COLORS);
    SelectObject(dc, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(dc);
    ReleaseDC(None, screen);

    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    let img: ImageBuffer<Rgba<u8>, _> = ImageBuffer::from_raw(w as u32, h as u32, buf).expect("buffer size");
    img.save(out).expect("save png");
    println!("shot {}", out.display());
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn xy(s: &str) -> (i32, i32) {
    let (a, b) = s.split_once(',').expect("x,y");
    (a.trim().parse().expect("x"), b.trim().parse().expect("y"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = PathBuf::from(&args[0]);
    let out_dir = PathBuf::from(&args[1]);
    std::fs::create_dir_all(&out_dir).ok();

    unsafe {
        let class = w!("MediaresHarnessHost");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(host_proc),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let host = CreateWindowExW(
            WS_EX_TOPMOST,
            class,
            w!("Harness"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_CLIPCHILDREN,
            100,
            100,
            1280,
            800,
            None,
            None,
            None,
            None,
        )
        .expect("host window");
        pump(200);

        let parent = if std::env::var_os("HARNESS_QUICKVIEW").is_some() {
            let mut rc = RECT::default();
            let _ = GetClientRect(host, &mut rc);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                None,
                WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN,
                rc.right / 3,
                0,
                rc.right - rc.right / 3,
                rc.bottom,
                Some(host),
                None,
                None,
                None,
            )
            .expect("quick view panel")
        } else {
            host
        };

        let viewer = ListLoadW(parent, wide(&file).as_ptr(), 0);
        println!("ListLoadW -> {:?}", viewer);
        if viewer.is_invalid() {
            let _ = DestroyWindow(host);
            return;
        }

        for step in &args[2..] {
            let (cmd, arg) = step.split_once(':').unwrap_or((step, ""));
            match cmd {
                "wait" => pump(arg.parse().expect("ms")),
                "shot" => {
                    // A fullscreen viewer is a top-level popup (GetParent would return its owner).
                    let fullscreen = (GetWindowLongPtrW(viewer, GWL_STYLE) as u32 & WS_CHILD.0) == 0;
                    let mut rc = RECT::default();
                    let _ = GetWindowRect(if fullscreen { viewer } else { host }, &mut rc);
                    capture(rc, &out_dir.join(format!("{}.png", arg)));
                }
                "key" => {
                    let vk = usize::from_str_radix(arg, 16).expect("hex vk");
                    let _ = PostMessageW(Some(viewer), WM_KEYDOWN, WPARAM(vk), LPARAM(0));
                    pump(50);
                }
                "click" | "dblclick" => {
                    let (x, y) = xy(arg);
                    let lp = LPARAM(((y as isize) << 16) | (x as isize & 0xFFFF));
                    let _ = PostMessageW(Some(viewer), WM_LBUTTONDOWN, WPARAM(1), lp);
                    let _ = PostMessageW(Some(viewer), WM_LBUTTONUP, WPARAM(0), lp);
                    if cmd == "dblclick" {
                        let _ = PostMessageW(Some(viewer), WM_LBUTTONDBLCLK, WPARAM(1), lp);
                        let _ = PostMessageW(Some(viewer), WM_LBUTTONUP, WPARAM(0), lp);
                    }
                    pump(50);
                }
                "wheel" => {
                    let delta: i16 = arg.parse().expect("delta");
                    let _ = PostMessageW(Some(viewer), WM_MOUSEWHEEL, WPARAM((delta as u16 as usize) << 16), LPARAM(0));
                    pump(50);
                }
                "rects" => {
                    let mut rc = RECT::default();
                    let _ = GetWindowRect(viewer, &mut rc);
                    println!("viewer {:?}", rc);
                    if let Ok(child) = GetWindow(viewer, GW_CHILD) {
                        let _ = GetWindowRect(child, &mut rc);
                        println!("surface {:?}", rc);
                    }
                }
                "resize" => {
                    let (w, h) = xy(arg);
                    let _ = SetWindowPos(host, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER);
                    pump(100);
                }
                "next" => {
                    let r = ListLoadNextW(parent, viewer, wide(Path::new(arg)).as_ptr(), 0);
                    println!("ListLoadNextW -> {}", r);
                }
                other => panic!("unknown step {}", other),
            }
        }

        ListCloseWindow(viewer);
        let _ = DestroyWindow(host);
        pump(100);
    }
}
