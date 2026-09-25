//! Dev harness: hosts the Lister plugin in a plain window (as Total Commander would), runs a
//! scripted scenario and saves screen captures as PNG.
//!
//! ```text
//! cargo run -p mediares_combo --example lister_harness -- <file> <out_dir> [steps...]
//! steps: wait:<ms>  shot:<name>  key:<vk hex>  click:<x>,<y>  dblclick:<x>,<y>
//!        resize:<w>,<h>  wheel:<delta>  next:<file>  rects
//!        input:[ctrl+|shift+]<vk hex>   real keyboard input (modifiers are seen by GetKeyState)
//!        oclick:top|bottom,<x>,<y>   click in the fullscreen panel
//!        move:<x>,<y>    move the real cursor (screen coordinates)
//!        copy:<name>     ListSendCommand(lc_copy), clipboard DIB saved as <name>.png
//!        thumb:<w>,<h>,<name>[,<file>]  ListGetPreviewBitmapW saved as <name>.png
//! ```
//! Screenshots cover the host window, or the whole monitor once the viewer went fullscreen.
//! With `HARNESS_QUICKVIEW=1` the plugin is hosted in a child panel, like TC's Quick View (Ctrl+Q).

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mediares_combo::{ListCloseWindow, ListGetPreviewBitmapW, ListLoadNextW, ListLoadW, ListSendCommand};
use mediares_core::image::{ImageBuffer, Rgba};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY, CAPTUREBLT,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::Graphics::Gdi::{DeleteObject as DeleteGdi, GetObjectW, BITMAP, HBITMAP};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::*;

fn save_bgra(path: &Path, width: u32, height: u32, bgra: &[u8], bottom_up: bool) {
    let row = width as usize * 4;
    let mut rgba = Vec::with_capacity(bgra.len());
    for y in 0..height as usize {
        let src = if bottom_up { height as usize - 1 - y } else { y };
        rgba.extend(bgra[src * row..(src + 1) * row].chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]));
    }
    ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, rgba).expect("buffer").save(path).expect("save png");
}

unsafe fn save_clipboard_dib(owner: HWND, path: &Path) {
    if OpenClipboard(Some(owner)).is_err() {
        println!("clipboard busy");
        return;
    }
    match GetClipboardData(8) {
        Ok(handle) => {
            let mem = HGLOBAL(handle.0);
            let p = GlobalLock(mem) as *const u8;
            let head = &*(p as *const BITMAPINFOHEADER);
            let (w, h) = (head.biWidth as u32, head.biHeight.unsigned_abs());
            let pixels = std::slice::from_raw_parts(p.add(head.biSize as usize), w as usize * 4 * h as usize);
            println!("clipboard DIB {}x{} {}bpp", w, h, head.biBitCount);
            save_bgra(path, w, h, pixels, head.biHeight > 0);
            let _ = GlobalUnlock(mem);
        }
        Err(_) => println!("clipboard has no DIB"),
    }
    let _ = CloseClipboard();
}

unsafe fn save_hbitmap(bitmap: HBITMAP, path: &Path) {
    let mut info = BITMAP::default();
    GetObjectW(bitmap.into(), size_of::<BITMAP>() as i32, Some(&mut info as *mut _ as *mut _));
    println!("preview bitmap {}x{} {}bpp", info.bmWidth, info.bmHeight, info.bmBitsPixel);
    let len = info.bmWidthBytes as usize * info.bmHeight as usize;
    let bits = std::slice::from_raw_parts(info.bmBits as *const u8, len);
    save_bgra(path, info.bmWidth as u32, info.bmHeight as u32, bits, false);
    let _ = DeleteGdi(bitmap.into());
}

unsafe fn send_keys(keys: &[(u16, bool)]) {
    let inputs: Vec<INPUT> = keys
        .iter()
        .map(|&(vk, up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, ..Default::default() },
            },
        })
        .collect();
    SendInput(&inputs, size_of::<INPUT>() as i32);
}

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
    let _ = BitBlt(dc, 0, 0, w, h, Some(screen), rc.left, rc.top, SRCCOPY | CAPTUREBLT);
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
                "input" => {
                    let mut mods = Vec::new();
                    let mut key = arg;
                    while let Some((m, rest)) = key.split_once('+') {
                        mods.push(match m {
                            "ctrl" => 0x11u16,
                            "shift" => 0x10,
                            other => panic!("modifier {}", other),
                        });
                        key = rest;
                    }
                    let vk = u16::from_str_radix(key, 16).expect("hex vk");
                    let _ = SetForegroundWindow(host);
                    let _ = SetFocus(Some(viewer));
                    let mut seq: Vec<(u16, bool)> = mods.iter().map(|&m| (m, false)).collect();
                    seq.push((vk, false));
                    seq.push((vk, true));
                    seq.extend(mods.iter().rev().map(|&m| (m, true)));
                    send_keys(&seq);
                    pump(300);
                }
                "oclick" => {
                    // Click inside an overlay strip ("top" / "bottom"), in its client coordinates.
                    let (which, xy_arg) = arg.split_once(',').expect("oclick:top|bottom,x,y");
                    let mut strips = Vec::new();
                    let mut after: Option<HWND> = None;
                    while let Ok(h) = FindWindowExW(None, after, w!("MediaresOverlay"), None) {
                        let mut rc = RECT::default();
                        let _ = GetWindowRect(h, &mut rc);
                        strips.push((rc.top, h));
                        after = Some(h);
                    }
                    strips.sort_by_key(|(top, _)| *top);
                    let target = if which == "top" { strips.first() } else { strips.last() };
                    match target {
                        Some(&(_, h)) => {
                            let (x, y) = xy(xy_arg);
                            let lp = LPARAM(((y as isize) << 16) | (x as isize & 0xFFFF));
                            let _ = PostMessageW(Some(h), WM_LBUTTONDOWN, WPARAM(1), lp);
                            let _ = PostMessageW(Some(h), WM_LBUTTONUP, WPARAM(0), lp);
                        }
                        _ => println!("no overlay strip {}", which),
                    }
                    pump(150);
                }
                "move" => {
                    // Real cursor movement, in screen coordinates.
                    let (x, y) = xy(arg);
                    let _ = SetCursorPos(x, y);
                    pump(150);
                }
                "copy" => {
                    let ok = ListSendCommand(viewer, 1, 0);
                    println!("ListSendCommand(lc_copy) -> {}", ok);
                    pump(100);
                    save_clipboard_dib(host, &out_dir.join(format!("{}.png", arg)));
                }
                "thumb" => {
                    let parts: Vec<&str> = arg.splitn(4, ',').collect();
                    let (w, h) = (parts[0].parse().expect("w"), parts[1].parse().expect("h"));
                    let target = parts.get(3).map(PathBuf::from).unwrap_or_else(|| file.clone());
                    let started = Instant::now();
                    let bitmap = ListGetPreviewBitmapW(wide(&target).as_ptr(), w, h, std::ptr::null(), 0);
                    println!("ListGetPreviewBitmapW({}) in {} ms", target.display(), started.elapsed().as_millis());
                    if bitmap.is_invalid() {
                        println!("no preview bitmap");
                    } else {
                        save_hbitmap(bitmap, &out_dir.join(format!("{}.png", parts[2])));
                    }
                }
                other => panic!("unknown step {}", other),
            }
        }

        ListCloseWindow(viewer);
        let _ = DestroyWindow(host);
        pump(100);
    }
}
