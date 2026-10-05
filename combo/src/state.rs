//! Per-HWND viewer state for a Total Commander Lister instance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mediares_core::image_decode::header_looks_decodable;
use mediares_core::probe::{probe_file, MediaType};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};

use crate::config::ViewerConfig;
use crate::gdi::Font;
use crate::i18n;
use crate::image_cache::{self, DecodeOptions, DecodedImage, Request, Ticket};
use crate::media_view::MediaView;
use crate::overlay::Fullscreen;
use crate::playlist::{self, EndAction};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZoomMode {
    Fit,
    Custom(f32),
}

/// Pan in progress: cursor position and image offset at the moment the button was pressed.
#[derive(Clone, Copy)]
pub struct Drag {
    pub start: (i32, i32),
    pub start_offset: (f32, f32),
}

/// Loupe (magnifier while the left button is held): the view to restore on release.
#[derive(Clone, Copy)]
pub struct Loupe {
    pub saved_zoom: ZoomMode,
    pub saved_offset: (f32, f32),
}

pub struct ViewerState {
    pub hwnd: HWND,
    /// The Lister window that hosts the viewer.
    pub lister: HWND,
    pub file_path: PathBuf,
    pub file_size: u64,
    /// The photo on screen, usually fitted to the screen (see [`DecodedImage::full`]).
    pub image: Option<Arc<DecodedImage>>,
    /// The photo being decoded in the background (then `image` is `None`).
    pub pending: Option<Ticket>,
    /// The whole picture of `image` when that is a fitted copy, turned like it; drawn when
    /// zoomed in beyond the copy's own pixels.
    pub whole: Option<Arc<DecodedImage>>,
    /// The whole picture being decoded in the background.
    whole_pending: Option<Ticket>,
    /// The last photo shown, drawn while `pending` so switching doesn't flash an empty window.
    pub previous: Option<Arc<DecodedImage>>,
    /// The current photo could not be decoded.
    pub load_failed: bool,
    /// Quarter turns clockwise made with R / L (the picture on screen differs from the file).
    pub quarter_turns: u8,
    /// Present while a video or audio file is shown (then `image` is `None`).
    pub media: Option<MediaView>,
    pub zoom: ZoomMode,
    /// Top-left corner of the image in client coordinates (Custom zoom only).
    pub offset: (f32, f32),
    pub drag: Option<Drag>,
    pub loupe: Option<Loupe>,
    /// The files navigated through: the folder's viewable files, or an M3U playlist's entries.
    pub dir_files: Vec<PathBuf>,
    pub current_idx: usize,
    /// Direction of the last move through the list (prefetching looks further that way).
    forward: bool,
    /// The M3U file `dir_files` came from.
    pub playlist: Option<PathBuf>,
    /// Monitor rectangle while fullscreen; the window is pinned to it.
    pub fullscreen: Option<RECT>,
    /// Fullscreen extras (floating panel, idle cursor); present while fullscreen.
    pub overlay: Option<Fullscreen>,
    /// Photos advance on a timer.
    pub slideshow: bool,
    pub config: ViewerConfig,
    /// Last TC show flags (`LCP_*`), to detect which option a `LC_NEWPARAMS` toggled.
    pub show_flags: i32,
    /// Lazily created OSD font; dropped whenever the config changes.
    pub osd_font: Option<Font>,
    /// Keeps the image cache while this window lives.
    _cache_hold: image_cache::ViewerHold,
}

impl ViewerState {
    /// Loads `path` into the viewer window `hwnd`; `None` if it cannot be displayed.
    pub fn new(hwnd: HWND, lister: HWND, path: &Path, config: ViewerConfig) -> Option<Self> {
        i18n::set(config.language.resolve());
        let mut state = Self {
            hwnd,
            lister,
            file_path: PathBuf::new(),
            file_size: 0,
            image: None,
            pending: None,
            whole: None,
            whole_pending: None,
            previous: None,
            load_failed: false,
            quarter_turns: 0,
            media: None,
            zoom: ZoomMode::Fit,
            offset: (0.0, 0.0),
            drag: None,
            loupe: None,
            dir_files: Vec::new(),
            current_idx: 0,
            forward: true,
            playlist: None,
            fullscreen: None,
            overlay: None,
            slideshow: false,
            config,
            show_flags: 0,
            osd_font: None,
            _cache_hold: image_cache::ViewerHold::new(),
        };
        state.set_file(path).then_some(state)
    }

