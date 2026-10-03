//! Owner-drawn popup menu in TC's light or dark theme.
//!
//! Native menus take their colors from uxtheme's process-wide state, which libmpv changes when it
//! creates its window, so the same menu came out light or dark depending on what ran before.
//! Drawing the items ourselves (`MFT_OWNERDRAW`) makes the look depend only on the theme TC
//! reports (`LCP_DARKMODE`). Items still carry their text, so screen readers can read them.
//! The owner window forwards `WM_MEASUREITEM` / `WM_DRAWITEM` to [`measure_item`] / [`draw_item`].

use windows::core::PWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    GetDC, GetTextExtentPoint32W, ReleaseDC, SelectObject, DT_CENTER, DT_LEFT, DT_NOPREFIX,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
};
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_CHECKED, ODS_SELECTED, ODT_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetMenuStringW, InsertMenuItemW, SetMenuInfo, TrackPopupMenu,
    HMENU, MENUINFO, MENUITEMINFOW, MENU_ITEM_STATE, MFS_CHECKED, MFT_OWNERDRAW, MFT_SEPARATOR,
    MF_BYCOMMAND, MIIM_DATA, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIM_BACKGROUND,
    TRACK_POPUP_MENU_FLAGS,
};

use crate::gdi::{self, Brush, Font};

struct Palette {
    background: u32,
    text: u32,
    highlight: u32,
    separator: u32,
}

// Close to the Windows 11 menus. Grays only, so the COLORREF byte order doesn't matter.
const LIGHT: Palette = Palette {
    background: 0xF9F9F9,
    text: 0x000000,
    highlight: 0xE5E5E5,
    separator: 0xD7D7D7,
};
const DARK: Palette = Palette {
    background: 0x2B2B2B,
    text: 0xFFFFFF,
    highlight: 0x414141,
    separator: 0x4D4D4D,
};

/// Room for the check mark, the right margin, the gap before a shortcut (at 96 DPI).
const CHECK_WIDTH: f32 = 28.0;
const MARGIN_RIGHT: f32 = 20.0;
const SHORTCUT_GAP: f32 = 32.0;

/// Shared by all items: every item's `itemData` points here.
struct Style {
    menu: HMENU,
    font: Font,
    background: Brush,
    palette: &'static Palette,
    scale: f32,
}

impl Style {
    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }

    /// Label and shortcut (after `\t`) of a command item.
    unsafe fn text(&self, id: u32) -> String {
        let mut buf = [0u16; 256];
        let len = GetMenuStringW(self.menu, id, Some(&mut buf), MF_BYCOMMAND) as usize;
        String::from_utf16_lossy(&buf[..len])
    }
}

/// Popup menu destroyed on drop. Command ids must be nonzero: id 0 marks a separator.
pub struct ThemedMenu(Box<Style>);

impl ThemedMenu {
    pub unsafe fn new(owner: HWND, dark: bool) -> Option<Self> {
        let menu = CreatePopupMenu().ok()?;
        let palette = if dark { &DARK } else { &LIGHT };
        let scale = gdi::dpi_scale(owner);
        let style = Box::new(Style {
            menu,
            font: gdi::create_font("Segoe UI", -(12.0 * scale).round() as i32, false),
            background: gdi::solid_brush(palette.background),
            palette,
            scale,
        });
        // Paints the margins around the items.
        let info = MENUINFO {
            cbSize: size_of::<MENUINFO>() as u32,
            fMask: MIM_BACKGROUND,
            hbrBack: style.background.0,
            ..Default::default()
        };
        let _ = SetMenuInfo(menu, &info);
        Some(Self(style))
    }

    /// `label` may carry a shortcut after `\t`, drawn right-aligned.
    pub unsafe fn item(&mut self, id: u32, label: &str, checked: bool) {
        let mut text: Vec<u16> = label.encode_utf16().chain([0]).collect();
        self.insert(MENUITEMINFOW {
            fMask: MIIM_FTYPE | MIIM_ID | MIIM_STATE | MIIM_STRING | MIIM_DATA,
            wID: id,
            fState: if checked {
                MFS_CHECKED
            } else {
                MENU_ITEM_STATE(0)
            },
            dwTypeData: PWSTR(text.as_mut_ptr()),
            ..Default::default()
        });
    }

