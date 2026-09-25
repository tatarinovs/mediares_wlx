//! Per-HWND viewer state for a Total Commander Lister instance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mediares_core::image_decode::header_looks_decodable;
use mediares_core::probe::{probe_file, MediaType};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{DeleteObject, HFONT};

use crate::config::ViewerConfig;
use crate::image_cache::{self, DecodedImage, Request, Ticket};
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
    pub image: Option<Arc<DecodedImage>>,
    /// The photo being decoded in the background (then `image` is `None`).
    pub pending: Option<Ticket>,
    /// The last photo shown, drawn while `pending` so switching doesn't flash an empty window.
    pub previous: Option<Arc<DecodedImage>>,
    /// The current photo could not be decoded.
    pub load_failed: bool,
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
    pub osd_font: Option<HFONT>,
}

impl ViewerState {
    /// Loads `path` into the viewer window `hwnd`; `None` if it cannot be displayed.
    pub fn new(hwnd: HWND, lister: HWND, path: &Path, config: ViewerConfig) -> Option<Self> {
        let mut state = Self {
            hwnd,
            lister,
            file_path: PathBuf::new(),
            file_size: 0,
            image: None,
            pending: None,
            previous: None,
            load_failed: false,
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
        };
        state.set_file(path).then_some(state)
    }

    /// Switches to `path`, resetting the view. Returns whether it could be displayed. An M3U
    /// playlist replaces the file list with its entries and shows the first one.
    pub fn set_file(&mut self, path: &Path) -> bool {
        if probe_file(path) == MediaType::Playlist {
            let entries: Vec<PathBuf> = playlist::read_m3u(path).into_iter().filter(|p| is_viewable(probe_file(p))).collect();
            let Some(first) = entries.first().cloned() else { return false };
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

    /// Takes the background decode result if it has arrived. Returns whether the view changed.
    pub fn image_ready(&mut self) -> bool {
        let Some(result) = self.pending.as_ref().and_then(Ticket::result) else { return false };
        self.pending = None;
        self.previous = None;
        match result {
            Some(img) => self.image = Some(img),
            None => self.load_failed = true,
        }
        true
    }

    /// Shows the file at `idx` of the list; false if it can't be displayed.
    fn go_to(&mut self, idx: usize) -> bool {
        let Some(path) = self.dir_files.get(idx).cloned() else { return false };
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
        match playlist::skip(&self.dir_files, self.current_idx, forward, &self.config.queue) {
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
        let idx = if forward { (self.current_idx + 1) % total } else { (self.current_idx + total - 1) % total };
        self.forward = forward;
        self.go_to(idx);
        true
    }

    pub fn apply_config(&mut self, config: ViewerConfig) {
        let reload = config.auto_rotate_exif != self.config.auto_rotate_exif;
        self.config = config;
        self.drop_osd_font();
        if reload && self.shows_photo() {
            self.load_media();
        }
    }

    pub fn drop_osd_font(&mut self) {
        if let Some(font) = self.osd_font.take() {
            unsafe {
                let _ = DeleteObject(font.into());
            }
        }
    }

    fn load_media(&mut self) {
        let rotate = self.config.auto_rotate_exif;
        let kind = probe_file(&self.file_path);
        self.file_size = std::fs::metadata(&self.file_path).map(|m| m.len()).unwrap_or(0);

        let shown = self.image.take();
        self.pending = None;
        self.load_failed = false;
        if kind.is_playable() {
            self.previous = None;
            // The player is reused when going from one file of the same kind to the next.
            self.media = unsafe { MediaView::open(self.hwnd, self.media.take(), &self.file_path, kind) };
            let can_skip = playlist::step(&self.dir_files, self.current_idx, true, true).is_some();
            if let Some(media) = self.media.as_mut() {
                media.set_skip(can_skip);
            }
        } else {
            self.media = None;
            if kind.is_image_kind() {
                match image_cache::request(&self.file_path, kind, rotate, self.hwnd) {
                    Some(Request::Ready(img)) => {
                        self.image = Some(img);
                        self.previous = None;
                    }
                    Some(Request::Pending(ticket)) if header_looks_decodable(&self.file_path, kind) => {
                        self.pending = Some(ticket);
                        self.previous = shown.or(self.previous.take());
                    }
                    _ => {
                        self.load_failed = true;
                        self.previous = None;
                    }
                }
            } else {
                self.previous = None;
            }
        }

        image_cache::prefetch(self.prefetch_candidates(), rotate);
    }

    /// Neighbouring photos to warm up, most likely next first: two ahead in the direction of the
    /// last move, one behind.
    fn prefetch_candidates(&self) -> Vec<PathBuf> {
        let total = self.dir_files.len();
        if total <= 1 {
            return Vec::new();
        }
        let at = |step: isize| (self.current_idx as isize + step).rem_euclid(total as isize) as usize;
        let dir = if self.forward { 1 } else { -1 };
        let mut indices = Vec::new();
        for i in [at(dir), at(-dir), at(2 * dir)] {
            if i != self.current_idx && !indices.contains(&i) {
                indices.push(i);
            }
        }
        indices.into_iter().map(|i| self.dir_files[i].clone()).filter(|p| probe_file(p).is_image_kind()).collect()
    }
}

impl Drop for ViewerState {
    fn drop(&mut self) {
        // Stop playback before the surface window goes away.
        self.overlay = None;
        self.media = None;
        self.drop_osd_font();
    }
}

/// Case-insensitive, separator-agnostic (`/` vs `\`) path equality, as on NTFS.
fn same_path(a: &Path, b: &Path) -> bool {
    let (mut x, mut y) = (a.components(), b.components());
    loop {
        match (x.next(), y.next()) {
            (None, None) => return true,
            (Some(p), Some(q)) if p.as_os_str().eq_ignore_ascii_case(q.as_os_str()) => {}
            _ => return false,
        }
    }
}

/// Lists the viewable files (images, videos, audio) in the file's directory in natural ("file2" < "file10") order.
pub fn scan_directory_media(current_file: &Path) -> (Vec<PathBuf>, usize) {
    let single = || (vec![current_file.to_path_buf()], 0);
    let Some(entries) = current_file.parent().and_then(|p| std::fs::read_dir(p).ok()) else {
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
    let idx = files.iter().position(|p| same_path(p, current_file)).unwrap_or(0);
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
        assert!(same_path(Path::new(r"C:\Media\Clip.MP4"), Path::new("c:/media/clip.mp4")));
        assert!(!same_path(Path::new(r"C:\Media\a.mp4"), Path::new(r"C:\Media\b.mp4")));
        assert!(!same_path(Path::new(r"C:\Media"), Path::new(r"C:\Media\a.mp4")));
    }

    #[test]
    fn natural_order() {
        let mut names = vec!["img10.jpg", "IMG2.jpg", "img1.jpg", "a.png", "img02.jpg"];
        names.sort_by_key(|n| natural_key(n));
        assert_eq!(names, ["a.png", "img1.jpg", "IMG2.jpg", "img02.jpg", "img10.jpg"]);
    }
}
