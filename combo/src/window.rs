//! Lister viewer window: creation, message handling and user commands.
//!
//! State lives in a `Box<ViewerState>` behind `GWLP_USERDATA`. Handlers fetch it per message and
//! never hold it across calls that pump messages (menus, dialogs), re-fetching afterwards.

use std::path::Path;
use std::sync::Once;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, EndPaint, InvalidateRect, ScreenToClient, UpdateWindow, HBRUSH,
    PAINTSTRUCT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetClassNameW, GetClientRect, GetWindowLongPtrW, KillTimer, LoadCursorW, PostMessageW,
    SetCursor, SetTimer, SetWindowLongPtrW, SetWindowTextW, ShowWindow, TrackPopupMenu, CS_DBLCLKS,
    GWLP_USERDATA, GWL_STYLE, IDC_ARROW, IDC_HAND, IDC_SIZEALL, MENU_ITEM_FLAGS, MF_CHECKED,
    MF_SEPARATOR, MF_STRING, MF_UNCHECKED, SW_PARENTCLOSING, SW_SHOW, TPM_LEFTALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WINDOWPOS, WINDOW_EX_STYLE, WM_APP, WM_APPCOMMAND, WM_DESTROY, WM_ERASEBKGND,
    WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCDESTROY, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_SHOWWINDOW, WM_SIZE, WM_TIMER,
    WM_WINDOWPOSCHANGING, WM_XBUTTONDOWN, WS_CHILD, WS_CLIPCHILDREN,
};

use mediares_core::tc_api::{LCP_FITTOWINDOW, LC_COPY, LC_NEWPARAMS};

use crate::config::ViewerConfig;
use crate::image_cache::WM_IMAGE_READY;
use crate::image_view::{self, client_size, point_from_lparam};
use crate::media_view::EventEffect;
use crate::overlay::{self, PanelKind, PhotoButton, OVERLAY_TIMER_ID};
use crate::playback_video::WM_MEDIA_EVENT;
use crate::playlist::Repeat;
use crate::state::{Drag, ViewerState, ZoomMode};
use crate::transport_bar::Click;
use crate::{
    config, dialog, exif_dialog, file_actions, fullscreen, gdi, module, osd_template,
    settings_dialog, snapshot,
};
use mediares_core::exif::read_orientation;

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
    TogglePlay,
    ToggleMute,
    SeekBack,
    SeekForward,
    KeyframeBack,
    KeyframeForward,
    VolumeUp,
    VolumeDown,
    PreviousTrack,
    NextTrack,
    ToggleAutoAdvance,
    RepeatOff,
    RepeatAll,
    RepeatOne,
    ToggleShuffle,
    Copy,
    SaveFrame,
    ToggleSlideshow,
    RotateLeft,
    RotateRight,
    Delete,
    OpenInEditor,
    ShowInFolder,
    SetWallpaper,
    Print,
    Slower,
    Faster,
    NormalSpeed,
    FrameBack,
    FrameForward,
}