    pub unsafe fn separator(&mut self) {
        self.insert(MENUITEMINFOW {
            fMask: MIIM_FTYPE | MIIM_DATA,
            fType: MFT_SEPARATOR,
            ..Default::default()
        });
    }

    unsafe fn insert(&mut self, mut info: MENUITEMINFOW) {
        info.cbSize = size_of::<MENUITEMINFOW>() as u32;
        info.fType |= MFT_OWNERDRAW;
        info.dwItemData = &*self.0 as *const Style as usize;
        let _ = InsertMenuItemW(self.0.menu, u32::MAX, true, &info);
    }

    /// Shows the menu (modal); returns the chosen id, 0 if none.
    pub unsafe fn track(&self, owner: HWND, x: i32, y: i32, flags: TRACK_POPUP_MENU_FLAGS) -> u32 {
        TrackPopupMenu(self.0.menu, flags, x, y, Some(0), owner, None).0 as u32
    }
}

impl Drop for ThemedMenu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0.menu);
        }
    }
}

/// `WM_MEASUREITEM` handler; false if the message isn't about a [`ThemedMenu`] item.
pub unsafe fn measure_item(lparam: LPARAM) -> bool {
    let mis = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
    if mis.CtlType != ODT_MENU || mis.itemData == 0 {
        return false;
    }
    let style = &*(mis.itemData as *const Style);
    if mis.itemID == 0 {
        mis.itemHeight = style.px(7.0) as u32;
        return true;
    }
    let text = style.text(mis.itemID);
    let (label, shortcut) = text.split_once('\t').unwrap_or((&text, ""));
    let dc = GetDC(None);
    let old = SelectObject(dc, style.font.0.into());
    let extent = |s: &str| {
        let wide: Vec<u16> = s.encode_utf16().collect();
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &wide, &mut size);
        size
    };
    let size = extent(label);
    let mut width = style.px(CHECK_WIDTH + MARGIN_RIGHT) + size.cx;
    if !shortcut.is_empty() {
        width += style.px(SHORTCUT_GAP) + extent(shortcut).cx;
    }
    SelectObject(dc, old);
    ReleaseDC(None, dc);
    mis.itemWidth = width as u32;
    mis.itemHeight = (size.cy + style.px(12.0)) as u32;
    true
}

/// `WM_DRAWITEM` handler; false if the message isn't about a [`ThemedMenu`] item.
pub unsafe fn draw_item(lparam: LPARAM) -> bool {
    let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
    if dis.CtlType != ODT_MENU || dis.itemData == 0 {
        return false;
    }
    let style = &*(dis.itemData as *const Style);
    let (p, dc, rc) = (style.palette, dis.hDC, dis.rcItem);
    gdi::fill(dc, rc, p.background);

    if dis.itemID == 0 {
        let top = (rc.top + rc.bottom) / 2;
        let line = RECT {
            left: rc.left + style.px(4.0),
            top,
            right: rc.right - style.px(4.0),
            bottom: top + style.px(1.0).max(1),
        };
        gdi::fill(dc, line, p.separator);
        return true;
    }

    if dis.itemState.0 & ODS_SELECTED.0 != 0 {
        let highlight = RECT {
            left: rc.left + style.px(4.0),
            top: rc.top + style.px(1.0),
            right: rc.right - style.px(4.0),
            bottom: rc.bottom - style.px(1.0),
        };
        gdi::fill(dc, highlight, p.highlight);
    }
    let font = Some(style.font.0);
    let flags = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
    let check_right = rc.left + style.px(CHECK_WIDTH);
    if dis.itemState.0 & ODS_CHECKED.0 != 0 {
        let check = RECT {
            right: check_right,
            ..rc
        };
        gdi::text(dc, check, "\u{2713}", font, p.text, flags | DT_CENTER);
    }
    let body = RECT {
        left: check_right,
        right: rc.right - style.px(MARGIN_RIGHT),
        ..rc
    };
    let text = style.text(dis.itemID);
    let (label, shortcut) = text.split_once('\t').unwrap_or((&text, ""));
    gdi::text(dc, body, label, font, p.text, flags | DT_LEFT);
    gdi::text(dc, body, shortcut, font, p.text, flags | DT_RIGHT);
    true
}
