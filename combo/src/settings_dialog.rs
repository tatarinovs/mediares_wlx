//! Modal settings dialog for the viewer configuration.

use std::cell::Cell;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

mod controls;
use controls::build_controls;

use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    InvalidateRect, MapWindowPoints, SetBkColor, SetBkMode, SetTextColor, HDC, OPAQUE,
};
use windows::Win32::UI::Controls::Dialogs::{
    ChooseColorW, CommDlgExtendedError, GetOpenFileNameW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW,
    FNERR_INVALIDFILENAME, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
    OPENFILENAMEW,
};
use windows::Win32::UI::Controls::{
    EnableThemeDialogTexture, InitCommonControlsEx, ICC_TAB_CLASSES, INITCOMMONCONTROLSEX, NMHDR,
    TCIF_TEXT, TCITEMW, TCM_ADJUSTRECT, TCM_GETCURSEL, TCM_INSERTITEMW, TCM_SETCURSEL,
    TCN_SELCHANGE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateDialogIndirectParamW, DefWindowProcW, GetDlgCtrlID, GetDlgItem, GetParent,
    GetWindowLongPtrW, GetWindowRect, IsWindowVisible, MessageBoxW, SendMessageW,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, BM_GETCHECK, BM_SETCHECK,
    BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_GROUPBOX, BS_PUSHBUTTON, CBS_DROPDOWNLIST, CB_ADDSTRING,
    CB_GETCURSEL, CB_SETCURSEL, DLGTEMPLATE, DS_CONTROL, GWLP_ID, GWLP_USERDATA, HWND_BOTTOM,
    IDCANCEL, IDOK, MB_ICONINFORMATION, MB_OK, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    SW_SHOWNA, WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC, WM_NCDESTROY, WM_NOTIFY, WS_BORDER,
    WS_CHILD, WS_CLIPSIBLINGS, WS_TABSTOP, WS_VSCROLL,
};

use crate::config::{OsdMode, ViewerConfig};
use crate::dialog::{self, ES_AUTOHSCROLL, SS_LEFT};
use crate::file_actions::{show_error, split_command};
use crate::gdi::{self, Brush};
use crate::i18n::{n, tr, LangSetting};
use crate::playlist::Repeat;
use crate::snapshot::PictureFormat;
use crate::tc_register::Registration;
use crate::{module, osd_template, osd_template_dialog};

const CLASS_NAME: PCWSTR = w!("MediaresSettingsDialogClass");

const IDC_START_FULLSCREEN: i32 = 101;
const IDC_AUTO_ROTATE_EXIF: i32 = 102;
const IDC_LOUPE_SCALE: i32 = 103;
const IDC_SHOW_OSD: i32 = 104;
const IDC_FONT_SIZE: i32 = 105;
const IDC_CHOOSE_COLOR: i32 = 106;
const IDC_COLOR_PREVIEW: i32 = 107;
const IDC_AUTO_ADVANCE: i32 = 108;
const IDC_REPEAT: i32 = 109;
const IDC_SHUFFLE: i32 = 110;
const IDC_OVERLAY_PHOTO: i32 = 111;
const IDC_OVERLAY_VIDEO: i32 = 112;
const IDC_OVERLAY_AUTOHIDE: i32 = 113;
const IDC_SLIDESHOW: i32 = 114;
const IDC_CHOOSE_BACKGROUND: i32 = 115;
const IDC_BACKGROUND_PREVIEW: i32 = 116;
const IDC_CONFIRM_DELETE: i32 = 117;
const IDC_RESUME_VIDEO: i32 = 118;
const IDC_PHOTO_EDITOR: i32 = 119;
const IDC_BROWSE_PHOTO_EDITOR: i32 = 120;
const IDC_VIDEO_EDITOR: i32 = 121;
const IDC_BROWSE_VIDEO_EDITOR: i32 = 122;
const IDC_AUDIO_EDITOR: i32 = 123;
const IDC_BROWSE_AUDIO_EDITOR: i32 = 124;
const IDC_REGISTER_WDX: i32 = 125;
const IDC_PHOTO_OSD: i32 = 126;
const IDC_VIDEO_OSD: i32 = 127;
const IDC_NO_UPSCALE: i32 = 128;
const IDC_LANGUAGE: i32 = 129;
const IDC_FRAME_FORMAT: i32 = 130;
const IDC_SMOOTH_ZOOM: i32 = 131;
const IDC_SEEK_STEP: i32 = 132;
const IDC_KEEP_ZOOM: i32 = 133;
const IDC_SKIP_RAW_TWINS: i32 = 134;
const IDC_REPLAY_GAIN: i32 = 135;
const IDC_TABS: i32 = 136;
const IDC_OSD_FONT_FAMILY: i32 = 137;
const IDC_WHEEL_ZOOM: i32 = 138;
const IDC_SEEK_PREVIEW: i32 = 139;
const IDC_RESUME_THRESHOLD: i32 = 141;
const IDC_CONTACT_COLUMNS: i32 = 142;
const IDC_CONTACT_ROWS: i32 = 143;