impl Command {
    const MENU: &[Option<(Command, &'static str)>] = &[
        Some((Command::TogglePlay, "Воспроизведение / пауза\tПробел")),
        Some((Command::ToggleMute, "Без звука\tM")),
        Some((Command::ToggleFullscreen, "Полноэкранный режим\tEnter / F")),
        Some((Command::Copy, "Копировать изображение\tCtrl+C")),
        Some((Command::SaveFrame, "Сохранить кадр рядом с видео\tShift+S")),
        Some((Command::ToggleOsd, "Отображать OSD\tO")),
        Some((Command::ToggleSlideshow, "Слайд-шоу\tF5")),
        Some((Command::RotateLeft, "Повернуть влево\tL")),
        Some((Command::RotateRight, "Повернуть вправо\tR")),
        None,
        Some((Command::Slower, "Медленнее\t[")),
        Some((Command::Faster, "Быстрее\t]")),
        Some((Command::NormalSpeed, "Обычная скорость\t\\")),
        Some((Command::FrameBack, "Кадр назад\t,")),
        Some((Command::FrameForward, "Кадр вперёд\t.")),
        None,
        Some((Command::ToggleAutoAdvance, "Автопереход к следующему файлу")),
        Some((Command::RepeatOff, "Без повтора")),
        Some((Command::RepeatAll, "Повторять список")),
        Some((Command::RepeatOne, "Повторять файл")),
        Some((Command::ToggleShuffle, "Случайный порядок")),
        None,
        Some((Command::ShowExif, "Просмотр EXIF...\tE")),
        Some((Command::OpenInEditor, "Открыть в редакторе")),
        Some((Command::ShowInFolder, "Показать в папке")),
        Some((Command::SetWallpaper, "Сделать обоями рабочего стола")),
        Some((Command::Print, "Печать...\tCtrl+P")),
        Some((Command::Delete, "Удалить в корзину\tDel")),
        None,
        Some((Command::ShowSettings, "Настройки...\tS")),
        None,
        Some((Command::Next, "Следующий файл\tПробел / Right")),
        Some((Command::Previous, "Предыдущий файл\tBackspace / Left")),
    ];

    /// Whether the item applies to the current content (photo, or audio/video).
    fn available(self, media: bool, video: bool) -> bool {
        use Command::*;
        match self {
            SaveFrame | Slower | Faster | NormalSpeed | FrameBack | FrameForward => video,
            TogglePlay | ToggleMute | ToggleAutoAdvance | RepeatOff | RepeatAll | RepeatOne
            | ToggleShuffle => media,
            ShowExif | ToggleSlideshow | RotateLeft | RotateRight | SetWallpaper => !media,
            ToggleOsd | Print => !media || video,
            _ => true,
        }
    }

    fn repeat_mode(self) -> Option<Repeat> {
        match self {
            Command::RepeatOff => Some(Repeat::Off),
            Command::RepeatAll => Some(Repeat::All),
            Command::RepeatOne => Some(Repeat::One),
            _ => None,
        }
    }

    /// Any command by its numeric value (posted messages, menu ids).
    fn from_raw(value: usize) -> Option<Command> {
        use Command::*;
        const ALL: [Command; 40] = [
            ToggleFullscreen,
            ToggleOsd,
            ShowExif,
            ShowSettings,
            Next,
            Previous,
            ZoomIn,
            ZoomOut,
            ZoomActualSize,
            ZoomFit,
            TogglePlay,
            ToggleMute,
            SeekBack,
            SeekForward,
            KeyframeBack,
            KeyframeForward,
            VolumeUp,
            VolumeDown,
            PreviousTrack,
            NextTrack,
            ToggleAutoAdvance,
            RepeatOff,
            RepeatAll,
            RepeatOne,
            ToggleShuffle,
            Copy,
            SaveFrame,
            ToggleSlideshow,
            RotateLeft,
            RotateRight,
            Delete,
            OpenInEditor,
            ShowInFolder,
            SetWallpaper,
            Print,
            Slower,
            Faster,
            NormalSpeed,
            FrameBack,
            FrameForward,
        ];
        // Values start at 1 and follow the declaration order.
        ALL.get(value.checked_sub(1)?).copied()
    }

    /// Previous / next file in the play queue.
    fn track(forward: bool) -> Command {
        if forward {
            Command::NextTrack
        } else {
            Command::PreviousTrack
        }
    }

    /// Hotkeys. With Ctrl held only zoom keys are ours; everything else goes to the Lister
    /// (Ctrl+P print, Ctrl+C copy, ...). For audio/video, player keys take precedence.
    fn from_key(vk: u16, ctrl: bool, shift: bool, media: bool) -> Option<Command> {
        use Command::*;
        if ctrl && !shift && vk == 0x43 {
            return Some(Copy); // Ctrl+C: the picture, not the file (TC copies files in its panels)
        }
        if media && shift && !ctrl && vk == 0x53 {
            return Some(SaveFrame); // Shift+S, as in VLC
        }
        if media {
            let keyboard_media = match vk {
                0xB0 => Some(NextTrack),     // VK_MEDIA_NEXT_TRACK
                0xB1 => Some(PreviousTrack), // VK_MEDIA_PREV_TRACK
                0xB3 => Some(TogglePlay),    // VK_MEDIA_PLAY_PAUSE
                _ => None,
            };
            if keyboard_media.is_some() {
                return keyboard_media;
            }
        }
        if media && !ctrl {
            let player = match vk {
                0x20 | 0x4B => Some(TogglePlay), // Space, K
                0x25 => Some(SeekBack),          // Left: -5 s
                0x27 => Some(SeekForward),       // Right: +5 s
                0x26 => Some(KeyframeForward),   // Up: next key frame (audio: +1 s)
                0x28 => Some(KeyframeBack),      // Down: previous key frame (audio: -1 s)
                0xBB | 0x6B => Some(VolumeUp),   // '+' / numpad +
                0xBD | 0x6D => Some(VolumeDown), // '-' / numpad -
                0x4D => Some(ToggleMute),        // M
                0xDB => Some(Slower),            // [
                0xDD => Some(Faster),            // ]
                0xDC => Some(NormalSpeed),       // \
                0xBC => Some(FrameBack),         // ,
                0xBE => Some(FrameForward),      // .
                _ => None,
            };
            if player.is_some() {
                return player;
            }
        }
        let zoom = match vk {
            0xBB | 0x6B => Some(ZoomIn),  // '+' / numpad +
            0xBD | 0x6D => Some(ZoomOut), // '-' / numpad -
            0x30 | 0x60 => Some(ZoomFit), // '0' / numpad 0
            _ => None,
        };
        if ctrl {
            return zoom;
        }
        zoom.or(match vk {
            0x0D | 0x46 | 0x7A => Some(ToggleFullscreen), // Enter, F, F11
            0x4F | 0x49 => Some(ToggleOsd),               // O, I
            0x74 => Some(ToggleSlideshow),                // F5
            0x45 => Some(ShowExif),                       // E
            0x53 => Some(ShowSettings),                   // S
            0x31 | 0x61 => Some(ZoomActualSize),          // '1' / numpad 1
            0x4C => Some(RotateLeft),                     // L
            0x52 => Some(RotateRight),                    // R
            0x2E => Some(Delete),                         // Del
            0x6A | 0x6F => Some(ZoomFit),                 // numpad * and /
            0x4E | 0x20 | 0x27 | 0x22 | 0x28 => Some(Next), // N, Space, Right, PgDn, Down
            0x50 | 0x08 | 0x25 | 0x21 | 0x26 => Some(Previous), // P, Backspace, Left, PgUp, Up
            _ => None,
        })
    }
}

const VK_ESCAPE: u16 = 0x1B;

unsafe fn register_class() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        gdi::register_class(CLASS_NAME, Some(wnd_proc), CS_DBLCLKS, HBRUSH::default());
    });
}

