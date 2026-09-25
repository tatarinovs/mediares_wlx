//! Win32 window management, double-buffered GDI rendering, and event dispatch.

use std::mem::size_of;
use std::path::Path;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    InvalidateRect,
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, GetMonitorInfoW, MonitorFromWindow, ScreenToClient,
    SelectObject, SetStretchBltMode, StretchDIBits, UpdateWindow,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HALFTONE, HDC,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetAncestor, GetClientRect, GetMenu, GetParent,
    GetWindowLongPtrW, GetWindowPlacement, LoadCursorW, PostMessageW, RegisterClassExW,
    SetCursor, SetMenu, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos, SetWindowTextW,
    CS_DBLCLKS, GA_ROOT, GWL_STYLE, GWLP_USERDATA, IDC_ARROW, IDC_HAND, IDC_SIZEALL,
    SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    WINDOWPLACEMENT, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SETCURSOR,
    WM_SIZE, WM_XBUTTONDOWN, WNDCLASSEXW,
    WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_MAXIMIZEBOX,
    WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME, WS_VISIBLE,
};

use crate::wlx_state::{ViewerState, ZoomMode};

const WINDOW_CLASS_NAME: PCWSTR = w!("MediaresViewerClass");

pub unsafe fn ensure_window_class_registered() {
    let hinstance = GetModuleHandleW(None).unwrap_or_default();

    let bg_brush = CreateSolidBrush(COLORREF(0x00181818));

    let mut wc = WNDCLASSEXW::default();
    wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
    wc.style = CS_DBLCLKS;
    wc.lpfnWndProc = Some(wlx_wnd_proc);
    wc.hInstance = hinstance.into();
    wc.hbrBackground = bg_brush;
    wc.lpszClassName = WINDOW_CLASS_NAME;

    let _ = RegisterClassExW(&wc);
}

pub unsafe fn create_viewer_window(parent: HWND, file_path: &Path) -> Option<HWND> {
    ensure_window_class_registered();

    // Prevent Delphi parent white flicker by ensuring WS_CLIPCHILDREN
    let p_style = GetWindowLongPtrW(parent, GWL_STYLE) as u32;
    if (p_style & WS_CLIPCHILDREN.0) == 0 {
        let _ = SetWindowLongPtrW(parent, GWL_STYLE, (p_style | WS_CLIPCHILDREN.0) as isize);
    }
    let top_win = GetAncestor(parent, GA_ROOT);
    if !top_win.is_invalid() && top_win != parent {
        let t_style = GetWindowLongPtrW(top_win, GWL_STYLE) as u32;
        if (t_style & WS_CLIPCHILDREN.0) == 0 {
            let _ = SetWindowLongPtrW(top_win, GWL_STYLE, (t_style | WS_CLIPCHILDREN.0) as isize);
        }
    }

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
        let state = Box::from_raw(ptr as *mut ViewerState);
        if state.is_fullscreen {
            let top_win = GetAncestor(state.parent_hwnd, GA_ROOT);
            let target = if !top_win.is_invalid() { top_win } else { state.parent_hwnd };
            let _ = SetWindowLongPtrW(target, GWL_STYLE, state.saved_style);
            if !state.saved_menu.is_invalid() {
                let _ = SetMenu(target, Some(state.saved_menu));
            }
            if let Some(ref wp) = state.saved_placement {
                let _ = SetWindowPlacement(target, wp);
            }
        }
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
        let zoom_str = match state.zoom_mode {
            ZoomMode::Fit => "Fit".to_string(),
            ZoomMode::Custom(s) => format!("{:.0}%", s * 100.0),
        };
        format!(" ({}x{}, {})", img.width, img.height, zoom_str)
    } else {
        String::new()
    };
    let total = state.dir_files.len();
    let idx = state.current_idx + 1;
    let fs_str = if state.is_fullscreen { " [Fullscreen]" } else { "" };
    let title_str = if total > 1 {
        format!("{} [{}/{}]{}{} - Lister\0", name, idx, total, dim_str, fs_str)
    } else {
        format!("{}{}{} - Lister\0", name, dim_str, fs_str)
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

    let _ = InvalidateRect(Some(hwnd), None, false);
    let _ = UpdateWindow(hwnd);

    update_lister_title(state.parent_hwnd, state);
}

