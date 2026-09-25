//! Win32 window management, double-buffered GDI rendering, and event dispatch.

use std::mem::size_of;
use std::path::Path;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, SelectObject, SetStretchBltMode, StretchDIBits,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HALFTONE, HDC,
    PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, GetParent, GetWindowLongPtrW,
    PostMessageW, RegisterClassExW, SetWindowLongPtrW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_MOUSEWHEEL, WM_PAINT, WM_SIZE, WM_XBUTTONDOWN, WNDCLASSEXW,
    WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE,
};

use crate::wlx_state::ViewerState;

const WINDOW_CLASS_NAME: PCWSTR = w!("MediaresViewerClass");

pub unsafe fn ensure_window_class_registered() {
    let hinstance = GetModuleHandleW(None).unwrap_or_default();

    let mut wc = WNDCLASSEXW::default();
    wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
    wc.style = CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS;
    wc.lpfnWndProc = Some(wlx_wnd_proc);
    wc.hInstance = hinstance.into();
    wc.hbrBackground = windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut());
    wc.lpszClassName = WINDOW_CLASS_NAME;

    let _ = RegisterClassExW(&wc);
}

pub unsafe fn create_viewer_window(parent: HWND, file_path: &Path) -> Option<HWND> {
    ensure_window_class_registered();

    let mut rect = RECT::default();
    GetClientRect(parent, &mut rect).ok()?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    let hinstance = GetModuleHandleW(None).unwrap_or_default();

    let hwnd = CreateWindowExW(
        windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
        WINDOW_CLASS_NAME,
        w!("MediaresViewer"),
        WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
        0,
        0,
        width,
        height,
        Some(parent),
        None,
        Some(hinstance.into()),
        None,
    ).ok()?;

    let state = Box::new(ViewerState::new(hwnd, file_path));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);

    // Immediately acquire keyboard focus so Lister navigation hotkeys work without clicking
    let _ = SetFocus(Some(hwnd));

    Some(hwnd)
}

pub unsafe fn destroy_viewer_window(hwnd: HWND) {
    let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    if ptr != 0 {
        let _ = Box::from_raw(ptr as *mut ViewerState);
    }
}

pub unsafe fn get_viewer_state<'a>(hwnd: HWND) -> Option<&'a mut ViewerState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if ptr == 0 {
        None
    } else {
        Some(&mut *(ptr as *mut ViewerState))
    }
}

/// Send Next ('N') or Previous ('P') file navigation key message to Lister parent window.
unsafe fn send_nav_to_parent(hwnd: HWND, next: bool) {
    if let Ok(parent) = GetParent(hwnd) {
        if !parent.is_invalid() {
            let vk = if next { 0x4Eu32 } else { 0x50u32 }; // 'N' = 0x4E, 'P' = 0x50
            let _ = PostMessageW(Some(parent), WM_KEYDOWN, WPARAM(vk as usize), LPARAM(0));
            let _ = PostMessageW(Some(parent), WM_KEYUP, WPARAM(vk as usize), LPARAM(0));
        }
    }
}