/// Creates the viewer for `path`, or returns `None` (so TC tries other plugins) if it can't be
/// displayed.
pub unsafe fn create_viewer(lister: HWND, path: &Path, show_flags: i32) -> Option<HWND> {
    register_class();

    let mut rc = RECT::default();
    let _ = GetClientRect(lister, &mut rc);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        CLASS_NAME,
        w!("MediaresViewer"),
        // Hidden until the content is loaded; clip children so painting never covers the video.
        WS_CHILD | WS_CLIPCHILDREN,
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

    // The window exists before loading: a video needs it to host its rendering surface.
    let Some(mut state) = ViewerState::new(hwnd, lister, path, ViewerConfig::load()).map(Box::new)
    else {
        let _ = DestroyWindow(hwnd);
        return None;
    };
    state.show_flags = show_flags;
    // Only the standalone Lister (F3) starts fullscreen, not the Quick View panel (Ctrl+Q).
    let start_fullscreen = state.config.start_fullscreen && !is_quick_view(lister);
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
    let _ = ShowWindow(hwnd, SW_SHOW);

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
pub unsafe fn load_next(lister: HWND, hwnd: HWND, path: &Path, show_flags: i32) -> bool {
    let Some(state) = get_state(hwnd) else {
        return false;
    };
    state.lister = lister;
    state.show_flags = show_flags;
    if !state.set_file(path) {
        return false;
    }
    refresh(state);
    true
}

/// `ListSendCommand`. A toggled `LCP_FITTOWINDOW` means the user pressed TC's `F` hotkey,
/// which we map to fullscreen (the viewer always fits the window anyway).
pub unsafe fn send_command(hwnd: HWND, command: i32, parameter: i32) -> bool {
    let Some(state) = get_state(hwnd) else {
        return false;
    };
    if command != LC_NEWPARAMS && command != LC_COPY {
        return false;
    }
    if command == LC_COPY {
        execute(hwnd, Command::Copy);
        return true;
    }
    let fit_toggled = (state.show_flags ^ parameter) & LCP_FITTOWINDOW != 0;
    state.show_flags = parameter;
    if fit_toggled {
        execute(hwnd, Command::ToggleFullscreen);
    }
    true
}

/// TC's Quick View panel (Ctrl+Q) hosts the plugin in a child window of the main window,
/// while the Lister (F3) is a top-level window.
unsafe fn is_quick_view(parent: HWND) -> bool {
    GetWindowLongPtrW(parent, GWL_STYLE) as u32 & WS_CHILD.0 != 0
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
    if ours {
        get_state(hwnd)
    } else {
        None
    }
}

unsafe fn redraw(hwnd: HWND) {
    let _ = InvalidateRect(Some(hwnd), None, false);
    let _ = UpdateWindow(hwnd);
}

/// Repaints synchronously and updates the Lister caption (and the fullscreen panel).
unsafe fn refresh(state: &mut ViewerState) {
    fullscreen::sync_overlay(state);
    sync_video_osd(state);
    redraw(state.hwnd);
    update_title(state);
}

unsafe fn update_title(state: &ViewerState) {
    if state.lister.is_invalid() {
        return;
    }
    let title = caption(state) + " - Mediares Lister";
    let _ = SetWindowTextW(state.lister, &HSTRING::from(title));
}

/// Video OSD per the config's template.
unsafe fn sync_video_osd(state: &mut ViewerState) {
    let Some(video) = state.media.as_mut().and_then(|m| m.video_mut()) else {
        return;
    };
    let config = &state.config;
    let style = config.osd.video().then(|| crate::video_view::OsdStyle {
        face: config.osd_font_name.clone(),
        size_pt: config.osd_font_size,
        color: config.osd_font_color,
    });
    if style.is_none() {
        video.set_osd(
            None,
            crate::video_view::OsdText {
                template: String::new(),
                fields: Default::default(),
            },
        );
        return;
    }
    let template = config.video_osd.clone();
    let position =
        (!state.dir_files.is_empty()).then_some((state.current_idx, state.dir_files.len()));
    let (width, height, fps) = (video.info.width, video.info.height, video.info.frame_rate);
    let tags = osd_template::uses_any(&template, osd_template::VIDEO_TAG_KEYS)
        .then(|| video.tags())
        .flatten()
        .unwrap_or_default();
    let meta = Some(&video.info);
    let mut fields = std::collections::HashMap::new();
    for (_, group) in osd_template::VIDEO_FIELDS {
        for field in *group {
            let value =
                osd_template::file_field(&state.file_path, state.file_size, position, field.key)
                    .or_else(|| osd_template::video_field(meta, &tags, field.key))
                    .or_else(|| match field.key {
                        "width" => Some(width.to_string()),
                        "height" => Some(height.to_string()),
                        "fps" => Some(osd_template::format_fps(fps)),
                        _ => None,
                    });
            if let Some(value) = value {
                fields.insert(field.key, value);
            }
        }
    }
    video.set_osd(style, crate::video_view::OsdText { template, fields });
}

/// "name - [details] [3/12]" for the Lister caption.
unsafe fn caption(state: &ViewerState) -> String {
    let name = state
        .file_path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let mut title = name.into_owned();
    if let Some(img) = &state.image {
        let mp = img.width as f64 * img.height as f64 / 1_000_000.0;
        title += &format!(" - [{}x{}, {:.1} MP]", img.width, img.height, mp);
        if let Some(zoom) = image_view::zoom_percent(state) {
            title += &format!(" [{}%]", zoom);
        }
    } else if let Some(media) = &state.media {
        if let Some(tagged) = media.display_title() {
            title = tagged;
        }
        title += &format!(" - [{}]", media.title_info());
    }
    if state.has_content() && !state.dir_files.is_empty() {
        let list = state
            .playlist
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| format!(" · {}", n.to_string_lossy()));
        title += &format!(
            " [{}/{}{}]",
            state.current_idx + 1,
            state.dir_files.len(),
            list.unwrap_or_default()
        );
    }
    if state.slideshow {
        title += " [слайд-шоу]";
    }
    title
}