pub unsafe fn toggle_fullscreen(hwnd: HWND) {
    let Some(state) = get_viewer_state(hwnd) else { return };
    let top_win = GetAncestor(state.parent_hwnd, GA_ROOT);
    let target = if !top_win.is_invalid() { top_win } else { state.parent_hwnd };

    if !state.is_fullscreen {
        // Entering Fullscreen
        let mut wp = WINDOWPLACEMENT::default();
        wp.length = size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(target, &mut wp).is_ok() {
            state.saved_placement = Some(wp);
        }

        let style = GetWindowLongPtrW(target, GWL_STYLE);
        state.saved_style = style;

        let menu = GetMenu(target);
        state.saved_menu = menu;
        if !menu.is_invalid() {
            let _ = SetMenu(target, None);
        }

        let hmon = MonitorFromWindow(target, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO::default();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            let new_style = (style as u32) & !(WS_CAPTION.0 | WS_THICKFRAME.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0 | WS_SYSMENU.0);
            let _ = SetWindowLongPtrW(target, GWL_STYLE, new_style as isize);

            let rc = mi.rcMonitor;
            let _ = SetWindowPos(
                target,
                Some(HWND::default()),
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                SWP_NOZORDER | SWP_FRAMECHANGED,
            );

            // Resize viewer window to fill new client rect
            let mut client_rc = RECT::default();
            if GetClientRect(target, &mut client_rc).is_ok() {
                let w = client_rc.right - client_rc.left;
                let h = client_rc.bottom - client_rc.top;
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND::default()),
                    0,
                    0,
                    w,
                    h,
                    SWP_NOZORDER | SWP_FRAMECHANGED,
                );
            }

            state.is_fullscreen = true;
        }
    } else {
        // Exiting Fullscreen
        let _ = SetWindowLongPtrW(target, GWL_STYLE, state.saved_style);
        if !state.saved_menu.is_invalid() {
            let _ = SetMenu(target, Some(state.saved_menu));
        }
        if let Some(ref wp) = state.saved_placement {
            let _ = SetWindowPlacement(target, wp);
        }
        let _ = SetWindowPos(
            target,
            Some(HWND::default()),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );

        let mut client_rc = RECT::default();
        if GetClientRect(target, &mut client_rc).is_ok() {
            let w = client_rc.right - client_rc.left;
            let h = client_rc.bottom - client_rc.top;
            let _ = SetWindowPos(
                hwnd,
                Some(HWND::default()),
                0,
                0,
                w,
                h,
                SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }

        state.is_fullscreen = false;
    }

    update_lister_title(state.parent_hwnd, state);
    let _ = InvalidateRect(Some(hwnd), None, false);
    let _ = UpdateWindow(hwnd);
}

pub unsafe fn perform_zoom(hwnd: HWND, zoom_in: bool, center_x: f32, center_y: f32) {
    let Some(state) = get_viewer_state(hwnd) else { return };
    let Some(ref img) = state.image else { return };

    let mut client_rc = RECT::default();
    if GetClientRect(hwnd, &mut client_rc).is_err() { return; }
    let win_w = (client_rc.right - client_rc.left) as f32;
    let win_h = (client_rc.bottom - client_rc.top) as f32;
    if win_w <= 0.0 || win_h <= 0.0 { return; }

    let fit_scale_x = win_w / img.width as f32;
    let fit_scale_y = win_h / img.height as f32;
    let fit_scale = fit_scale_x.min(fit_scale_y);

    let (current_scale, cur_off_x, cur_off_y) = match state.zoom_mode {
        ZoomMode::Fit => {
            let dst_w = (img.width as f32 * fit_scale).round();
            let dst_h = (img.height as f32 * fit_scale).round();
            let dst_x = ((win_w - dst_w) / 2.0).round();
            let dst_y = ((win_h - dst_h) / 2.0).round();
            (fit_scale, dst_x, dst_y)
        }
        ZoomMode::Custom(s) => (s, state.offset_x, state.offset_y),
    };

    let factor = if zoom_in { 1.25 } else { 0.8 };
    let new_scale = (current_scale * factor).clamp(0.05, 50.0);

    // If zooming out and very close to fit scale, snap back to Fit
    if (new_scale - fit_scale).abs() / fit_scale < 0.04 && !zoom_in {
        state.zoom_mode = ZoomMode::Fit;
        state.offset_x = 0.0;
        state.offset_y = 0.0;
    } else {
        // Zoom centered at cursor
        let img_coord_x = (center_x - cur_off_x) / current_scale;
        let img_coord_y = (center_y - cur_off_y) / current_scale;
        let new_off_x = center_x - img_coord_x * new_scale;
        let new_off_y = center_y - img_coord_y * new_scale;

        state.zoom_mode = ZoomMode::Custom(new_scale);
        state.offset_x = new_off_x;
        state.offset_y = new_off_y;
    }

    let _ = InvalidateRect(Some(hwnd), None, false);
    let _ = UpdateWindow(hwnd);
    update_lister_title(state.parent_hwnd, state);
}

