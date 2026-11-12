//! Tab pages and controls for viewer settings.

use super::*;

/// Creates the tab strip, its pages with their controls and OK / Cancel; returns OK and the pages
/// (all hidden but the open one).
pub(super) unsafe fn build_controls(
    dlg: HWND,
    ctx: &Context,
    font: windows::Win32::Graphics::Gdi::HFONT,
) -> (HWND, Pages) {
    let cfg = &ctx.config;
    let tab = WS_TABSTOP.0;

    let tabs = dialog::control(
        dlg,
        w!("SysTabControl32"),
        "",
        tab | WS_CLIPSIBLINGS.0,
        (10, 10, 466, 372),
        IDC_TABS as usize,
        font,
    );
    for (i, title) in PAGE_TITLES.iter().enumerate() {
        let mut text: Vec<u16> = tr(title).encode_utf16().chain(Some(0)).collect();
        let item = TCITEMW {
            mask: TCIF_TEXT,
            pszText: PWSTR(text.as_mut_ptr()),
            ..Default::default()
        };
        SendMessageW(
            tabs,
            TCM_INSERTITEMW,
            Some(WPARAM(i)),
            Some(LPARAM(&item as *const _ as isize)),
        );
    }
    let (pages, origin) = create_pages(dlg, tabs);

    // Page controls are laid out in dialog coordinates; `origin` is where the pages start.
    let page = Cell::new(PAGE_COUNT);
    let control =
        |class, text: &str, style: u32, (x, y, w, h): (i32, i32, i32, i32), id: i32| match pages
            .get(page.get())
        {
            Some(&parent) => {
                let rect = (x - origin.0, y - origin.1, w, h);
                dialog::control(parent, class, text, style, rect, id as usize, font)
            }
            None => dialog::control(dlg, class, text, style, (x, y, w, h), id as usize, font),
        };
    let checkbox = |x: i32, y: i32, width: i32, text: &str, id: i32, checked: bool| {
        let hwnd = control(
            w!("BUTTON"),
            text,
            tab | BS_AUTOCHECKBOX as u32,
            (x, y, width, ROW_HEIGHT),
            id,
        );
        if checked {
            SendMessageW(hwnd, BM_SETCHECK, Some(WPARAM(BST_CHECKED)), None);
        }
    };
    let combo = |(x, y, w): (i32, i32, i32), id: i32, items: Vec<String>, selected: usize| {
        let hwnd = control(
            w!("COMBOBOX"),
            "",
            tab | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
            (x, y, w, COMBO_HEIGHT),
            id,
        );
        for item in items {
            let text = HSTRING::from(item);
            SendMessageW(
                hwnd,
                CB_ADDSTRING,
                None,
                Some(LPARAM(text.as_ptr() as isize)),
            );
        }
        SendMessageW(hwnd, CB_SETCURSEL, Some(WPARAM(selected)), None);
    };
    // "<label>  [combo]" on the grid: the label shares the row's text column, the value is in
    // `FIELD`, so every combo on every page lines up.
    let field =
        |x: i32, y: i32, text: &str, id: i32, items: Vec<String>, selected: usize, w: i32| {
            control(
                w!("STATIC"),
                text,
                SS_LEFT,
                (x, y + 3, FIELD - x - 10, 20),
                0,
            );
            combo((FIELD, y, w), id, items, selected);
        };
    let button = |x: i32, y: i32, w: i32, text: &str, id: i32| {
        control(
            w!("BUTTON"),
            text,
            tab | BS_PUSHBUTTON as u32,
            (x, y, w, BUTTON_HEIGHT),
            id,
        )
    };
    let group_height = |rows: i32| GROUP_TITLE + rows * ROW + GROUP_PAD;
    // A titled box with `rows` rows inside; `row(top, n)` puts control `n` of it on the grid.
    let group = |text: &str, y: i32, rows: i32| {
        control(
            w!("BUTTON"),
            text,
            BS_GROUPBOX as u32,
            (GROUP_X, y, GROUP_W, group_height(rows)),
            0,
        );
    };
    let row = |top: i32, n: i32| top + GROUP_TITLE + n * ROW;
    let below = |top: i32, rows: i32| top + group_height(rows) + GROUP_GAP;
    // "<label> [path] [Обзор...]" in the editors group; empty = the system's choice.
    let program_row = |y: i32, label: &str, path: &str, edit_id: i32, browse_id: i32| {
        control(w!("STATIC"), label, SS_LEFT, (GROUPED, y + 3, 60, 20), 0);
        let edit = control(
            w!("EDIT"),
            path,
            tab | WS_BORDER.0 | ES_AUTOHSCROLL,
            (100, y - 1, 268, BUTTON_HEIGHT - 2),
            edit_id,
        );
        button(374, y - 1, 80, tr("Browse..."), browse_id);
        let hint = HSTRING::from(tr("the program assigned in Windows"));
        SendMessageW(
            edit,
            EM_SETCUEBANNER,
            Some(WPARAM(1)),
            Some(LPARAM(hint.as_ptr() as isize)),
        );
    };

    // Photos: how they are shown, then the loupe, background and slideshow.
    page.set(0);
    let viewing = PAGE_TOP;
    group(tr("Viewing"), viewing, 7);
    for (i, (text, id, checked)) in [
        (
            tr("Auto-rotate by EXIF orientation"),
            IDC_AUTO_ROTATE_EXIF,
            cfg.auto_rotate_exif,
        ),
        (
            tr("Don't enlarge small images"),
            IDC_NO_UPSCALE,
            cfg.no_upscale,
        ),
        (
            tr("Smooth enlarged images"),
            IDC_SMOOTH_ZOOM,
            cfg.smooth_zoom,
        ),
        (
            tr("Keep the zoom on the next photo (Z)"),
            IDC_KEEP_ZOOM,
            cfg.keep_zoom,
        ),
        (
            tr("Skip RAW files that have a JPEG twin"),
            IDC_SKIP_RAW_TWINS,
            cfg.skip_raw_twins,
        ),
        (
            tr("Mouse wheel zooms the picture (Ctrl+wheel pages instead)"),
            IDC_WHEEL_ZOOM,
            cfg.wheel_zoom,
        ),
        (
            tr("Confirm moving to Recycle Bin (Del)"),
            IDC_CONFIRM_DELETE,
            cfg.confirm_delete,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        checkbox(
            GROUPED,
            row(viewing, i as i32),
            GROUPED_W,
            text,
            id,
            checked,
        );
    }

    let loupe = below(viewing, 7);
    group(tr("Background, loupe and slideshow"), loupe, 3);
    let background = row(loupe, 0);
    control(
        w!("STATIC"),
        tr("Background:"),
        SS_LEFT,
        (GROUPED, background + 3, FIELD - GROUPED - 10, 20),
        0,
    );
    button(
        FIELD,
        background - 1,
        130,
        tr("Choose color..."),
        IDC_CHOOSE_BACKGROUND,
    );
    control(
        w!("STATIC"),
        "",
        SS_SUNKEN,
        (FIELD + 145, background - 1, 45, BUTTON_HEIGHT),
        IDC_BACKGROUND_PREVIEW,
    );
    let loupe_labels = ctx
        .loupe_scales
        .iter()
        .map(|s| crate::i18n::decimal(format!("{}:1", s)))
        .collect();
    let loupe_sel = ctx
        .loupe_scales
        .iter()
        .position(|s| (s - cfg.loupe_scale).abs() < 0.05)
        .unwrap_or(0);
    field(
        GROUPED,
        row(loupe, 1),
        tr("Loupe zoom (left click):"),
        IDC_LOUPE_SCALE,
        loupe_labels,
        loupe_sel,
        90,
    );
    let slide_labels = ctx
        .slideshow_seconds
        .iter()
        .map(|s| format!("{} {}", s, tr("s")))
        .collect();
    let slide_sel = ctx
        .slideshow_seconds
        .iter()
        .position(|&s| s == cfg.slideshow_seconds)
        .unwrap_or(0);
    field(
        GROUPED,
        row(loupe, 2),
        tr("Slideshow interval (F5):"),
        IDC_SLIDESHOW,
        slide_labels,
        slide_sel,
        90,
    );

    // Video and audio: what plays, then seeking and saved frames.
    page.set(1);
    let playback = PAGE_TOP;
    group(tr("Playback"), playback, 5);
    checkbox(
        GROUPED,
        row(playback, 0),
        GROUPED_W,
        tr("Auto-advance to the next file"),
        IDC_AUTO_ADVANCE,
        cfg.queue.auto_advance,
    );
    let repeat_labels = Repeat::ALL.iter().map(|r| r.label().to_string()).collect();
    field(
        GROUPED,
        row(playback, 1),
        tr("Repeat:"),
        IDC_REPEAT,
        repeat_labels,
        cfg.queue.repeat.index() as usize,
        190,
    );
    checkbox(
        GROUPED,
        row(playback, 2),
        GROUPED_W,
        tr("Shuffle"),
        IDC_SHUFFLE,
        cfg.queue.shuffle,
    );
    // One sentence-like row: the box, the length, and "where they stopped" stay on one line.
    let resume = row(playback, 3);
    checkbox(
        GROUPED,
        resume,
        FIELD - GROUPED - 3,
        tr("Resume videos longer than"),
        IDC_RESUME_VIDEO,
        cfg.resume_video,
    );
    // Whole minutes; a value set in the INI by hand may be seconds.
    let threshold_labels = ctx
        .resume_thresholds
        .iter()
        .map(|s| match s % 60 {
            0 => format!("{} {}", s / 60, tr("min")),
            _ => format!("{} {}", s, tr("s")),
        })
        .collect();
    let threshold_sel = ctx
        .resume_thresholds
        .iter()
        .position(|&s| s == cfg.resume_threshold_sec)
        .unwrap_or(0);
    combo(
        (FIELD, resume, 65),
        IDC_RESUME_THRESHOLD,
        threshold_labels,
        threshold_sel,
    );
    control(
        w!("STATIC"),
        tr("where they stopped"),
        SS_LEFT,
        (FIELD + 75, resume, 165, ROW_HEIGHT),
        0,
    );
    checkbox(
        GROUPED,
        row(playback, 4),
        GROUPED_W,
        tr("Play audio at its ReplayGain loudness"),
        IDC_REPLAY_GAIN,
        cfg.replay_gain,
    );

    let seeking = below(playback, 5);
    group(tr("Seeking and snapshots"), seeking, 4);
    checkbox(
        GROUPED,
        row(seeking, 0),
        GROUPED_W,
        tr("Show a frame preview on the timeline"),
        IDC_SEEK_PREVIEW,
        cfg.seek_preview,
    );
    let step_labels = ctx
        .seek_steps
        .iter()
        .map(|s| format!("{} {}", s, tr("s")))
        .collect();
    let step_sel = ctx
        .seek_steps
        .iter()
        .position(|&s| s == cfg.seek_step_sec)
        .unwrap_or(0);
    field(
        GROUPED,
        row(seeking, 1),
        tr("Seek step (← →):"),
        IDC_SEEK_STEP,
        step_labels,
        step_sel,
        90,
    );
    let frame_labels = PictureFormat::ALL
        .iter()
        .map(|f| f.label().to_string())
        .collect();
    let frame_sel = PictureFormat::ALL
        .iter()
        .position(|&f| f == cfg.frame_format)
        .unwrap_or(0);
    field(
        GROUPED,
        row(seeking, 2),
        tr("Frames (Shift+S):"),
        IDC_FRAME_FORMAT,
        frame_labels,
        frame_sel,
        190,
    );
    let sheet = row(seeking, 3);
    control(
        w!("STATIC"),
        tr("Contact sheet (Ctrl+Shift+S):"),
        SS_LEFT,
        (GROUPED, sheet + 3, FIELD - GROUPED - 10, 20),
        0,
    );
    let grid_combo = |options: &[u32], current: u32, x: i32, id: i32| {
        let labels = options.iter().map(u32::to_string).collect();
        let sel = options.iter().position(|&v| v == current).unwrap_or(0);
        combo((x, sheet, 55), id, labels, sel);
    };
    grid_combo(
        &ctx.grid_columns,
        cfg.contact_sheet_columns,
        FIELD,
        IDC_CONTACT_COLUMNS,
    );
    control(
        w!("STATIC"),
        "×",
        SS_CENTER,
        (FIELD + 60, sheet + 3, 20, 20),
        0,
    );
    grid_combo(
        &ctx.grid_rows,
        cfg.contact_sheet_rows,
        FIELD + 85,
        IDC_CONTACT_ROWS,
    );

    // Display: full screen and the info line.
    page.set(2);
    let full = PAGE_TOP;
    group(tr("Full screen"), full, 4);
    for (i, (text, id, checked)) in [
        (
            tr("Start in full screen"),
            IDC_START_FULLSCREEN,
            cfg.start_fullscreen,
        ),
        (
            tr("⏮ ⏯ ⏭ buttons over photos"),
            IDC_OVERLAY_PHOTO,
            cfg.overlay_photo,
        ),
        (
            tr("Control bar over videos"),
            IDC_OVERLAY_VIDEO,
            cfg.overlay_video,
        ),
        (
            tr("Hide the bar when idle"),
            IDC_OVERLAY_AUTOHIDE,
            cfg.overlay_autohide,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        checkbox(GROUPED, row(full, i as i32), GROUPED_W, text, id, checked);
    }

    let osd = below(full, 4);
    group(tr("Info line (OSD)"), osd, 5);
    // Combo boxes end at the right edge of "Videos..." below; the font size keeps its own width.
    let osd_labels = OsdMode::ALL.iter().map(|m| m.label().to_string()).collect();
    field(
        GROUPED,
        row(osd, 0),
        tr("Show OSD:"),
        IDC_SHOW_OSD,
        osd_labels,
        cfg.osd.index() as usize,
        240,
    );
    let size_labels = ctx.font_sizes.iter().map(|s| format!("{} pt", s)).collect();
    let size_sel = ctx
        .font_sizes
        .iter()
        .position(|&s| s == cfg.osd_font_size)
        .unwrap_or(0);
    field(
        GROUPED,
        row(osd, 1),
        tr("Font size:"),
        IDC_FONT_SIZE,
        size_labels,
        size_sel,
        90,
    );
    // A wide combo: "Segoe UI Semibold" and "Times New Roman" must not be cut off.
    let family_sel = ctx
        .font_families
        .iter()
        .position(|f| f.eq_ignore_ascii_case(&cfg.osd_font_name))
        .unwrap_or(0);
    field(
        GROUPED,
        row(osd, 2),
        tr("Family:"),
        IDC_OSD_FONT_FAMILY,
        ctx.font_families.clone(),
        family_sel,
        240,
    );
    let color = row(osd, 3);
    control(
        w!("STATIC"),
        tr("Font color:"),
        SS_LEFT,
        (GROUPED, color + 3, FIELD - GROUPED - 10, 20),
        0,
    );
    button(
        FIELD,
        color - 1,
        115,
        tr("Choose color..."),
        IDC_CHOOSE_COLOR,
    );
    control(
        w!("STATIC"),
        "Aa",
        SS_CENTER | SS_CENTERIMAGE,
        (FIELD + 125, color - 1, 115, BUTTON_HEIGHT),
        IDC_COLOR_PREVIEW,
    );
    let contents = row(osd, 4);
    control(
        w!("STATIC"),
        tr("Contents:"),
        SS_LEFT,
        (GROUPED, contents + 3, FIELD - GROUPED - 10, 20),
        0,
    );
    button(FIELD, contents - 1, 115, tr("Photos..."), IDC_PHOTO_OSD);
    button(
        FIELD + 125,
        contents - 1,
        115,
        tr("Videos..."),
        IDC_VIDEO_OSD,
    );

    // General: language, editors, WDX.
    page.set(3);
    let interface = PAGE_TOP;
    group(tr("Interface"), interface, 1);
    let languages = LangSetting::all();
    let lang_labels = languages.iter().map(|l| l.label().to_string()).collect();
    let current = LangSetting::Fixed(cfg.language.resolve().code);
    let lang_sel = languages.iter().position(|&l| l == current).unwrap_or(0);
    // Same right edge as the "Browse..." buttons of the editors below.
    field(
        GROUPED,
        row(interface, 0),
        &language_caption(),
        IDC_LANGUAGE,
        lang_labels,
        lang_sel,
        244,
    );

    let editors = below(interface, 1);
    group(tr("External editors (\"Open in editor\")"), editors, 3);
    program_row(
        row(editors, 0),
        tr("Photo:"),
        &cfg.photo_editor,
        IDC_PHOTO_EDITOR,
        IDC_BROWSE_PHOTO_EDITOR,
    );
    program_row(
        row(editors, 1),
        tr("Video:"),
        &cfg.video_editor,
        IDC_VIDEO_EDITOR,
        IDC_BROWSE_VIDEO_EDITOR,
    );
    program_row(
        row(editors, 2),
        tr("Audio:"),
        &cfg.audio_editor,
        IDC_AUDIO_EDITOR,
        IDC_BROWSE_AUDIO_EDITOR,
    );

    let wdx = below(editors, 3);
    group(tr("Content plugin (WDX)"), wdx, 3);
    let hint = tr("mediares fields for TC columns, duplicate search and multi-rename");
    control(
        w!("STATIC"),
        hint,
        SS_LEFT,
        (GROUPED, row(wdx, 0) + 3, GROUPED_W, 36),
        0,
    );
    let registered = ctx.registration.as_ref().is_some_and(|r| r.registered);
    let wdx_label = if registered {
        registered_label()
    } else {
        tr("Register WDX")
    };
    let register = button(GROUPED, row(wdx, 2) - 1, 200, wdx_label, IDC_REGISTER_WDX);
    let _ = EnableWindow(register, !registered);

    page.set(PAGE_COUNT);
    let ok = control(
        w!("BUTTON"),
        tr("OK"),
        tab | BS_DEFPUSHBUTTON as u32,
        (276, 392, 95, 28),
        IDOK.0,
    );
    control(
        w!("BUTTON"),
        tr("Cancel"),
        tab | BS_PUSHBUTTON as u32,
        (381, 392, 95, 28),
        IDCANCEL.0,
    );

    // Under the pages, so its pane never paints over them; it then comes last in the Tab order
    // too, between Cancel and the first control of the page, as in property sheets.
    let _ = SetWindowPos(
        tabs,
        Some(HWND_BOTTOM),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    (ok, pages)
}