/// Posted by the fullscreen panel's buttons, so that the command (which may replace the panel) runs
/// outside the panel's own window procedure. `wparam` = command id.
const WM_OVERLAY_COMMAND: u32 = WM_APP + 0x11;

/// Messages of the fullscreen panel window (see `overlay.rs`); `None` = default handling.
pub unsafe fn overlay_message(
    viewer: HWND,
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    let state = get_live_state(viewer)?;
    let fs = state.overlay.as_mut()?;
    let kind = fs.panel.as_ref().filter(|p| p.hwnd == window)?.kind;
    let pt = point_from_lparam(lparam.0);
    if matches!(msg, WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP) {
        fs.panel_used(viewer);
    }
    match (msg, kind) {
        (WM_PAINT, PanelKind::Video) => state.media.as_mut()?.paint_bar_host(window),
        (WM_PAINT, PanelKind::Photo) => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(window, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(window, &mut rc);
            let scale = gdi::dpi_scale(window);
            let playing = state.slideshow;
            gdi::with_buffer(hdc, rc.right, rc.bottom, rc, |dc| unsafe {
                overlay::paint_photo_panel(dc, rc, scale, playing)
            });
            let _ = EndPaint(window, &ps);
        }
        (WM_MOUSEMOVE, PanelKind::Video) => state.media.as_mut()?.mouse_move(pt.x),
        (WM_LBUTTONDOWN, PanelKind::Video) => {
            let media = state.media.as_mut()?;
            match media.bar_mouse_down(pt.x, pt.y) {
                Click::Bar if media.is_dragging() => {
                    let _ = SetCapture(window);
                }
                Click::Skip(forward) => {
                    let cmd = Command::track(forward);
                    let _ = PostMessageW(
                        Some(viewer),
                        WM_OVERLAY_COMMAND,
                        WPARAM(cmd as usize),
                        LPARAM(0),
                    );
                }
                _ => {}
            }
        }
        (WM_LBUTTONDOWN, PanelKind::Photo) => {
            let command = overlay::hit_photo(gdi::dpi_scale(window), pt.x, pt.y).map(|b| match b {
                PhotoButton::Previous => Command::Previous,
                PhotoButton::Slideshow => Command::ToggleSlideshow,
                PhotoButton::Next => Command::Next,
            });
            if let Some(cmd) = command {
                let _ = PostMessageW(
                    Some(viewer),
                    WM_OVERLAY_COMMAND,
                    WPARAM(cmd as usize),
                    LPARAM(0),
                );
            }
        }
        (WM_LBUTTONUP, _) => {
            let _ = ReleaseCapture();
            if kind == PanelKind::Video {
                let media = state.media.as_mut()?;
                if media.is_dragging() {
                    media.mouse_up(pt.x);
                }
            }
        }
        (WM_MOUSEMOVE, _) => {}
        // Wheel over the floating bar: volume (over the picture it pages through the files).
        (WM_MOUSEWHEEL, PanelKind::Video) => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;
            if delta != 0 {
                let media = state.media.as_mut()?;
                media.change_volume(delta > 0);
                media.invalidate_bar();
            }
        }
        _ => return None,
    }
    Some(LRESULT(0))
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
            // Switches the OSD for the kind of content shown (audio has none).
            let osd = state.config.osd;
            let video = state.media.as_ref().is_some_and(|m| m.is_video());
            state.config.osd = match (video, state.shows_photo()) {
                (true, _) => osd.with(osd.photo(), !osd.video()),
                (false, true) => osd.with(!osd.photo(), osd.video()),
                (false, false) => return,
            };
            state.config.save();
            refresh(state);
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
        Command::Copy => {
            // The picture for editors, plus a file for pasting into a folder: the photo itself,
            // or the frame / album art saved as a temporary PNG.
            let (picture, file) = match &state.media {
                Some(media) => {
                    let picture = media.current_picture();
                    let file = picture.as_ref().and_then(|img| {
                        snapshot::save_temp_png(
                            img,
                            &state.file_path,
                            media.is_video().then(|| media.position()),
                        )
                    });
                    (picture, file)
                }
                None => (state.image.clone(), Some(state.file_path.clone())),
            };
            let copied =
                picture.is_some_and(|img| snapshot::copy_to_clipboard(hwnd, &img, file.as_deref()));
            if let Some(media) = state.media.as_mut() {
                let text = if copied {
                    "Изображение скопировано в буфер обмена"
                } else {
                    "Нечего копировать"
                };
                media.show_status(text.to_string());
            }
        }
        Command::SaveFrame => {
            let path = state.file_path.clone();
            let Some(media) = state.media.as_mut().filter(|m| m.is_video()) else {
                return;
            };
            let saved = media.current_picture().and_then(|frame| {
                let target = snapshot::frame_file_name(&path, media.position());
                snapshot::save_png(&frame, &target).then_some(target)
            });
            let text = match saved {
                Some(target) => format!(
                    "Кадр сохранён: {}",
                    target.file_name().unwrap_or_default().to_string_lossy()
                ),
                None => "Не удалось сохранить кадр".to_string(),
            };
            media.show_status(text);
        }
        Command::ToggleSlideshow => {
            let run = !state.slideshow && state.shows_photo();
            set_slideshow(state, run);
            update_title(state);
        }
        Command::RotateLeft | Command::RotateRight => {
            if state.rotate(command == Command::RotateRight) {
                refresh(state);
            }
        }
        Command::Delete => delete_current(hwnd),
        Command::OpenInEditor => {
            leave_fullscreen(hwnd);
            let Some(state) = get_state(hwnd) else { return };
            let editor = match &state.media {
                None => state.config.photo_editor.clone(),
                Some(media) if media.is_video() => state.config.video_editor.clone(),
                Some(_) => state.config.audio_editor.clone(),
            };
            let path = state.file_path.clone();
            if !file_actions::open_in_editor(hwnd, &path, &editor) {
                let text = if editor.is_empty() {
                    "Не удалось открыть файл во внешней программе.".to_string()
                } else {
                    format!(
                        "Не удалось запустить редактор:\n{}\n\nПуть задаётся в настройках (S).",
                        editor
                    )
                };
                file_actions::show_error(dialog::modal_owner(hwnd), &text);
            }
        }
        Command::ShowInFolder => {
            let path = state.file_path.clone();
            leave_fullscreen(hwnd);
            file_actions::show_in_folder(hwnd, &path);
        }
        Command::SetWallpaper => set_wallpaper(hwnd),
        Command::Print => {
            print(hwnd, None);
        }
        Command::Slower | Command::Faster | Command::NormalSpeed => {
            if let Some(media) = state.media.as_mut() {
                media.change_rate(match command {
                    Command::Slower => Some(false),
                    Command::Faster => Some(true),
                    _ => None,
                });
            }
        }
        Command::FrameBack | Command::FrameForward => {
            if let Some(media) = state.media.as_mut() {
                media.frame_step(command == Command::FrameForward);
                media.invalidate_bar();
            }
        }
        Command::NextTrack | Command::PreviousTrack => {
            if state.skip_track(command == Command::NextTrack) {
                refresh(state);
            }
        }
        Command::ToggleAutoAdvance
        | Command::RepeatOff
        | Command::RepeatAll
        | Command::RepeatOne
        | Command::ToggleShuffle => {
            let queue = &mut state.config.queue;
            match command {
                Command::ToggleAutoAdvance => queue.auto_advance = !queue.auto_advance,
                Command::ToggleShuffle => queue.shuffle = !queue.shuffle,
                _ => queue.repeat = command.repeat_mode().unwrap_or_default(),
            }
            state.config.save();
        }
        Command::ZoomIn | Command::ZoomOut => {
            if let Some(view) = client_size(hwnd) {
                zoom_at(
                    state,
                    view,
                    command == Command::ZoomIn,
                    (view.0 / 2.0, view.1 / 2.0),
                );
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
        Command::TogglePlay
        | Command::ToggleMute
        | Command::SeekBack
        | Command::SeekForward
        | Command::KeyframeBack
        | Command::KeyframeForward
        | Command::VolumeUp
        | Command::VolumeDown => {
            let Some(media) = state.media.as_mut() else {
                return;
            };
            match command {
                Command::TogglePlay => media.toggle_play(),
                Command::ToggleMute => media.toggle_mute(),
                Command::SeekBack | Command::SeekForward => {
                    media.seek_by(command == Command::SeekForward)
                }
                Command::KeyframeBack | Command::KeyframeForward => {
                    media.seek_keyframe(command == Command::KeyframeForward)
                }
                _ => media.change_volume(command == Command::VolumeUp),
            }
            media.invalidate_bar();
        }
    }
}