const PAGE_COUNT: usize = 4;
const PAGE_TITLES: [&str; PAGE_COUNT] =
    [n("Photo"), n("Video and audio"), n("Display"), n("General")];
/// The page the dialog opens at: the one last looked at in this session.
static LAST_PAGE: AtomicUsize = AtomicUsize::new(0);
/// Each page is a child dialog holding its controls, laid over the tab strip's pane.
type Pages = [HWND; PAGE_COUNT];
/// Control id of page `i` in the dialog: `PAGE_ID + i`.
const PAGE_ID: i32 = 200;

/// `ETDT_ENABLETAB`: the page draws the themed tab pane under its controls, as a property sheet.
const ETDT_ENABLETAB: u32 = 6;

/// Left edge of a page's controls, of the controls inside a group box, and of the value column.
const COLUMN: i32 = 25;
const GROUPED: i32 = 33;
const FIELD: i32 = 210;

/// The page grid, at 96 DPI: rows are [`ROW`] apart; a group box puts its first row
/// [`GROUP_TITLE`] below its top, leaves [`GROUP_PAD`] under the last, and the next group starts
/// [`GROUP_GAP`] lower.
const ROW: i32 = 26;
const GROUP_TITLE: i32 = 20;
const GROUP_PAD: i32 = 10;
const GROUP_GAP: i32 = 10;
/// Checkboxes and one-line labels are this high; a combo's dropped list is [`COMBO_HEIGHT`] tall.
const ROW_HEIGHT: i32 = 22;
const COMBO_HEIGHT: i32 = 160;
const BUTTON_HEIGHT: i32 = 24;

/// Group boxes, the controls inside them, and the top of the first group on a page.
const GROUP_X: i32 = COLUMN - 7;
const GROUP_W: i32 = 446;
const GROUPED_W: i32 = 420;
const PAGE_TOP: i32 = 40;

/// `EM_SETCUEBANNER`: grey hint text in an empty edit box.
const EM_SETCUEBANNER: u32 = 0x1501;

const BST_CHECKED: usize = 1;
const SS_CENTER: u32 = 0x0001;
const SS_CENTERIMAGE: u32 = 0x0200;
const SS_SUNKEN: u32 = 0x1000;

const LOUPE_SCALES: &[f32] = &[1.0, 1.5, 2.0, 2.5, 3.0];
const FONT_SIZES: &[i32] = &[10, 12, 14, 16, 18, 20, 24, 28, 32];
const SLIDESHOW_SECONDS: &[u32] = &[2, 3, 4, 5, 7, 10, 15, 30, 60];
const SEEK_STEPS: &[u32] = &[2, 3, 5, 10, 15, 20, 30, 60];
/// Minutes, as seconds.
const RESUME_THRESHOLDS: &[u32] = &[60, 120, 180, 300, 600, 900, 1800];
const GRID_SIZES: &[u32] = &[2, 3, 4, 5, 6, 8];
const FONT_FAMILIES: &[&str] = &[
    "Segoe UI",
    "Segoe UI Light",
    "Segoe UI Semibold",
    "Tahoma",
    "Verdana",
    "Arial",
    "Calibri",
    "Consolas",
    "Georgia",
    "Times New Roman",
];