    /// Switches to `path`, resetting the view. Returns whether it could be displayed. An M3U
    /// playlist replaces the file list with its entries and shows the first one.
    pub fn set_file(&mut self, path: &Path) -> bool {
        if probe_file(path) == MediaType::Playlist {
            let entries: Vec<PathBuf> = playlist::read_m3u(path)
                .into_iter()
                .filter(|p| is_viewable(probe_file(p)))
                .collect();
            let Some(first) = entries.first().cloned() else {
                return false;
            };
            self.dir_files = entries;
            self.current_idx = 0;
            self.playlist = Some(path.to_path_buf());
            return self.show(&first);
        }
        match self.dir_files.iter().position(|p| same_path(p, path)) {
            Some(pos) => self.current_idx = pos,
            None => {
                (self.dir_files, self.current_idx) = scan_directory_media(path);
                self.playlist = None;
            }
        }
        self.show(path)
    }

    fn show(&mut self, path: &Path) -> bool {
        self.file_path = path.to_path_buf();
        self.zoom = ZoomMode::Fit;
        self.offset = (0.0, 0.0);
        self.drag = None;
        self.loupe = None;
        self.quarter_turns = 0;
        self.load_media();
        self.has_content()
    }

    pub fn has_content(&self) -> bool {
        self.shows_photo() || self.media.is_some()
    }

    /// A photo is shown or on its way.
    pub fn shows_photo(&self) -> bool {
        self.image.is_some() || self.pending.is_some()
    }

    /// Where the photo shown was taken, if its EXIF says.
    pub fn photo_gps(&self) -> Option<(f64, f64)> {
        self.image.as_ref()?.exif.as_ref()?.gps()
    }

    /// Takes the background decode results that have arrived.
    pub fn image_ready(&mut self) -> Arrived {
        let mut arrived = Arrived::Nothing;
        if let Some(result) = self.whole_pending.as_ref().and_then(Ticket::result) {
            self.whole_pending = None;
            if let Some(img) = result {
                self.whole = Some(turned(img, self.quarter_turns));
                arrived = Arrived::Whole;
            }
        }
        if let Some(result) = self.pending.as_ref().and_then(Ticket::result) {
            self.pending = None;
            self.previous = None;
            match result {
                Some(img) => self.set_image(img),
                None => self.load_failed = true,
            }
            arrived = Arrived::Photo;
        }
        arrived
    }

    /// Shows `img` (just decoded, so not turned yet) and, when it is a fitted copy, has the
    /// whole picture decoded in the background.
    fn set_image(&mut self, img: Arc<DecodedImage>) {
        self.image = Some(img);
        self.request_whole();
    }

    /// Queues the whole picture of a fitted photo on screen. Called after the neighbours are
    /// queued: it runs once they are done.
    fn request_whole(&mut self) {
        let fitted = self.image.as_ref().is_some_and(|img| !img.is_whole());
        if fitted && self.whole.is_none() && self.whole_pending.is_none() {
            let kind = probe_file(&self.file_path);
            self.whole_pending =
                image_cache::request_whole(&self.file_path, kind, self.decode_options(), self.hwnd);
        }
    }

    /// The whole picture of the photo on screen, as turned on screen; decoded right here when
    /// the background decode is not done yet (printing, copying).
    pub fn whole_image(&mut self) -> Option<Arc<DecodedImage>> {
        let img = self.image.clone()?;
        if img.is_whole() {
            return Some(img);
        }
        if self.whole.is_none() {
            let whole = match self.whole_pending.take() {
                Some(ticket) => ticket.finish()?,
                None => {
                    let kind = probe_file(&self.file_path);
                    let options = self.decode_options();
                    Arc::new(image_cache::decode_whole(&self.file_path, kind, options)?)
                }
            };
            self.whole = Some(turned(whole, self.quarter_turns));
        }
        self.whole.clone()
    }

    /// Shows the file at `idx` of the list; false if it can't be displayed.
    fn go_to(&mut self, idx: usize) -> bool {
        let Some(path) = self.dir_files.get(idx).cloned() else {
            return false;
        };
        self.current_idx = idx;
        self.show(&path)
    }

    /// Slideshow step: the next photo in the list (wrapping). False if there is no other photo.
    pub fn next_photo(&mut self) -> bool {
        let n = self.dir_files.len();
        let next = (1..n)
            .map(|k| (self.current_idx + k) % n)
            .find(|&i| probe_file(&self.dir_files[i]).is_image_kind());
        match next {
            Some(idx) => {
                self.forward = true;
                self.go_to(idx)
            }
            None => false,
        }
    }

    /// The bar's previous / next track and media keys: the adjacent audio/video file.
    pub fn skip_track(&mut self, forward: bool) -> bool {
        match playlist::skip(
            &self.dir_files,
            self.current_idx,
            forward,
            &self.config.queue,
        ) {
            Some(idx) => self.go_to(idx),
            None => false,
        }
    }

