//! Lister viewer window: creation, message handling and user commands.
//!
//! State lives in a `Box<ViewerState>` behind `GWLP_USERDATA`. Handlers fetch it per message and
//! never hold it across calls that pump messages (menus, dialogs), re-fetching afterwards.

use std::path::Path;
use std::sync::Once;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, ClientToScreen, EndPaint, InvalidateRect, ScreenToClient, UpdateWindow, PAINTSTRUCT};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetClassNameW, GetClientRect, GetWindowLongPtrW, LoadCursorW, PostMessageW, RegisterClassExW,
    SetCursor, SetWindowLongPtrW, SetWindowTextW, TrackPopupMenu, CS_DBLCLKS, GWLP_USERDATA,
    IDC_ARROW, IDC_HAND, IDC_SIZEALL, MENU_ITEM_FLAGS, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    MF_UNCHECKED, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCDESTROY, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_SIZE, WM_XBUTTONDOWN,
    WNDCLASSEXW, WS_CHILD, WS_VISIBLE,
};

use crate::config::ViewerConfig;
use crate::image_view::{self, client_size, point_from_lparam};
use crate::state::{Drag, ViewerState, ZoomMode};
use crate::{dialog, exif_dialog, fullscreen, module, settings_dialog};

const CLASS_NAME: PCWSTR = w!("MediaresListerViewerClass");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Command {
    ToggleFullscreen = 1,
    ToggleOsd,
    ShowExif,
    ShowSettings,
    Next,
    Previous,
    ZoomIn,
    ZoomOut,
    ZoomActualSize,
    ZoomFit,
}

impl Command {
    const MENU: &[Option<(Command, &'static str)>] = &[
        Some((Command::ToggleFullscreen, "Полноэкранный режим\tEnter / F")),
        Some((Command::ToggleOsd, "Отображать OSD\tO")),
        None,
        Some((Command::ShowExif, "Просмотр EXIF...\tE")),
        Some((Command::ShowSettings, "Настройки...\tS")),
        None,
        Some((Command::Next, "Следующий файл\tПробел / Right")),
        Some((Command::Previous, "Предыдущий файл\tBackspace / Left")),
    ];

    fn from_id(id: i32) -> Option<Command> {
        Command::MENU.iter().flatten().map(|&(c, _)| c).find(|&c| c as i32 == id)
    }

    /// Hotkeys. With Ctrl held only zoom keys are ours; everything else goes to the Lister
    /// (Ctrl+P print, Ctrl+C copy, ...).
    fn from_key(vk: u16, ctrl: bool) -> Option<Command> {
        use Command::*;
        let zoom = match vk {
            0xBB | 0x6B => Some(ZoomIn),          // '+' / numpad +
            0xBD | 0x6D => Some(ZoomOut),         // '-' / numpad -
            0x30 | 0x60 => Some(ZoomFit),         // '0' / numpad 0
            _ => None,
        };
        if ctrl {
            return zoom;
        }
        zoom.or(match vk {
            0x0D | 0x46 | 0x7A => Some(ToggleFullscreen),            // Enter, F, F11
            0x4F | 0x49 => Some(ToggleOsd),                          // O, I
            0x45 => Some(ShowExif),                                  // E
            0x53 => Some(ShowSettings),                              // S
            0x31 | 0x61 => Some(ZoomActualSize),                     // '1' / numpad 1
            0x6A | 0x6F => Some(ZoomFit),                            // numpad * and /
            0x4E | 0x20 | 0x27 | 0x22 | 0x28 => Some(Next),          // N, Space, Right, PgDn, Down
            0x50 | 0x08 | 0x25 | 0x21 | 0x26 => Some(Previous),      // P, Backspace, Left, PgUp, Up
            _ => None,
        })
    }
}

const VK_ESCAPE: u16 = 0x1B;

unsafe fn register_class() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wnd_proc),
            hInstance: module(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    });
}

/// Creates the viewer for `path`, or returns `None` (so TC tries other plugins) if it isn't a
/// displayable image.
pub unsafe fn create_viewer(lister: HWND, path: &Path) -> Option<HWND> {
    let mut state = Box::new(ViewerState::new(lister, path, ViewerConfig::load())?);
    register_class();

    let mut rc = RECT::default();
    let _ = GetClientRect(lister, &mut rc);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        CLASS_NAME,
        w!("MediaresViewer"),
        WS_CHILD | WS_VISIBLE,
        0,
        0,
        rc.right - rc.left,
        rc.bottom - rc.top,
        Some(lister),
        None,
        Some(module()),
        None,
    )
    .ok()?;

    state.hwnd = hwnd;
    let start_fullscreen = state.config.start_fullscreen;
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);

    let state = get_state(hwnd)?;
    if start_fullscreen {
        fullscreen::toggle(state);
    }
    update_title(state);
    Some(hwnd)
}

pub unsafe fn close_viewer(hwnd: HWND) {
    let _ = DestroyWindow(hwnd);
}