/// Custom colors of the color picker, kept for the session; `None` until first used.
static CUSTOM_COLORS: Mutex<Option<[COLORREF; 16]>> = Mutex::new(None);

/// Offered in the picker's custom colors: black, the default dark, grays, white.
const PRESET_COLORS: [u32; 6] = [
    0x0000_0000,
    0x0018_1818,
    0x0040_4040,
    0x0080_8080,
    0x00C0_C0C0,
    0x00FF_FFFF,
];

struct Context {
    /// The settings as the dialog opened, to save only what changed.
    initial: ViewerConfig,
    config: ViewerConfig,
    color: u32,
    background: u32,
    loupe_scales: Vec<f32>,
    font_sizes: Vec<i32>,
    slideshow_seconds: Vec<u32>,
    seek_steps: Vec<u32>,
    resume_thresholds: Vec<u32>,
    grid_columns: Vec<u32>,
    grid_rows: Vec<u32>,
    font_families: Vec<String>,
    /// Fills both previews: the background swatch and the OSD "Aa" (shown on that background).
    background_brush: Brush,
    /// Read from `wincmd.ini` on every opening: TC or the user may have changed the plugin list.
    registration: Option<Registration>,
    pages: Pages,
    result: Option<ViewerConfig>,
}

/// Shows the dialog; returns the new (already saved) configuration if the user pressed OK.
pub unsafe fn show(owner: HWND, current: &ViewerConfig) -> Option<ViewerConfig> {
    let controls = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_TAB_CLASSES,
    };
    let _ = InitCommonControlsEx(&controls);
    dialog::register_class(CLASS_NAME, Some(wnd_proc));
    let dlg = dialog::create_frame(owner, CLASS_NAME, tr("Mediares Settings"), 502, 470)?;

    let ctx = Box::into_raw(Box::new(Context {
        initial: current.clone(),
        config: current.clone(),
        color: current.osd_font_color,
        background: current.photo_background,
        loupe_scales: with_current(LOUPE_SCALES, current.loupe_scale, |a, b| {
            (a - b).abs() < 0.05
        }),
        font_sizes: with_current(FONT_SIZES, current.osd_font_size, |a, b| a == b),
        slideshow_seconds: with_current(SLIDESHOW_SECONDS, current.slideshow_seconds, |a, b| {
            a == b
        }),
        seek_steps: with_current(SEEK_STEPS, current.seek_step_sec, |a, b| a == b),
        resume_thresholds: with_current(RESUME_THRESHOLDS, current.resume_threshold_sec, |a, b| {
            a == b
        }),
        grid_columns: with_current(GRID_SIZES, current.contact_sheet_columns, |a, b| a == b),
        grid_rows: with_current(GRID_SIZES, current.contact_sheet_rows, |a, b| a == b),
        font_families: with_current_str(FONT_FAMILIES, &current.osd_font_name),
        background_brush: gdi::solid_brush(current.photo_background),
        registration: Registration::find(),
        pages: Pages::default(),
        result: None,
    }));
    SetWindowLongPtrW(dlg, GWLP_USERDATA, ctx as isize);

    let font = dialog::font(dlg, "Segoe UI", -12);
    let (ok, pages) = build_controls(dlg, &*ctx, font.0);
    (*ctx).pages = pages;
    show_page(
        dlg,
        &*ctx,
        LAST_PAGE.load(Ordering::Relaxed).min(PAGE_COUNT - 1),
    );
    dialog::run_modal(dlg, ok);

    // run_modal returns only after the window is destroyed, so nothing references `ctx` anymore.
    Box::from_raw(ctx).result
}