    /// The current file played to its end. Returns whether another file is now shown.
    pub fn playback_ended(&mut self) -> bool {
        match playlist::on_end(&self.dir_files, self.current_idx, &self.config.queue) {
            EndAction::Replay => {
                if let Some(media) = &self.media {
                    media.replay();
                }
                false
            }
            EndAction::Play(idx) => self.go_to(idx),
            EndAction::Stop => false,
        }
    }

    /// Moves to the next/previous file in the directory (wrapping around).
    pub fn navigate(&mut self, forward: bool) -> bool {
        let total = self.dir_files.len();
        if total <= 1 {
            return false;
        }
        let idx = if forward {
            (self.current_idx + 1) % total
        } else {
            (self.current_idx + total - 1) % total
        };
        self.forward = forward;
        self.go_to(idx);
        true
    }

    pub fn apply_config(&mut self, config: ViewerConfig) {
        let old = self.decode_options();
        i18n::set(config.language.resolve());
        self.config = config;
        self.osd_font = None;
        if old != self.decode_options() && self.shows_photo() {
            self.load_media();
        }
    }

    fn decode_options(&self) -> DecodeOptions {
        DecodeOptions {
            auto_rotate: self.config.auto_rotate_exif,
            background: self.config.photo_background,
            fit: screen_side(self.hwnd),
        }
    }

    /// Turns the photo on screen by a quarter (for viewing only; the file is untouched).
    pub fn rotate(&mut self, clockwise: bool) -> bool {
        let Some(img) = &self.image else { return false };
        self.image = Some(Arc::new(image_cache::rotated(img, clockwise)));
        if let Some(whole) = &self.whole {
            self.whole = Some(Arc::new(image_cache::rotated(whole, clockwise)));
        }
        self.quarter_turns = (self.quarter_turns + if clockwise { 1 } else { 3 }) % 4;
        self.zoom = ZoomMode::Fit;
        self.loupe = None;
        self.drag = None;
        true
    }

    /// The current file was deleted: drops it from the list and shows the one that took its
    /// place (or the new last one). False if nothing is left; a next file that can't be shown
    /// still keeps the viewer open, so the user can move on from it.
    pub fn remove_current(&mut self) -> bool {
        if self.current_idx < self.dir_files.len() {
            self.dir_files.remove(self.current_idx);
        }
        self.image = None;
        self.whole = None;
        self.previous = None;
        if self.dir_files.is_empty() {
            self.pending = None;
            self.media = None;
            return false;
        }
        self.go_to(self.current_idx.min(self.dir_files.len() - 1));
        true
    }

    /// Shows the current file again (e.g. after its player was closed to free the file).
    pub fn reload(&mut self) {
        let path = self.file_path.clone();
        self.show(&path);
    }

    fn load_media(&mut self) {
        let options = self.decode_options();
        let kind = probe_file(&self.file_path);
        self.file_size = std::fs::metadata(&self.file_path)
            .map(|m| m.len())
            .unwrap_or(0);

        let shown = self.image.take();
        self.pending = None;
        self.whole = None;
        self.whole_pending = None;
        self.load_failed = false;
        if kind.is_playable() {
            self.previous = None;
            // The player is reused when going from one file of the same kind to the next.
            let resume = self.config.resume_video;
            self.media = unsafe {
                MediaView::open(self.hwnd, self.media.take(), &self.file_path, kind, resume)
            };
            let can_skip = playlist::step(&self.dir_files, self.current_idx, true, true).is_some();
            if let Some(media) = self.media.as_mut() {
                media.set_skip(can_skip);
            }
        } else {
            self.media = None;
            if kind.is_image_kind() {
                // Checked before queueing: a file that fails it would be decoded for nothing.
                let request = header_looks_decodable(&self.file_path, kind)
                    .then(|| image_cache::request(&self.file_path, kind, options, self.hwnd))
                    .flatten();
                match request {
                    Some(Request::Ready(img)) => {
                        self.image = Some(img);
                        self.previous = None;
                    }
                    Some(Request::Pending(ticket)) => {
                        self.pending = Some(ticket);
                        self.previous = shown.or(self.previous.take());
                    }
                    None => {
                        self.load_failed = true;
                        self.previous = None;
                    }
                }
            } else {
                self.previous = None;
            }
        }

        image_cache::prefetch(self.prefetch_candidates(), options);
        self.request_whole();
    }

    /// Neighbouring photos to warm up, most likely next first: two ahead in the direction of the
    /// last move, one behind.
    fn prefetch_candidates(&self) -> Vec<PathBuf> {
        let total = self.dir_files.len();
        if total <= 1 {
            return Vec::new();
        }
        let at =
            |step: isize| (self.current_idx as isize + step).rem_euclid(total as isize) as usize;
        let dir = if self.forward { 1 } else { -1 };
        let mut indices = Vec::new();
        for i in [at(dir), at(-dir), at(2 * dir)] {
            if i != self.current_idx && !indices.contains(&i) {
                indices.push(i);
            }
        }
        indices
            .into_iter()
            .map(|i| self.dir_files[i].clone())
            .filter(|p| probe_file(p).is_image_kind())
            .collect()
    }
}