/// Another program is about to open: fullscreen (topmost) would keep it hidden.
unsafe fn leave_fullscreen(hwnd: HWND) {
    if get_state(hwnd).is_some_and(|s| s.fullscreen.is_some()) {
        execute(hwnd, Command::ToggleFullscreen);
    }
}

/// Del: the file goes to the Recycle Bin and the next one is shown (the Lister closes when none
/// is left). State is re-fetched after each call that may show a dialog.
unsafe fn delete_current(hwnd: HWND) {
    let Some(state) = get_state(hwnd) else { return };
    let (path, confirm) = (state.file_path.clone(), state.config.confirm_delete);
    if !path.is_file() {
        return;
    }
    let owner = dialog::modal_owner(hwnd);
    if confirm && !file_actions::confirm_delete(owner, &path) {
        return;
    }
    let Some(state) = get_live_state(hwnd) else {
        return;
    };
    // A player keeps its file open: close it first.
    let had_player = state.media.take().is_some();
    let deleted = file_actions::recycle(owner, &path);
    let Some(state) = get_live_state(hwnd) else {
        return;
    };
    if !deleted {
        if had_player {
            state.reload();
            refresh(state);
        }
        return;
    }
    if state.remove_current() {
        refresh(state);
    } else if !state.lister.is_invalid() {
        let _ = PostMessageW(
            Some(state.lister),
            WM_KEYDOWN,
            WPARAM(VK_ESCAPE as usize),
            LPARAM(0),
        );
    }
}