/// Standard options plus the configured value if it isn't one of them (e.g. edited in the INI).
fn with_current<T: Copy + PartialOrd>(
    standard: &[T],
    current: T,
    same: impl Fn(T, T) -> bool,
) -> Vec<T> {
    let mut options = standard.to_vec();
    if !options.iter().any(|&o| same(o, current)) {
        options.push(current);
        options.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    }
    options
}

/// Like [`with_current`], for the font family combo: a name not in `standard` (set in the INI by
/// hand) is kept, compared case-insensitively.
fn with_current_str(standard: &[&str], current: &str) -> Vec<String> {
    let mut options: Vec<String> = standard.iter().map(ToString::to_string).collect();
    if !options.iter().any(|o| o.eq_ignore_ascii_case(current)) {
        options.push(current.to_string());
    }
    options
}

/// Empty child dialogs over the pane of `tabs`, with the themed tab background under whatever is
/// put on them (`DefDlgProc` paints it and answers the controls' `WM_CTLCOLOR*`). Returns them and
/// their top-left corner in 96 DPI dialog coordinates.
unsafe fn create_pages(dlg: HWND, tabs: HWND) -> (Pages, (i32, i32)) {
    let mut pane = RECT::default();
    let _ = GetWindowRect(tabs, &mut pane);
    let corners = std::slice::from_raw_parts_mut(&mut pane as *mut RECT as *mut _, 2);
    MapWindowPoints(None, Some(dlg), corners);
    SendMessageW(
        tabs,
        TCM_ADJUSTRECT,
        Some(WPARAM(0)),
        Some(LPARAM(&mut pane as *mut _ as isize)),
    );

    // DLGTEMPLATE, then empty menu, class and title; the system wants it DWORD-aligned.
    #[repr(C, align(4))]
    struct Template(DLGTEMPLATE, [u16; 3]);
    let template = Template(
        DLGTEMPLATE {
            style: WS_CHILD.0 | DS_CONTROL as u32,
            ..Default::default()
        },
        [0; 3],
    );
    let mut pages = Pages::default();
    for (i, page) in pages.iter_mut().enumerate() {
        let Ok(hwnd) = CreateDialogIndirectParamW(
            Some(module()),
            &template.0,
            Some(dlg),
            Some(page_proc),
            LPARAM(0),
        ) else {
            continue;
        };
        SetWindowLongPtrW(hwnd, GWLP_ID, (PAGE_ID + i as i32) as isize);
        let _ = EnableThemeDialogTexture(hwnd, ETDT_ENABLETAB);
        let _ = SetWindowPos(
            hwnd,
            None,
            pane.left,
            pane.top,
            pane.right - pane.left,
            pane.bottom - pane.top,
            SWP_NOACTIVATE,
        );
        *page = hwnd;
    }
    let scale = gdi::dpi_scale(dlg);
    let unscale = |v: i32| (v as f32 / scale).round() as i32;
    (pages, (unscale(pane.left), unscale(pane.top)))
}

