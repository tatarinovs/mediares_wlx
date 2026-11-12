//! Dev harness: hosts the Lister plugin in a plain window (as Total Commander would), runs a
//! scripted scenario and saves screen captures as PNG.
//!
//! ```text
//! cargo run -p mediares_combo --example lister_harness -- <file> <out_dir> [steps...]
//! steps: wait:<ms>  shot:<name>  key:<vk hex>  click:<x>,<y>  dblclick:<x>,<y>
//!        resize:<w>,<h>  wheel:<delta>[,<x>,<y>]  next:<file>  rects
//!        ldown:<x>,<y>  drag:<x>,<y>  lup:<x>,<y>   left button down / moved with it / up
//!        steal           the host takes the mouse capture (as a menu or another program would)
//!        hold:<vk hex>,<n>  a key held for <n> auto-repeats, all queued at once (a slow handler
//!                        falling behind), then released; prints how long the queue took
//!        input:[ctrl+|shift+]<vk hex>   real keyboard input (modifiers are seen by GetKeyState)
//!        oclick:top|bottom,<x>,<y>   click in the fullscreen panel
//!        move:<x>,<y>    move the real cursor (screen coordinates)
//!        copy:<name>     ListSendCommand(lc_copy), clipboard DIB saved as <name>.png
//!        print           open print preview through ListPrintW (pair with dialog to cancel)
//!        print-check     check preview orientation/margins/copies, capture it, then cancel
//!        print-pdf-check print through the preview and the PDF driver's Save As dialog
//!        print-pdf-cancel-check cancel the PDF filename dialog and return to the preview
//!        thumb:<w>,<h>,<name>[,<file>]  ListGetPreviewBitmapW saved as <name>.png
//!        dialog:<name>[,<pages>]  put before the step that opens a modal dialog: waits for it,
//!                        saves <name>_0.png, then Ctrl+Tab and <name>_<i>.png for each further
//!                        page, and closes it (Cancel: nothing is saved)
//! ```
//! Screenshots cover the host window, or the whole monitor once the viewer went fullscreen.
//! With `HARNESS_QUICKVIEW=1` the plugin is hosted in a child panel, like TC's Quick View (Ctrl+Q).
//! With `HARNESS_SHOW_AFTER_LOAD=1` the host is an ordinary (not topmost) window shown only after
//! `ListLoad`, the order TC uses; the start of fullscreen mode looks as in TC then.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mediares_combo::{
    ListCloseWindow, ListGetPreviewBitmapW, ListLoadNextW, ListLoadW, ListPrintW, ListSendCommand,
};
use mediares_core::image::{ImageBuffer, Rgba};
use windows::core::w;
use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    InvalidateRect, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
    DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::Graphics::Gdi::{DeleteObject as DeleteGdi, GetObjectW, BITMAP, HBITMAP};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    IsWindowEnabled, SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::*;

fn save_bgra(path: &Path, width: u32, height: u32, bgra: &[u8], bottom_up: bool) {
    let row = width as usize * 4;
    let mut rgba = Vec::with_capacity(bgra.len());
    for y in 0..height as usize {
        let src = if bottom_up {
            height as usize - 1 - y
        } else {
            y
        };
        rgba.extend(
            bgra[src * row..(src + 1) * row]
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[2], p[1], p[0], 255]),
        );
    }
    ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, rgba)
        .expect("buffer")
        .save(path)
        .expect("save png");
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
            let pixels = std::slice::from_raw_parts(
                p.add(head.biSize as usize),
                w as usize * 4 * h as usize,
            );
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
    GetObjectW(
        bitmap.into(),
        size_of::<BITMAP>() as i32,
        Some(&mut info as *mut _ as *mut _),
    );
    println!(
        "preview bitmap {}x{} {}bpp",
        info.bmWidth, info.bmHeight, info.bmBitsPixel
    );
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
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    ..Default::default()
                },
            },
        })
        .collect();
    SendInput(&inputs, size_of::<INPUT>() as i32);
}