/// The photo as the desktop wallpaper: the file itself when Windows can show it as is, otherwise
/// (RAW, PSD, turned with R / L, EXIF-rotated) the picture on screen saved as PNG.
unsafe fn set_wallpaper(hwnd: HWND) {
    let Some(state) = get_state(hwnd) else { return };
    let Some(img) = state.image.clone() else {
        return;
    };
    let path = state.file_path.clone();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let as_is = !state.turned
        && matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "bmp")
        && read_orientation(&path).is_none_or(|o| o == 1);
    let source = if as_is {
        Some(path)
    } else {
        let target = config::data_file("mediares_wallpaper.png");
        snapshot::save_png(&img, &target).then_some(target)
    };
    if !source.is_some_and(|s| file_actions::set_wallpaper(&s)) {
        file_actions::show_error(dialog::modal_owner(hwnd), "Не удалось установить обои.");
    }
}

/// Prints the photo, or the video frame / album art. `margins` in 1/100 mm.
pub unsafe fn print(hwnd: HWND, margins: Option<RECT>) -> bool {
    let Some(state) = get_state(hwnd) else {
        return false;
    };
    let picture = match &state.media {
        Some(media) => media.current_picture(),
        None => state.image.clone(),
    };
    let Some(picture) = picture else { return false };
    let name = state
        .file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    crate::print::print(dialog::modal_owner(hwnd), &picture, &name, margins)
}

/// Viewer timer of the slideshow.
const SLIDESHOW_TIMER_ID: usize = 0x4D56;

unsafe fn set_slideshow(state: &mut ViewerState, run: bool) {
    state.slideshow = run;
    if run {
        SetTimer(
            Some(state.hwnd),
            SLIDESHOW_TIMER_ID,
            state.config.slideshow_seconds.max(1) * 1000,
            None,
        );
    } else {
        let _ = KillTimer(Some(state.hwnd), SLIDESHOW_TIMER_ID);
    }
    if let Some(fs) = &state.overlay {
        fs.invalidate_panel();
    }
}