/// A page hands its controls' commands and the previews' colors to the dialog.
unsafe extern "system" fn page_proc(page: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> isize {
    match msg {
        WM_COMMAND => {
            if let Ok(dlg) = GetParent(page) {
                SendMessageW(dlg, msg, Some(wparam), Some(lparam));
            }
            1
        }
        WM_CTLCOLORSTATIC
            if matches!(
                GetDlgCtrlID(HWND(lparam.0 as *mut _)),
                IDC_COLOR_PREVIEW | IDC_BACKGROUND_PREVIEW
            ) =>
        {
            GetParent(page).map_or(0, |dlg| {
                SendMessageW(dlg, msg, Some(wparam), Some(lparam)).0
            })
        }
        _ => 0,
    }
}

/// The control `id`, on whichever page it is. `pages` are the already-resolved page windows
/// (`ctx.pages`), so this never re-discovers them through `dlg`.
unsafe fn item(dlg: HWND, pages: &Pages, id: i32) -> HWND {
    if let Ok(hwnd) = GetDlgItem(Some(dlg), id) {
        return hwnd;
    }
    pages
        .iter()
        .find_map(|&page| GetDlgItem(Some(page), id).ok())
        .unwrap_or_default()
}

/// Shows page `index` (and selects its tab), hides the others; keeps the keyboard focus visible.
unsafe fn show_page(dlg: HWND, ctx: &Context, index: usize) {
    LAST_PAGE.store(index, Ordering::Relaxed);
    let Ok(tabs) = GetDlgItem(Some(dlg), IDC_TABS) else {
        return;
    };
    SendMessageW(tabs, TCM_SETCURSEL, Some(WPARAM(index)), None);
    for (i, &page) in ctx.pages.iter().enumerate() {
        let _ = ShowWindow(page, if i == index { SW_SHOWNA } else { SW_HIDE });
    }
    // A hidden control can't keep the focus: hand it to the tab strip.
    let focus = GetFocus();
    if focus.is_invalid() || !IsWindowVisible(focus).as_bool() {
        let _ = SetFocus(Some(tabs));
    }
}

/// Ctrl+Tab / Ctrl+Shift+Tab from the modal loop: the next / previous page.
unsafe fn turn_page(dlg: HWND, ctx: &Context, back: bool) {
    let current = LAST_PAGE.load(Ordering::Relaxed).min(PAGE_COUNT - 1);
    let next = if back {
        (current + PAGE_COUNT - 1) % PAGE_COUNT
    } else {
        (current + 1) % PAGE_COUNT
    };
    show_page(dlg, ctx, next);
}

/// Opens the template editor; the result is kept in `ctx` and saved with OK.
unsafe fn edit_osd_template(dlg: HWND, ctx: &mut Context, video: bool) {
    let cfg = &mut ctx.config;
    let (title, template, default, fields) = if video {
        (
            tr("Video OSD"),
            &mut cfg.video_osd,
            osd_template::DEFAULT_VIDEO,
            osd_template::VIDEO_FIELDS,
        )
    } else {
        (
            tr("Photo OSD"),
            &mut cfg.photo_osd,
            osd_template::DEFAULT_PHOTO,
            osd_template::PHOTO_FIELDS,
        )
    };
    if let Some(edited) = osd_template_dialog::show(dlg, title, template, default, fields) {
        *template = edited;
    }
}

fn registered_label() -> &'static str {
    tr("WDX registered")
}

/// Adds this DLL to `[ContentPlugins]` right away (independent of OK / Cancel), then greys the button.
unsafe fn register_wdx(dlg: HWND, ctx: &mut Context) {
    let Some(registration) = ctx.registration.as_mut() else {
        show_error(
            dlg,
            tr("wincmd.ini not found. Add the plugin manually: Configuration → Options → Plugins → Content plugins → Add, select mediares.wlx64."),
        );
        return;
    };
    if let Err(message) = registration.register() {
        show_error(dlg, &message);
        return;
    }
    MessageBoxW(
        Some(dlg),
        &HSTRING::from(tr("WDX registered, restart Total Commander.")),
        w!("Mediares"),
        MB_OK | MB_ICONINFORMATION,
    );
    let button = item(dlg, &ctx.pages, IDC_REGISTER_WDX);
    let _ = SetWindowTextW(button, &HSTRING::from(registered_label()));
    let _ = EnableWindow(button, false);
    // The disabled button can't keep the keyboard focus.
    if let Ok(ok) = GetDlgItem(Some(dlg), IDOK.0) {
        let _ = SetFocus(Some(ok));
    }
}

unsafe fn is_checked(dlg: HWND, pages: &Pages, id: i32) -> bool {
    SendMessageW(item(dlg, pages, id), BM_GETCHECK, None, None).0 as usize == BST_CHECKED
}

/// "Язык / Language:": the English word stays, so a wrong language is easy to switch back.
fn language_caption() -> String {
    match tr("Language") {
        "Language" => "Language:".to_string(),
        translated => format!("{translated} / Language:"),
    }
}