/// `ListLoadNext`: shows `path` in the existing window. False if it can't be displayed.
pub unsafe fn load_next(lister: HWND, hwnd: HWND, path: &Path) -> bool {
    let Some(state) = get_state(hwnd) else { return false };
    state.lister = lister;
    if !state.set_file(path) {
        return false;
    }
    refresh(state);
    true
}

unsafe fn get_state<'a>(hwnd: HWND) -> Option<&'a mut ViewerState> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ViewerState).as_mut()
}

/// Like [`get_state`], but first verifies `hwnd` is still one of our viewer windows. Used after
/// modal loops, during which the viewer may have been destroyed and its handle reused.
unsafe fn get_live_state<'a>(hwnd: HWND) -> Option<&'a mut ViewerState> {
    let mut class = [0u16; 64];
    let len = GetClassNameW(hwnd, &mut class) as usize;
    let ours = HSTRING::from_wide(&class[..len]) == CLASS_NAME.to_hstring();
    if ours { get_state(hwnd) } else { None }
}

unsafe fn redraw(hwnd: HWND) {
    let _ = InvalidateRect(Some(hwnd), None, false);
    let _ = UpdateWindow(hwnd);
}

/// Repaints synchronously and updates the Lister caption.
unsafe fn refresh(state: &ViewerState) {
    redraw(state.hwnd);
    update_title(state);
}

unsafe fn update_title(state: &ViewerState) {
    if state.lister.is_invalid() {
        return;
    }
    let name = state.file_path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let mut title = name.into_owned();
    if let Some(img) = &state.image {
        let mp = img.width as f64 * img.height as f64 / 1_000_000.0;
        title += &format!(" - [{}x{}, {:.1} MP]", img.width, img.height, mp);
        if let Some(zoom) = image_view::zoom_percent(state) {
            title += &format!(" [{}%]", zoom);
        }
        if !state.dir_files.is_empty() {
            title += &format!(" [{}/{}]", state.current_idx + 1, state.dir_files.len());
        }
    }
    title += " - Mediares Lister";
    let _ = SetWindowTextW(state.lister, &HSTRING::from(title));
}

unsafe fn forward_to_lister(state: &ViewerState, wparam: WPARAM, lparam: LPARAM) {
    if !state.lister.is_invalid() {
        let _ = PostMessageW(Some(state.lister), WM_KEYDOWN, wparam, lparam);
    }
}

unsafe fn execute(hwnd: HWND, command: Command) {
    let Some(state) = get_state(hwnd) else { return };
    match command {
        Command::ToggleFullscreen => {
            state.drag = None;
            image_view::loupe_end(state);
            fullscreen::toggle(state);
            refresh(state);
        }
        Command::ToggleOsd => {
            state.config.show_osd = !state.config.show_osd;
            state.config.save();
            redraw(hwnd);
        }
        Command::ShowExif => {
            let path = state.file_path.clone();
            exif_dialog::show(dialog::modal_owner(hwnd), &path);
        }
        Command::ShowSettings => {
            let current = state.config.clone();
            let new_config = settings_dialog::show(dialog::modal_owner(hwnd), &current);
            if let (Some(config), Some(state)) = (new_config, get_live_state(hwnd)) {
                state.apply_config(config);
                refresh(state);
            }
        }
        Command::Next | Command::Previous => {
            if state.navigate(command == Command::Next) {
                refresh(state);
            }
        }
        Command::ZoomIn | Command::ZoomOut => {
            if let Some(view) = client_size(hwnd) {
                zoom_at(state, view, command == Command::ZoomIn, (view.0 / 2.0, view.1 / 2.0));
            }
        }
        Command::ZoomActualSize => {
            if let (Some(img), Some(view)) = (state.image.clone(), client_size(hwnd)) {
                image_view::zoom_actual_size(state, &img, view);
                refresh(state);
            }
        }
        Command::ZoomFit => {
            state.loupe = None;
            state.drag = None;
            state.zoom = ZoomMode::Fit;
            refresh(state);
        }
    }
}

unsafe fn zoom_at(state: &mut ViewerState, view: (f32, f32), zoom_in: bool, anchor: (f32, f32)) {
    if let Some(img) = state.image.clone() {
        image_view::zoom_step(state, &img, view, zoom_in, anchor);
        refresh(state);
    }
}