/// Reacts to an engine event or a player timer tick.
unsafe fn apply_effect(hwnd: HWND, effect: EventEffect) {
    let Some(state) = get_state(hwnd) else { return };
    match effect {
        EventEffect::Relayout => {
            if let Some(media) = &state.media {
                media.layout();
            }
            refresh(state);
        }
        EventEffect::RepaintBar => {
            if let Some(media) = &state.media {
                media.invalidate_bar();
            }
        }
        EventEffect::Ended => {
            if state.playback_ended() {
                refresh(state);
            } else if let Some(media) = &state.media {
                media.invalidate_bar();
            }
        }
        EventEffect::None => {}
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
    let (fullscreen, media, queue) = (
        state.fullscreen.is_some(),
        state.media.is_some(),
        state.config.queue,
    );
    let video = state.media.as_ref().is_some_and(|m| m.is_video());
    let osd = if video {
        state.config.osd.video()
    } else {
        state.config.osd.photo()
    };

    let Ok(menu) = CreatePopupMenu() else { return };
    // Fullscreen hides the idle cursor: the menu needs it.
    if let Some(fs) = state.overlay.as_mut() {
        fs.reveal_cursor(hwnd);
    }
    let items = Command::MENU
        .iter()
        .filter(|item| item.is_none_or(|(cmd, _)| cmd.available(media, video)));
    let mut previous_was_separator = true;
    for item in items {
        // Skip separators that would end up leading or doubled after filtering.
        if item.is_none() && previous_was_separator {
            continue;
        }
        previous_was_separator = item.is_none();
        match item {
            Some((cmd, label)) => {
                let flags = match cmd {
                    Command::ToggleFullscreen => checked(fullscreen),
                    Command::ToggleOsd => checked(osd),
                    Command::ToggleAutoAdvance => checked(queue.auto_advance),
                    Command::ToggleShuffle => checked(queue.shuffle),
                    cmd => cmd
                        .repeat_mode()
                        .map_or(MENU_ITEM_FLAGS(0), |r| checked(r == queue.repeat)),
                };
                let _ = AppendMenuW(
                    menu,
                    MF_STRING | flags,
                    *cmd as usize,
                    &HSTRING::from(*label),
                );
            }
            None => {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            }
        }
    }
    let id = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
        screen.x,
        screen.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);

    if let Some(cmd) = usize::try_from(id.0).ok().and_then(Command::from_raw) {
        execute(hwnd, cmd);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    mediares_core::ffi::guard(LRESULT(0), || unsafe {
        handle_message(hwnd, msg, wparam, lparam)
    })
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
                let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
                match get_state(hwnd) {
                    Some(ViewerState {
                        media: Some(media), ..
                    }) => media.paint(hdc, w, h, ps.rcPaint),
                    state => image_view::paint(hdc, state, w, h, ps.rcPaint),
                }
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
        WM_DESTROY => {
            // Shut the engine down while its surface window still exists.
            if let Some(state) = get_state(hwnd) {
                state.media = None;
            }
            return LRESULT(0);
        }
        _ => {}
    }

    let Some(state) = get_state(hwnd) else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };

    match msg {
        WM_WINDOWPOSCHANGING => {
            if let Some(pos) = (lparam.0 as *mut WINDOWPOS).as_mut() {
                fullscreen::pin(state, pos);
            }
        }
        WM_MEDIA_EVENT => {
            if let Some(media) = state.media.as_mut() {
                let effect = media.on_event(wparam.0 as i32, lparam.0);
                apply_effect(hwnd, effect);
            }
        }
        WM_OVERLAY_COMMAND => {
            if let Some(cmd) = Command::from_raw(wparam.0) {
                execute(hwnd, cmd);
            }
        }
        WM_IMAGE_READY => {
            if state.image_ready() {
                if state.slideshow {
                    set_slideshow(state, true);
                }
                refresh(state);
            }
        }
        // A photo still decoding gets its full slideshow interval once it is shown.
        WM_TIMER if wparam.0 == SLIDESHOW_TIMER_ID && state.pending.is_some() => {}
        WM_TIMER if wparam.0 == SLIDESHOW_TIMER_ID => {
            // Stops by itself when a video / audio file was opened or there's no other photo.
            if state.shows_photo() && state.next_photo() && state.shows_photo() {
                refresh(state);
            } else {
                set_slideshow(state, false);
                update_title(state);
            }
        }
        WM_TIMER if wparam.0 == OVERLAY_TIMER_ID => {
            let busy = state.media.as_ref().is_some_and(|m| m.is_dragging());
            if let Some(fs) = state.overlay.as_mut() {
                fs.idle(hwnd, busy);
            }
        }
        WM_TIMER => {
            match state
                .media
                .as_mut()
                .and_then(|media| media.on_timer(wparam.0))
            {
                Some(effect) => apply_effect(hwnd, effect),
                None => return DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
        WM_SHOWWINDOW => {
            // Lister minimized: don't keep playing a video unseen (music may go on). Plain
            // hide/show pairs also come from SetParent when toggling fullscreen and must not pause.
            if wparam.0 == 0 && lparam.0 as u32 == SW_PARENTCLOSING.0 {
                if let Some(media) = state.media.as_ref().filter(|m| m.is_video()) {
                    media.pause();
                }
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        WM_APPCOMMAND if state.media.is_some() => {
            // Multimedia keys delivered as app commands (GET_APPCOMMAND_LPARAM).
            let command = match ((lparam.0 >> 16) & 0x0FFF) as u32 {
                11 => Some(Command::NextTrack),     // APPCOMMAND_MEDIA_NEXTTRACK
                12 => Some(Command::PreviousTrack), // APPCOMMAND_MEDIA_PREVIOUSTRACK
                14 | 46 | 47 => Some(Command::TogglePlay), // PLAY_PAUSE, PLAY, PAUSE
                _ => None,
            };
            match command {
                Some(cmd) => {
                    execute(hwnd, cmd);
                    return LRESULT(1);
                }
                None => return DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
        WM_SIZE => {
            if let Some(media) = &state.media {
                media.layout();
            }
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
            if let Some(media) = state.media.as_mut() {
                match media.mouse_down(pt.x, pt.y) {
                    Click::Outside => {
                        media.toggle_play();
                        media.invalidate_bar();
                    }
                    Click::Bar if media.is_dragging() => {
                        let _ = SetCapture(hwnd);
                    }
                    Click::Bar => {}
                    Click::Skip(forward) => execute(hwnd, Command::track(forward)),
                }
                return LRESULT(0);
            }
            let (Some(img), Some(view)) = (state.image.clone(), client_size(hwnd)) else {
                return LRESULT(0);
            };
            if state.zoom == ZoomMode::Fit {
                image_view::loupe_begin(state, &img, view, (pt.x, pt.y));
                refresh(state);
            } else {
                state.drag = Some(Drag {
                    start: (pt.x, pt.y),
                    start_offset: state.offset,
                });
            }
            let _ = SetCapture(hwnd);
        }
        WM_MOUSEMOVE => {
            let pt = point_from_lparam(lparam.0);
            if let Some(fs) = state.overlay.as_mut() {
                fs.mouse_moved(hwnd);
            }
            if let Some(media) = state.media.as_mut() {
                media.mouse_move(pt.x);
                return LRESULT(0);
            }
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
            if let Some(media) = state.media.as_mut() {
                if media.is_dragging() {
                    media.mouse_up(point_from_lparam(lparam.0).x);
                }
                return LRESULT(0);
            }
            state.drag = None;
            if image_view::loupe_end(state) {
                refresh(state);
            }
        }
        WM_LBUTTONDBLCLK => {
            let pt = point_from_lparam(lparam.0);
            if let Some(media) = state.media.as_mut() {
                match media.mouse_down(pt.x, pt.y) {
                    // The first click of the pair already toggled playback: undo it.
                    Click::Outside => {
                        media.toggle_play();
                        execute(hwnd, Command::ToggleFullscreen);
                    }
                    Click::Bar if media.is_dragging() => {
                        let _ = SetCapture(hwnd);
                    }
                    Click::Bar => {}
                    Click::Skip(forward) => execute(hwnd, Command::track(forward)),
                }
                return LRESULT(0);
            }
            let _ = ReleaseCapture();
            execute(hwnd, Command::ToggleFullscreen);
        }
        WM_SETCURSOR => {
            // Fullscreen and idle: no cursor over the picture.
            if state.overlay.as_ref().is_some_and(|fs| fs.cursor_hidden()) {
                SetCursor(None);
                return LRESULT(1);
            }
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
            let mut pt = point_from_lparam(lparam.0);
            let _ = ScreenToClient(hwnd, &mut pt);
            // Over the picture the wheel pages through the files whatever they are; volume is
            // the wheel over the transport bar, or Ctrl+wheel.
            let volume = state
                .media
                .as_ref()
                .is_some_and(|m| ctrl_down() || m.over_bar(pt.x, pt.y));
            if volume {
                if delta != 0 {
                    execute(
                        hwnd,
                        if delta > 0 {
                            Command::VolumeUp
                        } else {
                            Command::VolumeDown
                        },
                    );
                }
            } else if ctrl_down() {
                if let Some(view) = client_size(hwnd) {
                    zoom_at(state, view, delta > 0, (pt.x as f32, pt.y as f32));
                }
            } else if delta != 0 {
                execute(
                    hwnd,
                    if delta < 0 {
                        Command::Next
                    } else {
                        Command::Previous
                    },
                );
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
            if vk == VK_ESCAPE && state.fullscreen.is_some() {
                execute(hwnd, Command::ToggleFullscreen);
            } else if let Some(cmd) =
                Command::from_key(vk, ctrl_down(), shift_down(), state.media.is_some())
            {
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

unsafe fn shift_down() -> bool {
    GetKeyState(VK_SHIFT.0 as i32) < 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_combinations_go_to_lister() {
        assert_eq!(
            Command::from_key(0x50, false, false, false),
            Some(Command::Previous)
        ); // P
        assert_eq!(Command::from_key(0x50, true, false, false), None); // Ctrl+P: print in Lister
        assert_eq!(
            Command::from_key(0x43, true, false, false),
            Some(Command::Copy)
        ); // Ctrl+C: the picture
        assert_eq!(
            Command::from_key(0x53, false, true, true),
            Some(Command::SaveFrame)
        ); // Shift+S
        assert_eq!(
            Command::from_key(0x53, false, false, true),
            Some(Command::ShowSettings)
        ); // S
        assert_eq!(
            Command::from_key(0xBB, true, false, false),
            Some(Command::ZoomIn)
        );
    }

    #[test]
    fn video_keys_override_navigation() {
        assert_eq!(
            Command::from_key(0x20, false, false, true),
            Some(Command::TogglePlay)
        ); // Space
        assert_eq!(
            Command::from_key(0x20, false, false, false),
            Some(Command::Next)
        );
        assert_eq!(
            Command::from_key(0x27, false, false, true),
            Some(Command::SeekForward)
        ); // Right
        assert_eq!(
            Command::from_key(0x26, false, false, true),
            Some(Command::KeyframeForward)
        ); // Up
        assert_eq!(
            Command::from_key(0x28, false, false, true),
            Some(Command::KeyframeBack)
        ); // Down
        assert_eq!(
            Command::from_key(0xBB, false, false, true),
            Some(Command::VolumeUp)
        ); // '+'
        assert_eq!(
            Command::from_key(0xBB, false, false, false),
            Some(Command::ZoomIn)
        ); // '+' on a photo
        assert_eq!(
            Command::from_key(0x22, false, false, true),
            Some(Command::Next)
        ); // PgDn still navigates
        assert_eq!(
            Command::from_key(0x4E, false, false, true),
            Some(Command::Next)
        ); // N
        assert_eq!(
            Command::from_key(0xB0, false, false, true),
            Some(Command::NextTrack)
        ); // media key
        assert_eq!(
            Command::from_key(0xB3, true, false, true),
            Some(Command::TogglePlay)
        );
        assert_eq!(Command::from_key(0xB0, false, false, false), None); // photos: to the Lister
    }

    #[test]
    fn photo_and_video_tool_keys() {
        assert_eq!(
            Command::from_key(0x52, false, false, false),
            Some(Command::RotateRight)
        ); // R
        assert_eq!(
            Command::from_key(0x4C, false, false, false),
            Some(Command::RotateLeft)
        ); // L
        assert_eq!(
            Command::from_key(0x2E, false, false, false),
            Some(Command::Delete)
        ); // Del
        assert_eq!(
            Command::from_key(0x2E, false, false, true),
            Some(Command::Delete)
        );
        assert_eq!(
            Command::from_key(0xDD, false, false, true),
            Some(Command::Faster)
        ); // ]
        assert_eq!(
            Command::from_key(0xBC, false, false, true),
            Some(Command::FrameBack)
        ); // ,
        assert_eq!(Command::from_key(0xBE, false, false, false), None); // '.' on a photo: to the Lister
    }

    #[test]
    fn every_command_round_trips_through_posted_messages() {
        for (cmd, _) in Command::MENU.iter().flatten() {
            assert_eq!(Command::from_raw(*cmd as usize), Some(*cmd));
        }
    }

    #[test]
    fn raw_values_outside_the_commands() {
        assert_eq!(Command::from_raw(0), None);
        assert_eq!(Command::from_raw(Command::FrameForward as usize + 1), None);
        assert_eq!(Command::from_raw(1), Some(Command::ToggleFullscreen));
        for value in 1..=Command::FrameForward as usize {
            assert_eq!(Command::from_raw(value).map(|c| c as usize), Some(value));
        }
    }
}