unsafe fn selected<T: Clone>(dlg: HWND, pages: &Pages, id: i32, options: &[T]) -> Option<T> {
    let idx = SendMessageW(item(dlg, pages, id), CB_GETCURSEL, None, None).0;
    usize::try_from(idx)
        .ok()
        .and_then(|i| options.get(i).cloned())
}

unsafe fn accept(dlg: HWND, ctx: &mut Context) {
    let pages = &ctx.pages;
    let cfg = &mut ctx.config;
    cfg.language = selected(dlg, pages, IDC_LANGUAGE, &LangSetting::all()).unwrap_or(cfg.language);
    cfg.start_fullscreen = is_checked(dlg, pages, IDC_START_FULLSCREEN);
    cfg.auto_rotate_exif = is_checked(dlg, pages, IDC_AUTO_ROTATE_EXIF);
    cfg.osd = selected(dlg, pages, IDC_SHOW_OSD, &OsdMode::ALL).unwrap_or(cfg.osd);
    cfg.loupe_scale =
        selected(dlg, pages, IDC_LOUPE_SCALE, &ctx.loupe_scales).unwrap_or(cfg.loupe_scale);
    cfg.osd_font_size =
        selected(dlg, pages, IDC_FONT_SIZE, &ctx.font_sizes).unwrap_or(cfg.osd_font_size);
    cfg.osd_font_color = ctx.color;
    cfg.queue.auto_advance = is_checked(dlg, pages, IDC_AUTO_ADVANCE);
    cfg.queue.repeat = selected(dlg, pages, IDC_REPEAT, &Repeat::ALL).unwrap_or(cfg.queue.repeat);
    cfg.queue.shuffle = is_checked(dlg, pages, IDC_SHUFFLE);
    cfg.overlay_photo = is_checked(dlg, pages, IDC_OVERLAY_PHOTO);
    cfg.overlay_video = is_checked(dlg, pages, IDC_OVERLAY_VIDEO);
    cfg.overlay_autohide = is_checked(dlg, pages, IDC_OVERLAY_AUTOHIDE);
    cfg.slideshow_seconds = selected(dlg, pages, IDC_SLIDESHOW, &ctx.slideshow_seconds)
        .unwrap_or(cfg.slideshow_seconds);
    cfg.photo_background = ctx.background;
    cfg.no_upscale = is_checked(dlg, pages, IDC_NO_UPSCALE);
    cfg.smooth_zoom = is_checked(dlg, pages, IDC_SMOOTH_ZOOM);
    cfg.keep_zoom = is_checked(dlg, pages, IDC_KEEP_ZOOM);
    cfg.skip_raw_twins = is_checked(dlg, pages, IDC_SKIP_RAW_TWINS);
    cfg.replay_gain = is_checked(dlg, pages, IDC_REPLAY_GAIN);
    cfg.confirm_delete = is_checked(dlg, pages, IDC_CONFIRM_DELETE);
    cfg.resume_video = is_checked(dlg, pages, IDC_RESUME_VIDEO);
    cfg.resume_threshold_sec = selected(dlg, pages, IDC_RESUME_THRESHOLD, &ctx.resume_thresholds)
        .unwrap_or(cfg.resume_threshold_sec);
    cfg.seek_preview = is_checked(dlg, pages, IDC_SEEK_PREVIEW);
    cfg.wheel_zoom = is_checked(dlg, pages, IDC_WHEEL_ZOOM);
    cfg.seek_step_sec =
        selected(dlg, pages, IDC_SEEK_STEP, &ctx.seek_steps).unwrap_or(cfg.seek_step_sec);
    cfg.frame_format =
        selected(dlg, pages, IDC_FRAME_FORMAT, &PictureFormat::ALL).unwrap_or(cfg.frame_format);
    cfg.contact_sheet_columns = selected(dlg, pages, IDC_CONTACT_COLUMNS, &ctx.grid_columns)
        .unwrap_or(cfg.contact_sheet_columns);
    cfg.contact_sheet_rows =
        selected(dlg, pages, IDC_CONTACT_ROWS, &ctx.grid_rows).unwrap_or(cfg.contact_sheet_rows);
    cfg.osd_font_name = selected(dlg, pages, IDC_OSD_FONT_FAMILY, &ctx.font_families)
        .unwrap_or_else(|| cfg.osd_font_name.clone());
    cfg.photo_editor = edit_text(dlg, pages, IDC_PHOTO_EDITOR);
    cfg.video_editor = edit_text(dlg, pages, IDC_VIDEO_EDITOR);
    cfg.audio_editor = edit_text(dlg, pages, IDC_AUDIO_EDITOR);
    if !cfg.save(&ctx.initial) {
        show_error(
            dlg,
            &format!(
                "{}\n\n{}",
                tr("Could not save the settings to this file; they apply until Total Commander is closed:"),
                crate::config::ini_path().display()
            ),
        );
    }
    ctx.result = Some(cfg.clone());
}

