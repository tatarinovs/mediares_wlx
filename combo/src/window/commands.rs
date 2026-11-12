//! Viewer actions, menus and keyboard mappings.

use crate::i18n::n;
use crate::playlist::Repeat;

/// The commands, numbered from 1 in this order (menu ids, posted messages); `ALL` lists them.
macro_rules! commands {
    ($first:ident, $($rest:ident),* $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub(super) enum Command {
            $first = 1,
            $($rest),*
        }

        impl Command {
            pub(super) const ALL: &[Command] = &[Command::$first, $(Command::$rest),*];
        }
    };
}

commands! {
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
    SaveAs,
    SaveRotation,
    OpenMap,
    Zoom200,
    Zoom300,
    ToggleKeepZoom,
    ToggleCompare,
    AbLoop,
    NextAudioTrack,
    SaveContactSheet,
    ShowLyrics,
    PanLeft,
    PanRight,
    PanUp,
    PanDown,
    NextPage,
    PreviousPage,
    Close,
}

impl Command {
    /// Menu items with their (English) labels; shown through [`tr`].
    pub(super) const MENU: &[Option<(Command, &'static str)>] = &[
        Some((Command::TogglePlay, n("Play / pause\tSpace"))),
        Some((Command::ToggleMute, n("Mute\tM"))),
        Some((Command::ToggleFullscreen, n("Full screen\tEnter / F"))),
        Some((Command::Copy, n("Copy image\tCtrl+C"))),
        Some((Command::SaveAs, n("Save as...\tCtrl+S"))),
        Some((
            Command::SaveFrame,
            n("Save frame next to the video\tShift+S"),
        )),
        Some((
            Command::SaveContactSheet,
            n("Save contact sheet next to the video\tCtrl+Shift+S"),
        )),
        Some((Command::ToggleOsd, n("Show OSD\tO"))),
        Some((Command::ToggleSlideshow, n("Slideshow\tF5"))),
        Some((Command::RotateLeft, n("Rotate left\tL"))),
        Some((Command::RotateRight, n("Rotate right\tR"))),
        Some((
            Command::SaveRotation,
            n("Save rotation to file (JPEG, lossless)\tCtrl+R"),
        )),
        Some((
            Command::ToggleKeepZoom,
            n("Keep the zoom on the next photo\tZ"),
        )),
        Some((Command::ToggleCompare, n("Compare with this photo\tC"))),
        None,
        Some((Command::Slower, n("Slower\t["))),
        Some((Command::Faster, n("Faster\t]"))),
        Some((Command::NormalSpeed, n("Normal speed\t\\"))),
        Some((Command::FrameBack, n("Previous frame\t,"))),
        Some((Command::FrameForward, n("Next frame\t."))),
        Some((Command::AbLoop, n("A-B loop\tA"))),
        Some((Command::NextAudioTrack, n("Next audio track\tB"))),
        None,
        Some((
            Command::ToggleAutoAdvance,
            n("Auto-advance to the next file"),
        )),
        Some((Command::RepeatOff, n("No repeat"))),
        Some((Command::RepeatAll, n("Repeat list"))),
        Some((Command::RepeatOne, n("Repeat file"))),
        Some((Command::ToggleShuffle, n("Shuffle"))),
        None,
        Some((Command::ShowExif, n("EXIF info...\tE"))),
        Some((Command::ShowLyrics, n("Lyrics\tY"))),
        Some((Command::OpenMap, n("Open on map\tG"))),
        Some((Command::OpenInEditor, n("Open in editor\tF4"))),
        Some((Command::ShowInFolder, n("Show in folder\tCtrl+Enter"))),
        Some((Command::SetWallpaper, n("Set as desktop background"))),
        Some((Command::Print, n("Print...\tCtrl+P"))),
        Some((Command::Delete, n("Move to Recycle Bin\tDel"))),
        None,
        Some((Command::ShowSettings, n("Settings...\tS"))),
        None,
        Some((Command::NextPage, n("Next page\tRight / Ctrl+PgDn"))),
        Some((Command::PreviousPage, n("Previous page\tLeft / Ctrl+PgUp"))),
        Some((Command::Next, n("Next file\tSpace / Down"))),
        Some((Command::Previous, n("Previous file\tBackspace / Up"))),
    ];

    /// Whether the item applies to the current content (photo, or audio/video).
    pub(super) fn available(self, media: bool, video: bool) -> bool {
        use Command::*;
        match self {
            SaveFrame | Slower | Faster | NormalSpeed | FrameBack | FrameForward
            | NextAudioTrack | SaveContactSheet => video,
            TogglePlay | ToggleMute | ToggleAutoAdvance | RepeatOff | RepeatAll | RepeatOne
            | ToggleShuffle | AbLoop => media,
            ShowLyrics => media && !video,
            ShowExif | ToggleSlideshow | RotateLeft | RotateRight | SaveRotation | SetWallpaper
            | SaveAs | OpenMap | ToggleKeepZoom | ToggleCompare | NextPage | PreviousPage => !media,
            ToggleOsd | Print => !media || video,
            _ => true,
        }
    }

    /// For audio/video, where Space plays / pauses and Left / Right seek (see [`Self::from_key`]).
    pub(super) fn media_label(self, media: bool) -> Option<&'static str> {
        match self {
            Command::Next if media => Some(n("Next file\tN / PgDn")),
            Command::Previous if media => Some(n("Previous file\tBackspace / PgUp")),
            _ => None,
        }
    }