unsafe extern "system" fn host_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLOSE {
        // TC would close the Lister here; the scenario goes on and closes it at the end.
        println!("host: WM_CLOSE");
        return LRESULT(0);
    }
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
    let _ = BitBlt(
        dc,
        0,
        0,
        w,
        h,
        Some(screen),
        rc.left,
        rc.top,
        SRCCOPY | CAPTUREBLT,
    );
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
    GetDIBits(
        dc,
        bmp,
        0,
        h as u32,
        Some(buf.as_mut_ptr() as *mut _),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    SelectObject(dc, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(dc);
    ReleaseDC(None, screen);

    for px in buf.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
    let img: ImageBuffer<Rgba<u8>, _> =
        ImageBuffer::from_raw(w as u32, h as u32, buf).expect("buffer size");
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

/// The visible modal dialog of thread `thread`, if one is open.
unsafe fn find_dialog(thread: u32) -> Option<HWND> {
    unsafe extern "system" fn each(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if IsWindowVisible(hwnd).as_bool() && ex & WS_EX_DLGMODALFRAME.0 != 0 {
            *(found.0 as *mut isize) = hwnd.0 as isize;
            return false.into();
        }
        true.into()
    }
    let mut found: isize = 0;
    let _ = EnumThreadWindows(
        thread,
        Some(each),
        LPARAM(&mut found as *mut isize as isize),
    );
    (found != 0).then_some(HWND(found as *mut _))
}

/// Runs on its own thread while the main one sits in the dialog's modal loop.
fn shoot_dialog(thread: u32, name: String, pages: usize, out_dir: PathBuf, print_check: bool) {
    unsafe {
        let mut checks_passed = true;
        let until = Instant::now() + Duration::from_secs(10);
        let dlg = loop {
            if let Some(dlg) = find_dialog(thread) {
                break dlg;
            }
            if Instant::now() > until {
                println!("no dialog");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        std::thread::sleep(Duration::from_millis(400));
        if print_check {
            let checked = |id| {
                SendMessageW(GetDlgItem(Some(dlg), id).unwrap(), BM_GETCHECK, None, None).0 == 1
            };
            let click = |id| {
                SendMessageW(GetDlgItem(Some(dlg), id).unwrap(), BM_CLICK, None, None);
            };
            let get = |id| {
                let mut buf = [0u16; 256];
                let len = GetWindowTextW(GetDlgItem(Some(dlg), id).unwrap(), &mut buf) as usize;
                String::from_utf16_lossy(&buf[..len])
            };
            let preferred_printer = get(100) == "Microsoft Print to PDF";
            let tc_margin = get(110).replace(',', ".") == "12.5";
            click(103);
            let landscape = checked(103) && !checked(102) && checked(171);
            let print = GetDlgItem(Some(dlg), IDOK.0).unwrap();
            let set = |id, text: &str| {
                let text = windows::core::HSTRING::from(text);
                SendMessageW(
                    GetDlgItem(Some(dlg), id).unwrap(),
                    WM_SETTEXT,
                    None,
                    Some(LPARAM(text.as_ptr() as isize)),
                );
            };
            set(110, "999");
            let invalid_margin_blocked = !IsWindowEnabled(print).as_bool();
            set(110, "12,5");
            let decimal_margin_accepted = IsWindowEnabled(print).as_bool();
            set(116, "0");
            let invalid_copies_blocked = !IsWindowEnabled(print).as_bool();
            set(116, "2");
            let valid_copies_accepted = IsWindowEnabled(print).as_bool();
            click(173);
            set(180, "");
            let empty_size_blocked = !IsWindowEnabled(print).as_bool();
            set(180, "999999999999");
            let huge_size_blocked = !IsWindowEnabled(print).as_bool();
            set(180, "999");
            let spin = GetDlgItem(Some(dlg), 184).unwrap();
            let delta = windows::Win32::UI::Controls::NMUPDOWN {
                hdr: windows::Win32::UI::Controls::NMHDR {
                    hwndFrom: spin,
                    idFrom: 184,
                    code: windows::Win32::UI::Controls::UDN_DELTAPOS,
                },
                iPos: 999,
                iDelta: 1,
            };
            SendMessageW(
                dlg,
                WM_NOTIFY,
                Some(WPARAM(184)),
                Some(LPARAM(&delta as *const _ as isize)),
            );
            let thousand_size_accepted = get(180) == "1000" && IsWindowEnabled(print).as_bool();
            set(180, "12");
            let width = GetDlgItem(Some(dlg), 180).unwrap();
            SendMessageW(width, EM_SETSEL, Some(WPARAM(2)), Some(LPARAM(2)));
            for ch in [',', '5'] {
                SendMessageW(width, WM_CHAR, Some(WPARAM(ch as usize)), None);
            }
            let decimal_typing_accepted = get(180) == "12,5" && IsWindowEnabled(print).as_bool();
            set(180, "100");
            set(181, "150");
            click(176);
            checks_passed = [
                preferred_printer,
                tc_margin,
                landscape,
                invalid_margin_blocked,
                decimal_margin_accepted,
                invalid_copies_blocked,
                valid_copies_accepted,
                empty_size_blocked,
                huge_size_blocked,
                thousand_size_accepted,
                decimal_typing_accepted,
            ]
            .into_iter()
            .all(|ok| ok);
            println!("print preview: printer={preferred_printer}, TC margins={tc_margin}, landscape={landscape}, invalid margin={invalid_margin_blocked}, decimal margin={decimal_margin_accepted}, invalid copies={invalid_copies_blocked}, valid copies={valid_copies_accepted}, empty size={empty_size_blocked}, huge size={huge_size_blocked}, 1000 mm={thousand_size_accepted}, decimal typing={decimal_typing_accepted}");
            let _ = InvalidateRect(Some(dlg), None, true);
            std::thread::sleep(Duration::from_millis(300));
        }
        for page in 0..pages.max(1) {
            if page > 0 {
                let _ = SetForegroundWindow(dlg);
                send_keys(&[(0x11, false), (0x09, false), (0x09, true), (0x11, true)]);
                std::thread::sleep(Duration::from_millis(300));
            }
            let mut rc = RECT::default();
            let _ = GetWindowRect(dlg, &mut rc);
            capture(rc, &out_dir.join(format!("{}_{}.png", name, page)));
        }
        let _ = PostMessageW(Some(dlg), WM_CLOSE, WPARAM(0), LPARAM(0));
        if !checks_passed {
            std::process::exit(2);
        }
    }
}

/// Exercises the actual preview -> worker -> PDF driver's filename dialog -> completion path.
fn print_pdf_check(thread: u32, out_dir: PathBuf, cancel: bool) {
    unsafe extern "system" fn find_save(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut process));
        let mut class = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut class) as usize;
        if process == std::process::id()
            && IsWindowVisible(hwnd).as_bool()
            && String::from_utf16_lossy(&class[..len]) == "#32770"
        {
            *(found.0 as *mut isize) = hwnd.0 as isize;
            return false.into();
        }
        true.into()
    }
    unsafe extern "system" fn find_edit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut class = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut class) as usize;
        if IsWindowVisible(hwnd).as_bool()
            && GetDlgCtrlID(hwnd) == 1001
            && String::from_utf16_lossy(&class[..len]) == "Edit"
        {
            *(found.0 as *mut isize) = hwnd.0 as isize;
            return false.into();
        }
        true.into()
    }
    unsafe {
        let deadline = Instant::now() + Duration::from_secs(20);
        let dlg = loop {
            if let Some(dlg) = find_dialog(thread) {
                break dlg;
            }
            if Instant::now() > deadline {
                eprintln!("PDF preview not found");
                std::process::exit(2);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        std::thread::sleep(Duration::from_millis(300));
        let print = GetDlgItem(Some(dlg), IDOK.0).unwrap();
        SendMessageW(print, BM_CLICK, None, None);
        let busy = !IsWindowEnabled(print).as_bool()
            && IsWindowEnabled(GetDlgItem(Some(dlg), IDCANCEL.0).unwrap()).as_bool();
        let save = loop {
            let mut found = 0isize;
            let _ = EnumWindows(Some(find_save), LPARAM(&mut found as *mut _ as isize));
            if found != 0 {
                break HWND(found as *mut _);
            }
            if Instant::now() > deadline {
                eprintln!("PDF filename dialog not found");
                std::process::exit(2);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if cancel {
            SendMessageW(save, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), None);
            while !IsWindowEnabled(print).as_bool() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let ok = busy
                && IsWindow(Some(dlg)).as_bool()
                && IsWindowEnabled(print).as_bool()
                && IsWindowEnabled(dlg).as_bool();
            println!("PDF cancellation: responsive={busy}, preview restored={ok}");
            let _ = PostMessageW(Some(dlg), WM_CLOSE, WPARAM(0), LPARAM(0));
            if !ok {
                std::process::exit(2);
            }
            return;
        }
        let mut edit = 0isize;
        let _ = EnumChildWindows(
            Some(save),
            Some(find_edit),
            LPARAM(&mut edit as *mut _ as isize),
        );
        if edit == 0 {
            eprintln!("PDF filename edit not found");
            std::process::exit(2);
        }
        let output = out_dir.join(format!("preview-{}.pdf", std::process::id()));
        let absolute: PathBuf = std::env::current_dir()
            .unwrap()
            .join(&output)
            .components()
            .collect();
        SetWindowTextW(
            HWND(edit as *mut _),
            &windows::core::HSTRING::from(absolute.as_os_str()),
        )
        .unwrap();
        SendMessageW(save, WM_COMMAND, Some(WPARAM(IDOK.0 as usize)), None);
        while IsWindow(Some(dlg)).as_bool() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let ok = busy
            && !IsWindow(Some(dlg)).as_bool()
            && std::fs::read(&output).is_ok_and(|b| b.starts_with(b"%PDF-"));
        println!(
            "PDF preview: responsive={busy}, completed={ok}, output={}",
            output.display()
        );
        if !ok {
            std::process::exit(2);
        }
    }
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
        let show_after_load = std::env::var_os("HARNESS_SHOW_AFTER_LOAD").is_some();
        let (ex_style, visible) = if show_after_load {
            (WINDOW_EX_STYLE(0), WINDOW_STYLE(0))
        } else {
            (WS_EX_TOPMOST, WS_VISIBLE)
        };
        let host = CreateWindowExW(
            ex_style,
            class,
            w!("Harness"),
            WS_OVERLAPPEDWINDOW | visible | WS_CLIPCHILDREN,
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

        let started = Instant::now();
        let viewer = ListLoadW(parent, wide(&file).as_ptr(), 0);
        println!(
            "ListLoadW -> {:?} in {} ms",
            viewer,
            started.elapsed().as_millis()
        );
        if viewer.is_invalid() {
            let _ = DestroyWindow(host);
            return;
        }
        if show_after_load {
            let _ = ShowWindow(host, SW_SHOWMAXIMIZED); // TC opens the Lister maximized (with its DWM animation)
            let _ = SetForegroundWindow(host);
        }

        for step in &args[2..] {
            let (cmd, arg) = step.split_once(':').unwrap_or((step, ""));
            match cmd {
                "wait" => pump(arg.parse().expect("ms")),
                "shot" => {
                    // A fullscreen viewer is a top-level popup (GetParent would return its owner).
                    let fullscreen =
                        (GetWindowLongPtrW(viewer, GWL_STYLE) as u32 & WS_CHILD.0) == 0;
                    let mut rc = RECT::default();
                    let _ = GetWindowRect(if fullscreen { viewer } else { host }, &mut rc);
                    capture(rc, &out_dir.join(format!("{}.png", arg)));
                }
                "key" => {
                    let vk = usize::from_str_radix(arg, 16).expect("hex vk");
                    let _ = PostMessageW(Some(viewer), WM_KEYDOWN, WPARAM(vk), LPARAM(0));
                    pump(50);
                }
                "hold" => {
                    let (vk, n) = arg.split_once(',').expect("vk,n");
                    let vk = usize::from_str_radix(vk, 16).expect("hex vk");
                    let n: usize = n.parse().expect("repeats");
                    let started = Instant::now();
                    let _ = PostMessageW(Some(viewer), WM_KEYDOWN, WPARAM(vk), LPARAM(1));
                    for _ in 0..n {
                        // Bit 30: the key was already down.
                        let _ =
                            PostMessageW(Some(viewer), WM_KEYDOWN, WPARAM(vk), LPARAM(1 << 30 | 1));
                    }
                    let _ = PostMessageW(Some(viewer), WM_KEYUP, WPARAM(vk), LPARAM(3 << 30 | 1));
                    let done = WM_APP + 0x3F;
                    let _ = PostMessageW(Some(host), done, WPARAM(0), LPARAM(0));
                    let mut msg = MSG::default();
                    'queue: loop {
                        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                            if msg.message == done {
                                break 'queue;
                            }
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    println!(
                        "hold {n} repeats: queue done in {} ms",
                        started.elapsed().as_millis()
                    );
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
                "steal" => {
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(host);
                    pump(50);
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                    pump(50);
                }
                "ldown" | "drag" | "lup" => {
                    let (x, y) = xy(arg);
                    let lp = LPARAM(((y as isize) << 16) | (x as isize & 0xFFFF));
                    let (msg, keys) = match cmd {
                        "ldown" => (WM_LBUTTONDOWN, 1),
                        "drag" => (WM_MOUSEMOVE, 1),
                        _ => (WM_LBUTTONUP, 0),
                    };
                    let _ = PostMessageW(Some(viewer), msg, WPARAM(keys), lp);
                    pump(50);
                }
                "wheel" => {
                    // wheel:<delta>[,<x>,<y>] — the point in the viewer's client coordinates.
                    let (delta, at) = arg.split_once(',').unwrap_or((arg, "0,0"));
                    let delta: i16 = delta.parse().expect("delta");
                    let (x, y) = xy(at);
                    let mut pt = windows::Win32::Foundation::POINT { x, y };
                    let _ = windows::Win32::Graphics::Gdi::ClientToScreen(viewer, &mut pt);
                    let lp = LPARAM(((pt.y as isize) << 16) | (pt.x as isize & 0xFFFF));
                    let _ = PostMessageW(
                        Some(viewer),
                        WM_MOUSEWHEEL,
                        WPARAM((delta as u16 as usize) << 16),
                        lp,
                    );
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
                    let target = if which == "top" {
                        strips.first()
                    } else {
                        strips.last()
                    };
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
                "print" | "print-check" | "print-pdf-check" | "print-pdf-cancel-check" => {
                    let print_worker = if cmd == "print-check" {
                        let thread = windows::Win32::System::Threading::GetCurrentThreadId();
                        let out = out_dir.clone();
                        Some(std::thread::spawn(move || {
                            shoot_dialog(thread, "landscape".into(), 1, out, true)
                        }))
                    } else if cmd == "print-pdf-check" || cmd == "print-pdf-cancel-check" {
                        let thread = windows::Win32::System::Threading::GetCurrentThreadId();
                        let out = out_dir.clone();
                        let cancel = cmd == "print-pdf-cancel-check";
                        Some(std::thread::spawn(move || {
                            print_pdf_check(thread, out, cancel)
                        }))
                    } else {
                        None
                    };
                    let printer: Vec<u16> = "Microsoft Print to PDF\0".encode_utf16().collect();
                    let margins = RECT {
                        left: 100,
                        top: 125,
                        right: 100,
                        bottom: 100,
                    };
                    let result = ListPrintW(
                        viewer,
                        std::ptr::null(),
                        if cmd != "print" {
                            printer.as_ptr()
                        } else {
                            std::ptr::null()
                        },
                        0,
                        if cmd == "print-check" {
                            &margins
                        } else {
                            std::ptr::null()
                        },
                    );
                    println!("ListPrintW -> {result}");
                    if let Some(worker) = print_worker {
                        worker.join().unwrap();
                    }
                }
                "thumb" => {
                    let parts: Vec<&str> = arg.splitn(4, ',').collect();
                    let (w, h) = (parts[0].parse().expect("w"), parts[1].parse().expect("h"));
                    let target = parts
                        .get(3)
                        .map(PathBuf::from)
                        .unwrap_or_else(|| file.clone());
                    let started = Instant::now();
                    let bitmap =
                        ListGetPreviewBitmapW(wide(&target).as_ptr(), w, h, std::ptr::null(), 0);
                    println!(
                        "ListGetPreviewBitmapW({}) in {} ms",
                        target.display(),
                        started.elapsed().as_millis()
                    );
                    if bitmap.is_invalid() {
                        println!("no preview bitmap");
                    } else {
                        save_hbitmap(bitmap, &out_dir.join(format!("{}.png", parts[2])));
                    }
                }
                "dialog" => {
                    let (name, pages) = arg.split_once(',').unwrap_or((arg, "1"));
                    let (name, pages) = (name.to_string(), pages.parse().expect("pages"));
                    let thread = windows::Win32::System::Threading::GetCurrentThreadId();
                    let out = out_dir.clone();
                    std::thread::spawn(move || shoot_dialog(thread, name, pages, out, false));
                }
                other => panic!("unknown step {}", other),
            }
        }

        ListCloseWindow(viewer);
        let _ = DestroyWindow(host);
        pump(100);
    }
}