/// The system color picker starting at `initial`; `None` if cancelled.
unsafe fn pick_color(dlg: HWND, initial: u32) -> Option<u32> {
    let mut custom = CUSTOM_COLORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or_else(|| {
            let mut colors = [COLORREF(0x00FF_FFFF); 16];
            for (slot, &c) in colors.iter_mut().zip(&PRESET_COLORS) {
                *slot = COLORREF(c);
            }
            colors
        });
    let mut cc = CHOOSECOLORW {
        lStructSize: size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: dlg,
        rgbResult: COLORREF(initial),
        lpCustColors: custom.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        ..Default::default()
    };
    let picked = ChooseColorW(&mut cc).as_bool().then_some(cc.rgbResult.0);
    *CUSTOM_COLORS.lock().unwrap_or_else(|e| e.into_inner()) = Some(custom);
    picked
}

unsafe fn repaint_previews(dlg: HWND, pages: &Pages) {
    for id in [IDC_COLOR_PREVIEW, IDC_BACKGROUND_PREVIEW] {
        let _ = InvalidateRect(Some(item(dlg, pages, id)), None, true);
    }
}

unsafe fn edit_text(dlg: HWND, pages: &Pages, id: i32) -> String {
    dialog::window_text(item(dlg, pages, id)).trim().to_string()
}

/// "Обзор...": picks a program and puts its path into the edit box `edit_id`, keeping the
/// arguments already written after the old one.
unsafe fn browse_program(dlg: HWND, pages: &Pages, edit_id: i32) {
    let command = edit_text(dlg, pages, edit_id);
    let (program, args) = split_command(&command, |p| Path::new(p).is_file());
    let filter = HSTRING::from(tr("Programs (*.exe)\0*.exe\0All files (*.*)\0*.*\0"));
    let title = HSTRING::from(tr("Choose an editor"));
    // The dialog opens at the current program; a name it rejects (it refuses to open at all
    // then) is dropped.
    for initial in [program, ""] {
        let mut file = [0u16; 1024];
        let name: Vec<u16> = initial.encode_utf16().take(file.len() - 1).collect();
        file[..name.len()].copy_from_slice(&name);
        let mut ofn = OPENFILENAMEW {
            lStructSize: size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: dlg,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: PWSTR(file.as_mut_ptr()),
            nMaxFile: file.len() as u32,
            lpstrTitle: PCWSTR(title.as_ptr()),
            Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_HIDEREADONLY,
            ..Default::default()
        };
        if GetOpenFileNameW(&mut ofn).as_bool() {
            let len = file.iter().position(|&c| c == 0).unwrap_or(file.len());
            let picked = String::from_utf16_lossy(&file[..len]);
            let text = if args.is_empty() {
                picked
            } else {
                format!("\"{picked}\" {args}")
            };
            let _ = SetWindowTextW(item(dlg, pages, edit_id), &HSTRING::from(text));
            return;
        }
        if initial.is_empty() || CommDlgExtendedError() != FNERR_INVALIDFILENAME {
            return;
        }
    }
}

