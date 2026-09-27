//! Modal settings dialog for the viewer configuration.

use std::sync::Mutex;

use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    InvalidateRect, SetBkColor, SetBkMode, SetTextColor, HDC, OPAQUE,
};
use windows::Win32::UI::Controls::Dialogs::{
    ChooseColorW, GetOpenFileNameW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, OFN_FILEMUSTEXIST,
    OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, GetDlgCtrlID, GetDlgItem, GetDlgItemTextW, GetWindowLongPtrW, MessageBoxW,
    SendDlgItemMessageW, SetDlgItemTextW, SetWindowLongPtrW, BM_GETCHECK, BM_SETCHECK,
    BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_GROUPBOX, BS_PUSHBUTTON, CBS_DROPDOWNLIST, CB_ADDSTRING,
    CB_GETCURSEL, CB_SETCURSEL, GWLP_USERDATA, IDCANCEL, IDOK, MB_ICONINFORMATION, MB_OK, WM_CLOSE,
    WM_COMMAND, WM_CTLCOLORSTATIC, WM_NCDESTROY, WS_BORDER, WS_TABSTOP, WS_VSCROLL,
};

use crate::config::{OsdMode, ViewerConfig};
use crate::dialog;
use crate::file_actions::show_error;
use crate::gdi::{self, Brush};
use crate::i18n::{tr, LangSetting};
use crate::playlist::Repeat;
use crate::snapshot::FrameFormat;
use crate::tc_register::Registration;
use crate::{osd_template, osd_template_dialog};

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

const ES_AUTOHSCROLL: i32 = 0x0080;
/// `EM_SETCUEBANNER`: grey hint text in an empty edit box.
const EM_SETCUEBANNER: u32 = 0x1501;

const BST_CHECKED: usize = 1;
const SS_LEFT: u32 = 0x0000;
const SS_CENTER: u32 = 0x0001;
const SS_CENTERIMAGE: u32 = 0x0200;
const SS_SUNKEN: u32 = 0x1000;

const LOUPE_SCALES: &[f32] = &[1.0, 1.5, 2.0, 2.5, 3.0];
const FONT_SIZES: &[i32] = &[10, 12, 14, 16, 18, 20, 24, 28, 32];
const SLIDESHOW_SECONDS: &[u32] = &[2, 3, 4, 5, 7, 10, 15, 30, 60];

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
    config: ViewerConfig,
    color: u32,
    background: u32,
    loupe_scales: Vec<f32>,
    font_sizes: Vec<i32>,
    slideshow_seconds: Vec<u32>,
    /// Fills both previews: the background swatch and the OSD "Aa" (shown on that background).
    background_brush: Brush,
    /// Read from `wincmd.ini` on every opening: TC or the user may have changed the plugin list.
    registration: Option<Registration>,
    result: Option<ViewerConfig>,
}