pub unsafe fn reset_zoom_fit(hwnd: HWND) {
    let Some(state) = get_viewer_state(hwnd) else { return };
    state.zoom_mode = ZoomMode::Fit;
    state.offset_x = 0.0;
    state.offset_y = 0.0;
    state.is_dragging = false;

    let _ = InvalidateRect(Some(hwnd), None, false);
    let _ = UpdateWindow(hwnd);
    update_lister_title(state.parent_hwnd, state);
}

pub unsafe fn reset_zoom_100(hwnd: HWND) {
    let Some(state) = get_viewer_state(hwnd) else { return };
    let Some(ref img) = state.image else { return };

    let mut client_rc = RECT::default();
    if GetClientRect(hwnd, &mut client_rc).is_err() { return; }
    let win_w = (client_rc.right - client_rc.left) as f32;
    let win_h = (client_rc.bottom - client_rc.top) as f32;

    state.zoom_mode = ZoomMode::Custom(1.0);
    state.offset_x = ((win_w - img.width as f32) / 2.0).round();
    state.offset_y = ((win_h - img.height as f32) / 2.0).round();
    state.is_dragging = false;

    let _ = InvalidateRect(Some(hwnd), None, false);
    let _ = UpdateWindow(hwnd);
    update_lister_title(state.parent_hwnd, state);
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
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let _ = SetFocus(Some(hwnd));
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;

            if let Some(state) = get_viewer_state(hwnd) {
                if state.zoom_mode == ZoomMode::Fit {
                    let mut client_rc = RECT::default();
                    if GetClientRect(hwnd, &mut client_rc).is_ok() {
                        if let Some(ref img) = state.image {
                            let win_w = (client_rc.right - client_rc.left) as f32;
                            let win_h = (client_rc.bottom - client_rc.top) as f32;
                            let fit_scale = (win_w / img.width as f32).min(win_h / img.height as f32);
                            let dst_w = (img.width as f32 * fit_scale).round();
                            let dst_h = (img.height as f32 * fit_scale).round();
                            state.offset_x = ((win_w - dst_w) / 2.0).round();
                            state.offset_y = ((win_h - dst_h) / 2.0).round();
                        }
                    }
                }

                state.is_dragging = true;
                state.drag_start_x = x;
                state.drag_start_y = y;
                state.drag_start_offset_x = state.offset_x;
                state.drag_start_offset_y = state.offset_y;
                let _ = SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;

            if let Some(state) = get_viewer_state(hwnd) {
                if state.is_dragging {
                    let dx = x - state.drag_start_x;
                    let dy = y - state.drag_start_y;

                    if state.zoom_mode == ZoomMode::Fit {
                        let mut client_rc = RECT::default();
                        if GetClientRect(hwnd, &mut client_rc).is_ok() {
                            if let Some(ref img) = state.image {
                                let win_w = (client_rc.right - client_rc.left) as f32;
                                let win_h = (client_rc.bottom - client_rc.top) as f32;
                                let fit_scale = (win_w / img.width as f32).min(win_h / img.height as f32);
                                state.zoom_mode = ZoomMode::Custom(fit_scale);
                            }
                        }
                    }

                    state.offset_x = state.drag_start_offset_x + dx as f32;
                    state.offset_y = state.drag_start_offset_y + dy as f32;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    let _ = UpdateWindow(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = get_viewer_state(hwnd) {
                if state.is_dragging {
                    state.is_dragging = false;
                    let _ = ReleaseCapture();
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            toggle_fullscreen(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if let Some(state) = get_viewer_state(hwnd) {
                let cursor_id = if state.is_dragging {
                    IDC_SIZEALL
                } else if state.zoom_mode != ZoomMode::Fit {
                    IDC_HAND
                } else {
                    IDC_ARROW
                };
                if let Ok(c) = LoadCursorW(None, cursor_id) {
                    let _ = SetCursor(Some(c));
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_MOUSEWHEEL => {
            let ctrl_down = (GetKeyState(VK_CONTROL.0 as i32) as i16) < 0;
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;

            if ctrl_down {
                let mut pt = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut pt);
                perform_zoom(hwnd, delta > 0, pt.x as f32, pt.y as f32);
            } else {
                if delta < 0 {
                    navigate_viewer(hwnd, true);
                } else if delta > 0 {
                    navigate_viewer(hwnd, false);
                }
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
                // Enter (0x0D), F (0x46), or F11 (0x7A): Toggle Fullscreen
                0x0D | 0x46 | 0x7A => {
                    toggle_fullscreen(hwnd);
                    LRESULT(0)
                }
                // Escape (0x1B): If in fullscreen -> exit fullscreen; else close Lister
                0x1B => {
                    if let Some(state) = get_viewer_state(hwnd) {
                        if state.is_fullscreen {
                            toggle_fullscreen(hwnd);
                            return LRESULT(0);
                        }
                    }
                    if let Ok(parent) = GetParent(hwnd) {
                        if !parent.is_invalid() {
                            let _ = PostMessageW(Some(parent), WM_KEYDOWN, wparam, lparam);
                        }
                    }
                    LRESULT(0)
                }
                // Zoom In: '+' (0xBB = VK_OEM_PLUS, 0x6B = VK_ADD)
                0xBB | 0x6B => {
                    let mut rc = RECT::default();
                    if GetClientRect(hwnd, &mut rc).is_ok() {
                        let cx = ((rc.right - rc.left) / 2) as f32;
                        let cy = ((rc.bottom - rc.top) / 2) as f32;
                        perform_zoom(hwnd, true, cx, cy);
                    }
                    LRESULT(0)
                }
                // Zoom Out: '-' (0xBD = VK_OEM_MINUS, 0x6D = VK_SUBTRACT)
                0xBD | 0x6D => {
                    let mut rc = RECT::default();
                    if GetClientRect(hwnd, &mut rc).is_ok() {
                        let cx = ((rc.right - rc.left) / 2) as f32;
                        let cy = ((rc.bottom - rc.top) / 2) as f32;
                        perform_zoom(hwnd, false, cx, cy);
                    }
                    LRESULT(0)
                }
                // 100% Size: '1' (0x31) or Numpad 1 (0x61)
                0x31 | 0x61 => {
                    reset_zoom_100(hwnd);
                    LRESULT(0)
                }
                // Fit to Window: '0' (0x30), Numpad 0 (0x60), '*' (0x6A), '/' (0x6F)
                0x30 | 0x60 | 0x6A | 0x6F => {
                    reset_zoom_fit(hwnd);
                    LRESULT(0)
                }
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

    let bg_brush = CreateSolidBrush(COLORREF(0x00181818));
    FillRect(mem_dc, &client_rect, bg_brush);
    let _ = DeleteObject(bg_brush.into());

    if let Some(state) = get_viewer_state(hwnd) {
        if let Some(ref img) = state.image {
            let (dst_x, dst_y, dst_w, dst_h) = calculate_image_rect(
                img.width, img.height, win_w as u32, win_h as u32, state,
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

    let _ = BitBlt(hdc, 0, 0, win_w, win_h, Some(mem_dc), 0, 0, SRCCOPY);

    SelectObject(mem_dc, old_bmp);
    let _ = DeleteObject(mem_bmp.into());
    let _ = DeleteDC(mem_dc);
}

fn calculate_image_rect(img_w: u32, img_h: u32, win_w: u32, win_h: u32, state: &ViewerState) -> (i32, i32, i32, i32) {
    if img_w == 0 || img_h == 0 || win_w == 0 || win_h == 0 {
        return (0, 0, win_w as i32, win_h as i32);
    }

    let fit_scale_x = win_w as f32 / img_w as f32;
    let fit_scale_y = win_h as f32 / img_h as f32;
    let fit_scale = fit_scale_x.min(fit_scale_y);

    match state.zoom_mode {
        ZoomMode::Fit => {
            let dst_w = (img_w as f32 * fit_scale).round();
            let dst_h = (img_h as f32 * fit_scale).round();
            let dst_x = ((win_w as f32 - dst_w) / 2.0).round();
            let dst_y = ((win_h as f32 - dst_h) / 2.0).round();
            (dst_x as i32, dst_y as i32, dst_w as i32, dst_h as i32)
        }
        ZoomMode::Custom(scale) => {
            let dst_w = (img_w as f32 * scale).round();
            let dst_h = (img_h as f32 * scale).round();
            let dst_x = state.offset_x.round();
            let dst_y = state.offset_y.round();
            (dst_x as i32, dst_y as i32, dst_w as i32, dst_h as i32)
        }
    }
}