unsafe fn choose_color(dlg: HWND, ctx: &mut Context) {
    if let Some(color) = pick_color(dlg, ctx.color) {
        ctx.color = color;
        repaint_previews(dlg, &ctx.pages);
    }
}

unsafe fn choose_background(dlg: HWND, ctx: &mut Context) {
    if let Some(color) = pick_color(dlg, ctx.background) {
        ctx.background = color;
        ctx.background_brush = gdi::solid_brush(color);
        repaint_previews(dlg, &ctx.pages);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    mediares_core::ffi::guard(LRESULT(0), || unsafe {
        let ctx = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Context).as_mut();
        let Some(ctx) = ctx else {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        };

        match msg {
            WM_CTLCOLORSTATIC
                if matches!(
                    GetDlgCtrlID(HWND(lparam.0 as *mut _)),
                    IDC_COLOR_PREVIEW | IDC_BACKGROUND_PREVIEW
                ) =>
            {
                let hdc = HDC(wparam.0 as *mut _);
                SetBkMode(hdc, OPAQUE);
                SetBkColor(hdc, COLORREF(ctx.background));
                SetTextColor(hdc, COLORREF(ctx.color));
                LRESULT(ctx.background_brush.0 .0 as isize)
            }
            WM_NOTIFY => {
                let header = &*(lparam.0 as *const NMHDR);
                if header.idFrom == IDC_TABS as usize && header.code == TCN_SELCHANGE {
                    let index = SendMessageW(header.hwndFrom, TCM_GETCURSEL, None, None).0;
                    if let Ok(index) = usize::try_from(index) {
                        show_page(hwnd, ctx, index);
                    }
                }
                LRESULT(0)
            }
            dialog::WM_TURN_PAGE => {
                turn_page(hwnd, ctx, wparam.0 != 0);
                LRESULT(1)
            }
            WM_COMMAND => {
                match dialog::loword(wparam) as i32 {
                    IDC_CHOOSE_COLOR => choose_color(hwnd, ctx),
                    IDC_CHOOSE_BACKGROUND => choose_background(hwnd, ctx),
                    IDC_BROWSE_PHOTO_EDITOR => browse_program(hwnd, &ctx.pages, IDC_PHOTO_EDITOR),
                    IDC_BROWSE_VIDEO_EDITOR => browse_program(hwnd, &ctx.pages, IDC_VIDEO_EDITOR),
                    IDC_BROWSE_AUDIO_EDITOR => browse_program(hwnd, &ctx.pages, IDC_AUDIO_EDITOR),
                    IDC_REGISTER_WDX => register_wdx(hwnd, ctx),
                    IDC_PHOTO_OSD => edit_osd_template(hwnd, ctx, false),
                    IDC_VIDEO_OSD => edit_osd_template(hwnd, ctx, true),
                    id if id == IDOK.0 => {
                        accept(hwnd, ctx);
                        dialog::close(hwnd);
                    }
                    id if id == IDCANCEL.0 => dialog::close(hwnd),
                    _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                dialog::close(hwnd);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_value_is_kept_in_options() {
        assert_eq!(with_current(LOUPE_SCALES, 2.0, |a, b| a == b), LOUPE_SCALES);
        assert_eq!(with_current(FONT_SIZES, 15, |a, b| a == b)[3], 15);
        assert!(with_current(LOUPE_SCALES, 4.0, |a, b| a == b).contains(&4.0));
    }

    #[test]
    fn custom_font_is_kept_case_insensitively() {
        assert_eq!(
            with_current_str(FONT_FAMILIES, "segoe ui").len(),
            FONT_FAMILIES.len()
        );
        assert!(
            with_current_str(FONT_FAMILIES, "Comic Sans MS").contains(&"Comic Sans MS".to_string())
        );
    }
}