/// Shows the dialog; returns the new (already saved) configuration if the user pressed OK.
pub unsafe fn show(owner: HWND, current: &ViewerConfig) -> Option<ViewerConfig> {
    dialog::register_class(CLASS_NAME, Some(wnd_proc));
    let dlg = dialog::create_frame(
        owner,
        CLASS_NAME,
        tr("Настройки Mediares", "Mediares Settings"),
        850,
        625,
    )?;

    let ctx = Box::into_raw(Box::new(Context {
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
        background_brush: gdi::solid_brush(current.photo_background),
        registration: Registration::find(),
        result: None,
    }));
    SetWindowLongPtrW(dlg, GWLP_USERDATA, ctx as isize);

    let font = gdi::create_font("Segoe UI", -12, false);
    let ok = build_controls(dlg, &*ctx, font.0);
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

unsafe fn build_controls(
    dlg: HWND,
    ctx: &Context,
    font: windows::Win32::Graphics::Gdi::HFONT,
) -> HWND {
    let cfg = &ctx.config;
    let tab = WS_TABSTOP.0;
    let control = |class, text: &str, style: u32, rect, id: i32| {
        dialog::control(dlg, class, text, style, rect, id as usize, font)
    };
    let checkbox = |text: &str, rect, id: i32, checked: bool| {
        control(w!("BUTTON"), text, tab | BS_AUTOCHECKBOX as u32, rect, id);
        if checked {
            SendDlgItemMessageW(dlg, id, BM_SETCHECK, WPARAM(BST_CHECKED), LPARAM(0));
        }
    };
    let combo = |rect, id: i32, items: Vec<String>, selected: usize| {
        control(
            w!("COMBOBOX"),
            "",
            tab | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
            rect,
            id,
        );
        for item in items {
            let text = HSTRING::from(item);
            SendDlgItemMessageW(
                dlg,
                id,
                CB_ADDSTRING,
                WPARAM(0),
                LPARAM(text.as_ptr() as isize),
            );
        }
        SendDlgItemMessageW(dlg, id, CB_SETCURSEL, WPARAM(selected), LPARAM(0));
    };
    // "<label> [path] [Обзор...]" at label baseline `y` in the right column; empty = the system's choice.
    let program_row = |y: i32, label: &str, path: &str, edit_id: i32, browse_id: i32| {
        control(w!("STATIC"), label, SS_LEFT, (438, y, 55, 20), 0);
        control(
            w!("EDIT"),
            path,
            tab | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
            (495, y - 3, 235, 24),
            edit_id,
        );
        control(
            w!("BUTTON"),
            tr("Обзор...", "Browse..."),
            tab | BS_PUSHBUTTON as u32,
            (738, y - 4, 72, 26),
            browse_id,
        );
        let hint = HSTRING::from(tr(
            "программа, назначенная в Windows",
            "the program assigned in Windows",
        ));
        SendDlgItemMessageW(
            dlg,
            edit_id,
            EM_SETCUEBANNER,
            WPARAM(1),
            LPARAM(hint.as_ptr() as isize),
        );
    };

    // Left column: photos, OSD, fullscreen.
    checkbox(
        tr("Запускать в полноэкранном режиме", "Start in full screen"),
        (20, 15, 380, 22),
        IDC_START_FULLSCREEN,
        cfg.start_fullscreen,
    );
    checkbox(
        tr(
            "Автоповорот по ориентации EXIF",
            "Auto-rotate by EXIF orientation",
        ),
        (20, 42, 380, 22),
        IDC_AUTO_ROTATE_EXIF,
        cfg.auto_rotate_exif,
    );

    control(
        w!("BUTTON"),
        tr("Просмотр фото", "Photos"),
        BS_GROUPBOX as u32,
        (15, 72, 395, 172),
        0,
    );
    control(
        w!("STATIC"),
        tr("Масштаб лупы (ЛКМ):", "Loupe zoom (left click):"),
        SS_LEFT,
        (28, 98, 145, 20),
        0,
    );
    let loupe_labels = ctx
        .loupe_scales
        .iter()
        .map(|s| format!("{}:1", s).replace('.', ","))
        .collect();
    let loupe_sel = ctx
        .loupe_scales
        .iter()
        .position(|s| (s - cfg.loupe_scale).abs() < 0.05)
        .unwrap_or(0);
    combo((175, 95, 90, 160), IDC_LOUPE_SCALE, loupe_labels, loupe_sel);
    control(
        w!("STATIC"),
        tr("Цвет фона:", "Background:"),
        SS_LEFT,
        (28, 133, 140, 20),
        0,
    );
    control(
        w!("BUTTON"),
        tr("Выбрать цвет...", "Choose color..."),
        tab | BS_PUSHBUTTON as u32,
        (175, 130, 130, 26),
        IDC_CHOOSE_BACKGROUND,
    );
    control(
        w!("STATIC"),
        "",
        SS_SUNKEN,
        (320, 130, 45, 26),
        IDC_BACKGROUND_PREVIEW,
    );
    checkbox(
        tr(
            "Не растягивать маленькие изображения",
            "Don't enlarge small images",
        ),
        (28, 163, 365, 22),
        IDC_NO_UPSCALE,
        cfg.no_upscale,
    );
    checkbox(
        tr("Сглаживание при увеличении", "Smooth enlarged images"),
        (28, 188, 365, 22),
        IDC_SMOOTH_ZOOM,
        cfg.smooth_zoom,
    );
    checkbox(
        tr(
            "Спрашивать перед удалением в корзину (Del)",
            "Confirm moving to Recycle Bin (Del)",
        ),
        (28, 213, 365, 22),
        IDC_CONFIRM_DELETE,
        cfg.confirm_delete,
    );

    control(
        w!("BUTTON"),
        tr("Информационная строка (OSD)", "Info line (OSD)"),
        BS_GROUPBOX as u32,
        (15, 254, 395, 170),
        0,
    );
    control(
        w!("STATIC"),
        tr("Показывать OSD:", "Show OSD:"),
        SS_LEFT,
        (28, 282, 140, 20),
        0,
    );
    let osd_labels = OsdMode::ALL.iter().map(|m| m.label().to_string()).collect();
    combo(
        (175, 279, 190, 150),
        IDC_SHOW_OSD,
        osd_labels,
        cfg.osd.index() as usize,
    );
    control(
        w!("STATIC"),
        tr("Размер шрифта:", "Font size:"),
        SS_LEFT,
        (28, 314, 140, 20),
        0,
    );
    let size_labels = ctx.font_sizes.iter().map(|s| format!("{} pt", s)).collect();
    let size_sel = ctx
        .font_sizes
        .iter()
        .position(|&s| s == cfg.osd_font_size)
        .unwrap_or(0);
    combo((175, 311, 90, 200), IDC_FONT_SIZE, size_labels, size_sel);

    control(
        w!("STATIC"),
        tr("Цвет шрифта:", "Font color:"),
        SS_LEFT,
        (28, 350, 140, 20),
        0,
    );
    control(
        w!("BUTTON"),
        tr("Выбрать цвет...", "Choose color..."),
        tab | BS_PUSHBUTTON as u32,
        (175, 347, 130, 26),
        IDC_CHOOSE_COLOR,
    );
    control(
        w!("STATIC"),
        "Aa",
        SS_CENTER | SS_CENTERIMAGE,
        (320, 347, 45, 26),
        IDC_COLOR_PREVIEW,
    );
    control(
        w!("STATIC"),
        tr("Что показывать:", "Contents:"),
        SS_LEFT,
        (28, 387, 140, 20),
        0,
    );
    control(
        w!("BUTTON"),
        tr("Для фото...", "Photos..."),
        tab | BS_PUSHBUTTON as u32,
        (175, 384, 105, 26),
        IDC_PHOTO_OSD,
    );
    control(
        w!("BUTTON"),
        tr("Для видео...", "Videos..."),
        tab | BS_PUSHBUTTON as u32,
        (290, 384, 105, 26),
        IDC_VIDEO_OSD,
    );

    control(
        w!("BUTTON"),
        tr("Полноэкранный режим", "Full screen"),
        BS_GROUPBOX as u32,
        (15, 434, 395, 140),
        0,
    );
    checkbox(
        tr("Кнопки ⏮ ⏯ ⏭ поверх фото", "⏮ ⏯ ⏭ buttons over photos"),
        (28, 457, 365, 22),
        IDC_OVERLAY_PHOTO,
        cfg.overlay_photo,
    );
    checkbox(
        tr("Панель управления поверх видео", "Control bar over videos"),
        (28, 482, 365, 22),
        IDC_OVERLAY_VIDEO,
        cfg.overlay_video,
    );
    checkbox(
        tr("Скрывать панель при бездействии", "Hide the bar when idle"),
        (28, 507, 365, 22),
        IDC_OVERLAY_AUTOHIDE,
        cfg.overlay_autohide,
    );
    control(
        w!("STATIC"),
        tr("Интервал слайд-шоу (F5):", "Slideshow interval (F5):"),
        SS_LEFT,
        (28, 540, 170, 20),
        0,
    );
    let slide_labels = ctx
        .slideshow_seconds
        .iter()
        .map(|s| format!("{} {}", s, tr("с", "s")))
        .collect();
    let slide_sel = ctx
        .slideshow_seconds
        .iter()
        .position(|&s| s == cfg.slideshow_seconds)
        .unwrap_or(0);
    combo((205, 537, 90, 200), IDC_SLIDESHOW, slide_labels, slide_sel);

    // Right column: audio / video, external editors.
    control(
        w!("BUTTON"),
        tr("Аудио и видео", "Audio and video"),
        BS_GROUPBOX as u32,
        (425, 15, 395, 165),
        0,
    );
    checkbox(
        tr(
            "Автопереход к следующему файлу",
            "Auto-advance to the next file",
        ),
        (438, 40, 365, 22),
        IDC_AUTO_ADVANCE,
        cfg.queue.auto_advance,
    );
    control(
        w!("STATIC"),
        tr("Повтор:", "Repeat:"),
        SS_LEFT,
        (438, 72, 140, 20),
        0,
    );
    let repeat_labels = Repeat::ALL.iter().map(|r| r.label().to_string()).collect();
    combo(
        (585, 69, 190, 120),
        IDC_REPEAT,
        repeat_labels,
        cfg.queue.repeat.index() as usize,
    );
    checkbox(
        tr("Случайный порядок", "Shuffle"),
        (438, 100, 365, 22),
        IDC_SHUFFLE,
        cfg.queue.shuffle,
    );
    checkbox(
        tr(
            "Продолжать видео длиннее 5 мин с места остановки",
            "Resume videos longer than 5 min where they stopped",
        ),
        (438, 125, 375, 22),
        IDC_RESUME_VIDEO,
        cfg.resume_video,
    );
    control(
        w!("STATIC"),
        tr("Кадры (Shift+S):", "Frames (Shift+S):"),
        SS_LEFT,
        (438, 153, 140, 20),
        0,
    );
    let frame_labels = FrameFormat::ALL
        .iter()
        .map(|f| f.label().to_string())
        .collect();
    let frame_sel = FrameFormat::ALL
        .iter()
        .position(|&f| f == cfg.frame_format)
        .unwrap_or(0);
    combo(
        (585, 150, 190, 80),
        IDC_FRAME_FORMAT,
        frame_labels,
        frame_sel,
    );

    control(
        w!("BUTTON"),
        tr(
            "Внешние редакторы («Открыть в редакторе»)",
            "External editors (\"Open in editor\")",
        ),
        BS_GROUPBOX as u32,
        (425, 190, 395, 125),
        0,
    );
    program_row(
        218,
        tr("Фото:", "Photo:"),
        &cfg.photo_editor,
        IDC_PHOTO_EDITOR,
        IDC_BROWSE_PHOTO_EDITOR,
    );
    program_row(
        250,
        tr("Видео:", "Video:"),
        &cfg.video_editor,
        IDC_VIDEO_EDITOR,
        IDC_BROWSE_VIDEO_EDITOR,
    );
    program_row(
        282,
        tr("Аудио:", "Audio:"),
        &cfg.audio_editor,
        IDC_AUDIO_EDITOR,
        IDC_BROWSE_AUDIO_EDITOR,
    );

    control(
        w!("BUTTON"),
        tr("Контентный плагин (WDX)", "Content plugin (WDX)"),
        BS_GROUPBOX as u32,
        (425, 325, 395, 102),
        0,
    );
    let hint = tr(
        "Поля mediares для колонок, поиска дубликатов и группового переименования в TC",
        "mediares fields for TC columns, duplicate search and multi-rename",
    );
    control(w!("STATIC"), hint, SS_LEFT, (438, 348, 370, 36), 0);
    let registered = ctx.registration.as_ref().is_some_and(|r| r.registered);
    let label = if registered {
        registered_label()
    } else {
        tr("Зарегистрировать WDX", "Register WDX")
    };
    let button = control(
        w!("BUTTON"),
        label,
        tab | BS_PUSHBUTTON as u32,
        (438, 388, 200, 26),
        IDC_REGISTER_WDX,
    );
    let _ = EnableWindow(button, !registered);

    control(
        w!("STATIC"),
        "Язык / Language:",
        SS_LEFT,
        (438, 450, 140, 20),
        0,
    );
    let lang_labels = LangSetting::ALL
        .iter()
        .map(|l| l.label().to_string())
        .collect();
    let lang_sel = LangSetting::ALL
        .iter()
        .position(|&l| l == cfg.language)
        .unwrap_or(0);
    combo((585, 447, 225, 120), IDC_LANGUAGE, lang_labels, lang_sel);

    let ok = control(
        w!("BUTTON"),
        tr("ОК", "OK"),
        tab | BS_DEFPUSHBUTTON as u32,
        (615, 546, 95, 28),
        IDOK.0,
    );
    control(
        w!("BUTTON"),
        tr("Отмена", "Cancel"),
        tab | BS_PUSHBUTTON as u32,
        (725, 546, 95, 28),
        IDCANCEL.0,
    );
    ok
}

/// Opens the template editor; the result is kept in `ctx` and saved with OK.
unsafe fn edit_osd_template(dlg: HWND, ctx: &mut Context, video: bool) {
    let cfg = &mut ctx.config;
    let (title, template, default, fields) = if video {
        (
            tr("OSD для видео", "Video OSD"),
            &mut cfg.video_osd,
            osd_template::DEFAULT_VIDEO,
            osd_template::VIDEO_FIELDS,
        )
    } else {
        (
            tr("OSD для фото", "Photo OSD"),
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
    tr("WDX зарегистрирован", "WDX registered")
}

/// Adds this DLL to `[ContentPlugins]` right away (independent of OK / Cancel), then greys the button.
unsafe fn register_wdx(dlg: HWND, ctx: &mut Context) {
    let Some(registration) = ctx.registration.as_mut() else {
        show_error(
            dlg,
            tr(
                "Не найден wincmd.ini. Подключите плагин вручную: Конфигурация → Настройка → Плагины → Контентные плагины → Добавить, выбрать mediares.wlx64.",
                "wincmd.ini not found. Add the plugin manually: Configuration → Options → Plugins → Content plugins → Add, select mediares.wlx64.",
            ),
        );
        return;
    };
    if let Err(message) = registration.register() {
        show_error(dlg, &message);
        return;
    }
    MessageBoxW(
        Some(dlg),
        &HSTRING::from(tr(
            "WDX зарегистрирован, перезапустите Total Commander.",
            "WDX registered, restart Total Commander.",
        )),
        w!("Mediares"),
        MB_OK | MB_ICONINFORMATION,
    );
    if let Ok(button) = GetDlgItem(Some(dlg), IDC_REGISTER_WDX) {
        let _ = SetDlgItemTextW(dlg, IDC_REGISTER_WDX, &HSTRING::from(registered_label()));
        let _ = EnableWindow(button, false);
    }
    // The disabled button can't keep the keyboard focus.
    if let Ok(ok) = GetDlgItem(Some(dlg), IDOK.0) {
        let _ = SetFocus(Some(ok));
    }
}

unsafe fn is_checked(dlg: HWND, id: i32) -> bool {
    SendDlgItemMessageW(dlg, id, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize == BST_CHECKED
}

unsafe fn selected<T: Copy>(dlg: HWND, id: i32, options: &[T]) -> Option<T> {
    let idx = SendDlgItemMessageW(dlg, id, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
    usize::try_from(idx)
        .ok()
        .and_then(|i| options.get(i).copied())
}

unsafe fn accept(dlg: HWND, ctx: &mut Context) {
    let cfg = &mut ctx.config;
    cfg.language = selected(dlg, IDC_LANGUAGE, &LangSetting::ALL).unwrap_or(cfg.language);
    cfg.start_fullscreen = is_checked(dlg, IDC_START_FULLSCREEN);
    cfg.auto_rotate_exif = is_checked(dlg, IDC_AUTO_ROTATE_EXIF);
    cfg.osd = selected(dlg, IDC_SHOW_OSD, &OsdMode::ALL).unwrap_or(cfg.osd);
    cfg.loupe_scale = selected(dlg, IDC_LOUPE_SCALE, &ctx.loupe_scales).unwrap_or(cfg.loupe_scale);
    cfg.osd_font_size = selected(dlg, IDC_FONT_SIZE, &ctx.font_sizes).unwrap_or(cfg.osd_font_size);
    cfg.osd_font_color = ctx.color;
    cfg.queue.auto_advance = is_checked(dlg, IDC_AUTO_ADVANCE);
    cfg.queue.repeat = selected(dlg, IDC_REPEAT, &Repeat::ALL).unwrap_or(cfg.queue.repeat);
    cfg.queue.shuffle = is_checked(dlg, IDC_SHUFFLE);
    cfg.overlay_photo = is_checked(dlg, IDC_OVERLAY_PHOTO);
    cfg.overlay_video = is_checked(dlg, IDC_OVERLAY_VIDEO);
    cfg.overlay_autohide = is_checked(dlg, IDC_OVERLAY_AUTOHIDE);
    cfg.slideshow_seconds =
        selected(dlg, IDC_SLIDESHOW, &ctx.slideshow_seconds).unwrap_or(cfg.slideshow_seconds);
    cfg.photo_background = ctx.background;
    cfg.no_upscale = is_checked(dlg, IDC_NO_UPSCALE);
    cfg.smooth_zoom = is_checked(dlg, IDC_SMOOTH_ZOOM);
    cfg.confirm_delete = is_checked(dlg, IDC_CONFIRM_DELETE);
    cfg.resume_video = is_checked(dlg, IDC_RESUME_VIDEO);
    cfg.frame_format =
        selected(dlg, IDC_FRAME_FORMAT, &FrameFormat::ALL).unwrap_or(cfg.frame_format);
    cfg.photo_editor = edit_text(dlg, IDC_PHOTO_EDITOR);
    cfg.video_editor = edit_text(dlg, IDC_VIDEO_EDITOR);
    cfg.audio_editor = edit_text(dlg, IDC_AUDIO_EDITOR);
    cfg.save();
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

unsafe fn repaint_previews(dlg: HWND) {
    for id in [IDC_COLOR_PREVIEW, IDC_BACKGROUND_PREVIEW] {
        if let Ok(preview) = GetDlgItem(Some(dlg), id) {
            let _ = InvalidateRect(Some(preview), None, true);
        }
    }
}

unsafe fn edit_text(dlg: HWND, id: i32) -> String {
    let mut buf = [0u16; 1024];
    let len = GetDlgItemTextW(dlg, id, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..len]).trim().to_string()
}

/// "Обзор...": picks a program and puts its path into the edit box `edit_id`.
unsafe fn browse_program(dlg: HWND, edit_id: i32) {
    // The dialog opens at the current program (surrounding quotes dropped).
    let mut file = [0u16; 1024];
    let current: Vec<u16> = edit_text(dlg, edit_id)
        .trim_matches('"')
        .encode_utf16()
        .take(file.len() - 1)
        .collect();
    file[..current.len()].copy_from_slice(&current);
    let filter = HSTRING::from(tr(
        "Программы (*.exe)\0*.exe\0Все файлы (*.*)\0*.*\0",
        "Programs (*.exe)\0*.exe\0All files (*.*)\0*.*\0",
    ));
    let title = HSTRING::from(tr("Выберите редактор", "Choose an editor"));
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
        let _ = SetDlgItemTextW(dlg, edit_id, PCWSTR(file.as_ptr()));
    }
}

unsafe fn choose_color(dlg: HWND, ctx: &mut Context) {
    if let Some(color) = pick_color(dlg, ctx.color) {
        ctx.color = color;
        repaint_previews(dlg);
    }
}

unsafe fn choose_background(dlg: HWND, ctx: &mut Context) {
    if let Some(color) = pick_color(dlg, ctx.background) {
        ctx.background = color;
        ctx.background_brush = gdi::solid_brush(color);
        repaint_previews(dlg);
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
            WM_COMMAND => {
                match dialog::loword(wparam) as i32 {
                    IDC_CHOOSE_COLOR => choose_color(hwnd, ctx),
                    IDC_CHOOSE_BACKGROUND => choose_background(hwnd, ctx),
                    IDC_BROWSE_PHOTO_EDITOR => browse_program(hwnd, IDC_PHOTO_EDITOR),
                    IDC_BROWSE_VIDEO_EDITOR => browse_program(hwnd, IDC_VIDEO_EDITOR),
                    IDC_BROWSE_AUDIO_EDITOR => browse_program(hwnd, IDC_AUDIO_EDITOR),
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
}