unsafe extern "system" fn wlx_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => {
            // Prevent GDI flickering by handling background clearing inside double-buffered paint
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if !hdc.is_invalid() {
                paint_viewer(hwnd, hdc);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // Set focus on click so hotkeys continue working
            let _ = SetFocus(Some(hwnd));
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // Wheel down -> Next file, Wheel up -> Previous file
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;
            if delta < 0 {
                send_nav_to_parent(hwnd, true);
            } else if delta > 0 {
                send_nav_to_parent(hwnd, false);
            }
            LRESULT(0)
        }
        WM_XBUTTONDOWN => {
            // Mouse side buttons: forward -> Next file, back -> Previous file
            let btn = ((wparam.0 >> 16) & 0xFFFF) as u16;
            if btn == 1 {
                send_nav_to_parent(hwnd, false);
            } else if btn == 2 {
                send_nav_to_parent(hwnd, true);
            }
            LRESULT(1)
        }
        WM_KEYDOWN => {
            let vk = wparam.0 as usize;
            match vk {
                // Next file triggers:
                // 'N' (0x4E), Space (0x20), Right Arrow (0x27), PageDown (0x22)
                0x4E | 0x20 | 0x27 | 0x22 => {
                    send_nav_to_parent(hwnd, true);
                    LRESULT(0)
                }
                // Previous file triggers:
                // 'P' (0x50), Backspace (0x08), Left Arrow (0x25), PageUp (0x21)
                0x50 | 0x08 | 0x25 | 0x21 => {
                    send_nav_to_parent(hwnd, false);
                    LRESULT(0)
                }
                // Forward all other keys (Esc, 1..7, F3, etc.) to parent Lister window
                _ => {
                    if let Ok(parent) = GetParent(hwnd) {
                        if !parent.is_invalid() {
                            let _ = PostMessageW(Some(parent), WM_KEYDOWN, wparam, lparam);
                        }
                    }
                    LRESULT(0)
                }
            }
        }
        WM_DESTROY => {
            destroy_viewer_window(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn paint_viewer(hwnd: HWND, hdc: HDC) {
    let mut client_rect = RECT::default();
    if GetClientRect(hwnd, &mut client_rect).is_err() {
        return;
    }

    let win_w = client_rect.right - client_rect.left;
    let win_h = client_rect.bottom - client_rect.top;
    if win_w <= 0 || win_h <= 0 {
        return;
    }

    // Double buffering: Create offscreen memory DC and compatible bitmap
    let mem_dc = CreateCompatibleDC(Some(hdc));
    if mem_dc.is_invalid() {
        return;
    }
    let mem_bmp = CreateCompatibleBitmap(hdc, win_w, win_h);
    if mem_bmp.is_invalid() {
        let _ = DeleteDC(mem_dc);
        return;
    }
    let old_bmp = SelectObject(mem_dc, mem_bmp.into());

    // Dark sleek background (0x181818)
    let bg_brush = CreateSolidBrush(COLORREF(0x00181818));
    FillRect(mem_dc, &client_rect, bg_brush);
    let _ = DeleteObject(bg_brush.into());

    if let Some(state) = get_viewer_state(hwnd) {
        if let Some(ref img) = state.image {
            // Letterbox fit calculation
            let (dst_x, dst_y, dst_w, dst_h) = calculate_letterbox(
                img.width, img.height, win_w as u32, win_h as u32,
            );

            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = img.width as i32;
            bmi.bmiHeader.biHeight = -(img.height as i32); // Top-down DIB
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;

            SetStretchBltMode(mem_dc, HALFTONE);

            let _ = StretchDIBits(
                mem_dc,
                dst_x,
                dst_y,
                dst_w,
                dst_h,
                0,
                0,
                img.width as i32,
                img.height as i32,
                Some(img.bgra_pixels.as_ptr() as *const _),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }

    // Blit to screen in one single operation
    let _ = BitBlt(hdc, 0, 0, win_w, win_h, Some(mem_dc), 0, 0, SRCCOPY);

    SelectObject(mem_dc, old_bmp);
    let _ = DeleteObject(mem_bmp.into());
    let _ = DeleteDC(mem_dc);
}

fn calculate_letterbox(src_w: u32, src_h: u32, win_w: u32, win_h: u32) -> (i32, i32, i32, i32) {
    if src_w == 0 || src_h == 0 || win_w == 0 || win_h == 0 {
        return (0, 0, win_w as i32, win_h as i32);
    }

    let scale_x = win_w as f64 / src_w as f64;
    let scale_y = win_h as f64 / src_h as f64;
    let scale = scale_x.min(scale_y);

    let dst_w = (src_w as f64 * scale).round() as i32;
    let dst_h = (src_h as f64 * scale).round() as i32;
    let dst_x = (win_w as i32 - dst_w) / 2;
    let dst_y = (win_h as i32 - dst_h) / 2;

    (dst_x, dst_y, dst_w, dst_h)
}
