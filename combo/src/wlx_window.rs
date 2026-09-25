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
    PostMessageW, RegisterClassExW, SetWindowLongPtrW, SetWindowTextW,
    CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA,
    WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
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

    let state = Box::new(ViewerState::new(hwnd, parent, file_path));
    update_lister_title(parent, &state);
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);

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

pub unsafe fn update_lister_title(parent: HWND, state: &ViewerState) {
    if parent.is_invalid() {
        return;
    }
    let name = state.file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let dim_str = if let Some(ref img) = state.image {
        format!(" ({}x{})", img.width, img.height)
    } else {
        String::new()
    };
    let total = state.dir_files.len();
    let idx = state.current_idx + 1;
    let title_str = if total > 1 {
        format!("{} [{}/{}]{} - Lister\0", name, idx, total, dim_str)
    } else {
        format!("{}{} - Lister\0", name, dim_str)
    };
    let title_w: Vec<u16> = title_str.encode_utf16().collect();
    let _ = SetWindowTextW(parent, PCWSTR(title_w.as_ptr()));
}

pub unsafe fn navigate_viewer(hwnd: HWND, next: bool) {
    let Some(state) = get_viewer_state(hwnd) else { return };
    if state.dir_files.len() <= 1 {
        return;
    }

    let total = state.dir_files.len();
    let new_idx = if next {
        (state.current_idx + 1) % total
    } else {
        (state.current_idx + total - 1) % total
    };

    let next_path = state.dir_files[new_idx].clone();
    state.set_file(&next_path);

    update_lister_title(state.parent_hwnd, state);
    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, true);
}

unsafe extern "system" fn wlx_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => {
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
            let _ = SetFocus(Some(hwnd));
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            // Forward double click to parent Lister to toggle full screen
            if let Ok(parent) = GetParent(hwnd) {
                if !parent.is_invalid() {
                    let _ = PostMessageW(Some(parent), WM_LBUTTONDBLCLK, wparam, lparam);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;
            if delta < 0 {
                navigate_viewer(hwnd, true);
            } else if delta > 0 {
                navigate_viewer(hwnd, false);
            }
            LRESULT(0)
        }
        WM_XBUTTONDOWN => {
            let btn = ((wparam.0 >> 16) & 0xFFFF) as u16;
            if btn == 1 {
                navigate_viewer(hwnd, false);
            } else if btn == 2 {
                navigate_viewer(hwnd, true);
            }
            LRESULT(1)
        }
        WM_KEYDOWN => {
            let vk = wparam.0 as usize;
            match vk {
                // Next file: 'N' (0x4E), Space (0x20), Right Arrow (0x27), PageDown (0x22), Down Arrow (0x28)
                0x4E | 0x20 | 0x27 | 0x22 | 0x28 => {
                    navigate_viewer(hwnd, true);
                    LRESULT(0)
                }
                // Previous file: 'P' (0x50), Backspace (0x08), Left Arrow (0x25), PageUp (0x21), Up Arrow (0x26)
                0x50 | 0x08 | 0x25 | 0x21 | 0x26 => {
                    navigate_viewer(hwnd, false);
                    LRESULT(0)
                }
                // Escape (0x1B) or other keys: forward to parent Lister window
                0x1B => {
                    if let Ok(parent) = GetParent(hwnd) {
                        if !parent.is_invalid() {
                            let _ = PostMessageW(Some(parent), WM_KEYDOWN, wparam, lparam);
                        }
                    }
                    LRESULT(0)
                }
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