unsafe fn show_context_menu(hwnd: HWND, screen: POINT) {
    let Some(state) = get_state(hwnd) else { return };
    let checked = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    let (fullscreen, osd) = (state.fullscreen, state.config.show_osd);

    let Ok(menu) = CreatePopupMenu() else { return };
    for item in Command::MENU {
        match item {
            Some((cmd, label)) => {
                let flags = match cmd {
                    Command::ToggleFullscreen => checked(fullscreen),
                    Command::ToggleOsd => checked(osd),
                    _ => MENU_ITEM_FLAGS(0),
                };
                let _ = AppendMenuW(menu, MF_STRING | flags, *cmd as usize, &HSTRING::from(*label));
            }
            None => {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            }
        }
    }
    let id = TrackPopupMenu(menu, TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD, screen.x, screen.y, Some(0), hwnd, None);
    let _ = DestroyMenu(menu);

    if let Some(cmd) = Command::from_id(id.0) {
        execute(hwnd, cmd);
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    mediares_core::ffi::guard(LRESULT(0), || unsafe { handle_message(hwnd, msg, wparam, lparam) })
}

unsafe fn handle_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if !hdc.is_invalid() {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                image_view::paint(hdc, get_state(hwnd), rc.right - rc.left, rc.bottom - rc.top);
                let _ = EndPaint(hwnd, &ps);
            }
            return LRESULT(0);
        }
        WM_NCDESTROY => {
            let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut ViewerState;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        WM_DESTROY => return LRESULT(0),
        _ => {}
    }

    let Some(state) = get_state(hwnd) else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };

    match msg {
        WM_SIZE => {
            if let (Some(img), Some(view)) = (state.image.clone(), client_size(hwnd)) {
                image_view::clamp_offset(state, &img, view);
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            update_title(state);
        }
        WM_RBUTTONUP => {
            let mut pt = point_from_lparam(lparam.0);
            let _ = ClientToScreen(hwnd, &mut pt);
            show_context_menu(hwnd, pt);
        }
        WM_LBUTTONDOWN => {
            let _ = SetFocus(Some(hwnd));
            let pt = point_from_lparam(lparam.0);
            let (Some(img), Some(view)) = (state.image.clone(), client_size(hwnd)) else {
                return LRESULT(0);
            };
            if state.zoom == ZoomMode::Fit {
                image_view::loupe_begin(state, &img, view, (pt.x, pt.y));
                refresh(state);
            } else {
                state.drag = Some(Drag { start: (pt.x, pt.y), start_offset: state.offset });
            }
            let _ = SetCapture(hwnd);
        }
        WM_MOUSEMOVE => {
            let pt = point_from_lparam(lparam.0);
            let (Some(img), Some(view)) = (state.image.clone(), client_size(hwnd)) else {
                return LRESULT(0);
            };
            if let Some(drag) = state.drag {
                state.offset = (
                    drag.start_offset.0 + (pt.x - drag.start.0) as f32,
                    drag.start_offset.1 + (pt.y - drag.start.1) as f32,
                );
                image_view::clamp_offset(state, &img, view);
                redraw(hwnd);
            } else if state.loupe.is_some() {
                image_view::loupe_follow(state, &img, view, (pt.x, pt.y));
                redraw(hwnd);
            }
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            state.drag = None;
            if image_view::loupe_end(state) {
                refresh(state);
            }
        }
        WM_LBUTTONDBLCLK => {
            let _ = ReleaseCapture();
            execute(hwnd, Command::ToggleFullscreen);
        }
        WM_SETCURSOR => {
            let cursor = if state.drag.is_some() {
                IDC_SIZEALL
            } else if state.zoom != ZoomMode::Fit {
                IDC_HAND
            } else {
                IDC_ARROW
            };
            if let Ok(c) = LoadCursorW(None, cursor) {
                SetCursor(Some(c));
                return LRESULT(1);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;
            if ctrl_down() {
                let mut pt = point_from_lparam(lparam.0);
                let _ = ScreenToClient(hwnd, &mut pt);
                if let Some(view) = client_size(hwnd) {
                    zoom_at(state, view, delta > 0, (pt.x as f32, pt.y as f32));
                }
            } else if delta != 0 {
                execute(hwnd, if delta < 0 { Command::Next } else { Command::Previous });
            }
        }
        WM_XBUTTONDOWN => {
            match (wparam.0 >> 16) & 0xFFFF {
                1 => execute(hwnd, Command::Previous),
                2 => execute(hwnd, Command::Next),
                _ => {}
            }
            return LRESULT(1);
        }
        WM_KEYDOWN => {
            let vk = wparam.0 as u16;
            if vk == VK_ESCAPE && state.fullscreen {
                execute(hwnd, Command::ToggleFullscreen);
            } else if let Some(cmd) = Command::from_key(vk, ctrl_down()) {
                execute(hwnd, cmd);
            } else {
                forward_to_lister(state, wparam, lparam);
            }
        }
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    LRESULT(0)
}

unsafe fn ctrl_down() -> bool {
    GetKeyState(VK_CONTROL.0 as i32) < 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_combinations_go_to_lister() {
        assert_eq!(Command::from_key(0x50, false), Some(Command::Previous)); // P
        assert_eq!(Command::from_key(0x50, true), None); // Ctrl+P: print in Lister
        assert_eq!(Command::from_key(0x43, true), None); // Ctrl+C
        assert_eq!(Command::from_key(0xBB, true), Some(Command::ZoomIn));
    }

    #[test]
    fn menu_ids_round_trip() {
        for (cmd, _) in Command::MENU.iter().flatten() {
            assert_eq!(Command::from_id(*cmd as i32), Some(*cmd));
        }
        assert_eq!(Command::from_id(0), None);
    }
}