/// What [`ViewerState::image_ready`] took.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arrived {
    Nothing,
    /// The photo to show (or its failure).
    Photo,
    /// The whole picture of the photo already shown.
    Whole,
}

/// `img` turned by `quarter_turns` clockwise.
fn turned(img: Arc<DecodedImage>, quarter_turns: u8) -> Arc<DecodedImage> {
    match quarter_turns % 4 {
        0 => img,
        3 => Arc::new(image_cache::rotated(&img, false)),
        n => (0..n).fold(img, |img, _| Arc::new(image_cache::rotated(&img, true))),
    }
}

/// The longer side of the monitor showing `hwnd`: photos are decoded to fit a square of it, so
/// they stay sharp in fullscreen and when turned. 0 (the whole picture) if it is unknown.
fn screen_side(hwnd: HWND) -> u32 {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return 0;
        }
        let rc = info.rcMonitor;
        (rc.right - rc.left).max(rc.bottom - rc.top).max(0) as u32
    }
}

impl Drop for ViewerState {
    fn drop(&mut self) {
        // Stop playback before the surface window goes away.
        self.overlay = None;
        self.media = None;
    }
}

/// Case-insensitive, separator-agnostic (`/` vs `\`) path equality, as on NTFS.
pub fn same_path(a: &Path, b: &Path) -> bool {
    let (mut x, mut y) = (a.components(), b.components());
    loop {
        match (x.next(), y.next()) {
            (None, None) => return true,
            (Some(p), Some(q)) if same_name(p.as_os_str(), q.as_os_str()) => {}
            _ => return false,
        }
    }
}

/// Case-insensitive name comparison, Unicode included (NTFS ignores the case of Cyrillic too).
fn same_name(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    a.eq_ignore_ascii_case(b)
        || (!a.is_ascii()
            && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase())
}

/// Lists the viewable files (images, videos, audio) in the file's directory in natural ("file2" < "file10") order.
pub fn scan_directory_media(current_file: &Path) -> (Vec<PathBuf>, usize) {
    let single = || (vec![current_file.to_path_buf()], 0);
    let Some(entries) = current_file
        .parent()
        .and_then(|p| std::fs::read_dir(p).ok())
    else {
        return single();
    };

    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| is_viewable(probe_file(p)))
        .collect();
    if files.is_empty() {
        return single();
    }

    files.sort_by_cached_key(|p| natural_key(&p.file_name().unwrap_or_default().to_string_lossy()));
    let idx = files
        .iter()
        .position(|p| same_path(p, current_file))
        .unwrap_or(0);
    (files, idx)
}

fn is_viewable(kind: MediaType) -> bool {
    kind.is_image_kind() || kind.is_playable()
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Chunk {
    Num(u64, usize),
    Text(String),
}

/// Sort key splitting a name into case-insensitive text and numeric runs.
fn natural_key(name: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut chars = name.chars().peekable();
    while let Some(&c) = chars.peek() {
        let digits = c.is_ascii_digit();
        let mut run = String::new();
        while let Some(&c) = chars.peek().filter(|c| c.is_ascii_digit() == digits) {
            run.push(c);
            chars.next();
        }
        chunks.push(if digits {
            // Leading zeros break ties so "01" and "1" still have a stable order.
            Chunk::Num(run.parse().unwrap_or(u64::MAX), run.len())
        } else {
            Chunk::Text(run.to_lowercase())
        });
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_compare_like_the_file_system() {
        assert!(same_path(
            Path::new(r"C:\Media\Clip.MP4"),
            Path::new("c:/media/clip.mp4")
        ));
        assert!(!same_path(
            Path::new(r"C:\Media\a.mp4"),
            Path::new(r"C:\Media\b.mp4")
        ));
        assert!(!same_path(
            Path::new(r"C:\Media"),
            Path::new(r"C:\Media\a.mp4")
        ));
        assert!(same_path(
            Path::new(r"D:\Видео\Фильм.mkv"),
            Path::new(r"d:\видео\фильм.MKV")
        ));
    }

    #[test]
    fn natural_order() {
        let mut names = vec!["img10.jpg", "IMG2.jpg", "img1.jpg", "a.png", "img02.jpg"];
        names.sort_by_key(|n| natural_key(n));
        assert_eq!(
            names,
            ["a.png", "img1.jpg", "IMG2.jpg", "img02.jpg", "img10.jpg"]
        );
    }
}