    pub(super) fn repeat_mode(self) -> Option<Repeat> {
        match self {
            Command::RepeatOff => Some(Repeat::Off),
            Command::RepeatAll => Some(Repeat::All),
            Command::RepeatOne => Some(Repeat::One),
            _ => None,
        }
    }

    /// Any command by its numeric value (posted messages, menu ids).
    pub(super) fn from_raw(value: usize) -> Option<Command> {
        Self::ALL.get(value.checked_sub(1)?).copied()
    }

    /// Previous / next file in the play queue.
    pub(super) fn track(forward: bool) -> Command {
        if forward {
            Command::NextTrack
        } else {
            Command::PreviousTrack
        }
    }

    /// Hotkeys. With Ctrl held only zoom keys are ours; everything else goes to the Lister
    /// (Ctrl+P print, Ctrl+C copy, ...). For audio/video, player keys take precedence; for a
    /// zoomed-in photo (`zoomed`, never set together with `media`), the arrows pan it instead of
    /// paging to another file.
    pub(super) fn from_key(
        vk: u16,
        ctrl: bool,
        shift: bool,
        media: bool,
        zoomed: bool,
    ) -> Option<Command> {
        use Command::*;
        if ctrl && !shift && vk == 0x43 {
            return Some(Copy); // Ctrl+C: the picture, not the file (TC copies files in its panels)
        }
        if !ctrl && !shift && vk == 0x73 {
            return Some(OpenInEditor); // F4, like Edit in TC
        }
        if ctrl && !shift && vk == 0x0D {
            return Some(ShowInFolder); // Ctrl+Enter
        }
        if !media && ctrl && !shift && vk == 0x53 {
            return Some(SaveAs); // Ctrl+S
        }
        if !media && ctrl && !shift && vk == 0x52 {
            return Some(SaveRotation); // Ctrl+R
        }
        if media && shift && !ctrl && vk == 0x53 {
            return Some(SaveFrame); // Shift+S, as in VLC
        }
        if media && ctrl && shift && vk == 0x53 {
            return Some(SaveContactSheet); // Ctrl+Shift+S
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
                0x25 => Some(SeekBack),          // Left: -step (5 s by default)
                0x27 => Some(SeekForward),       // Right: +step
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
                0x41 => Some(AbLoop),            // A
                0x59 => Some(ShowLyrics),        // Y
                0x42 => Some(NextAudioTrack),    // B
                _ => None,
            };
            if player.is_some() {
                return player;
            }
        }
        if zoomed && !ctrl {
            let pan = match vk {
                0x25 => Some(PanLeft),  // Left: reveal more of the left edge
                0x27 => Some(PanRight), // Right
                0x26 => Some(PanUp),    // Up
                0x28 => Some(PanDown),  // Down
                _ => None,
            };
            if pan.is_some() {
                return pan;
            }
        }
        let zoom = match vk {
            0xBB | 0x6B => Some(ZoomIn),  // '+' / numpad +
            0xBD | 0x6D => Some(ZoomOut), // '-' / numpad -
            _ => None,
        };
        if ctrl {
            // Only with Ctrl: bare digits switch the Lister's view mode.
            return zoom.or(match vk {
                0x30 | 0x60 => Some(ZoomFit),        // Ctrl+0
                0x31 | 0x61 => Some(ZoomActualSize), // Ctrl+1
                0x32 | 0x62 => Some(Zoom200),        // Ctrl+2
                0x33 | 0x63 => Some(Zoom300),        // Ctrl+3
                // Pages also while zoomed in, when the arrows pan.
                0x22 if !media => Some(NextPage),     // Ctrl+PgDn
                0x21 if !media => Some(PreviousPage), // Ctrl+PgUp
                _ => None,
            });
        }
        zoom.or(match vk {
            0x0D | 0x46 | 0x7A => Some(ToggleFullscreen), // Enter, F, F11
            0x4F | 0x49 => Some(ToggleOsd),               // O, I
            0x74 => Some(ToggleSlideshow),                // F5
            0x45 => Some(ShowExif),                       // E
            0x47 => Some(OpenMap),                        // G
            0x53 => Some(ShowSettings),                   // S
            0x4C => Some(RotateLeft),                     // L
            0x52 => Some(RotateRight),                    // R
            0x5A => Some(ToggleKeepZoom),                 // Z
            0x43 => Some(ToggleCompare),                  // C
            0x2E => Some(Delete),                         // Del
            0x6A | 0x6F => Some(ZoomFit),                 // numpad * and /
            // Pages of a multi-page file; in other files they page through the files as well.
            0x27 => Some(NextPage),                      // Right
            0x25 => Some(PreviousPage),                  // Left
            0x4E | 0x20 | 0x22 | 0x28 => Some(Next),     // N, Space, PgDn, Down
            0x50 | 0x08 | 0x21 | 0x26 => Some(Previous), // P, Backspace, PgUp, Up
            _ => None,
        })
    }
}
