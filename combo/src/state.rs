//! Per-HWND viewer state for a Total Commander Lister instance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mediares_core::probe::probe_file;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{DeleteObject, HFONT};

use crate::config::ViewerConfig;
use crate::image_cache::{self, DecodedImage};

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
    pub zoom: ZoomMode,
    /// Top-left corner of the image in client coordinates (Custom zoom only).
    pub offset: (f32, f32),
    pub drag: Option<Drag>,
    pub loupe: Option<Loupe>,
    pub dir_files: Vec<PathBuf>,
    pub current_idx: usize,
    pub fullscreen: bool,
    pub config: ViewerConfig,
    /// Lazily created OSD font; dropped whenever the config changes.
    pub osd_font: Option<HFONT>,
}

impl ViewerState {
    /// Loads `path`; returns `None` if it is not a displayable image.
    pub fn new(lister: HWND, path: &Path, config: ViewerConfig) -> Option<Self> {
        let mut state = Self {
            hwnd: HWND::default(),
            lister,
            file_path: PathBuf::new(),
            file_size: 0,
            image: None,
            zoom: ZoomMode::Fit,
            offset: (0.0, 0.0),
            drag: None,
            loupe: None,
            dir_files: Vec::new(),
            current_idx: 0,
            fullscreen: false,
            config,
            osd_font: None,
        };
        state.set_file(path).then_some(state)
    }

    /// Switches to `path`, resetting the view. Returns whether an image could be displayed.
    pub fn set_file(&mut self, path: &Path) -> bool {
        self.file_path = path.to_path_buf();
        self.zoom = ZoomMode::Fit;
        self.offset = (0.0, 0.0);
        self.drag = None;
        self.loupe = None;

        match self.dir_files.iter().position(|p| same_path(p, path)) {
            Some(pos) => self.current_idx = pos,
            None => (self.dir_files, self.current_idx) = scan_directory_media(path),
        }
        self.load_media();
        self.image.is_some()
    }

    /// Moves to the next/previous image in the directory (wrapping around).
    pub fn navigate(&mut self, forward: bool) -> bool {
        let total = self.dir_files.len();
        if total <= 1 {
            return false;
        }
        let idx = if forward { (self.current_idx + 1) % total } else { (self.current_idx + total - 1) % total };
        let path = self.dir_files[idx].clone();
        self.set_file(&path);
        true
    }

    pub fn apply_config(&mut self, config: ViewerConfig) {
        let reload = config.auto_rotate_exif != self.config.auto_rotate_exif;
        self.config = config;
        self.drop_osd_font();
        if reload {
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
        self.image = if kind.is_image_kind() { image_cache::load(&self.file_path, kind, rotate) } else { None };

        // Warm up the neighbours for instant next/previous.
        let total = self.dir_files.len();
        if total > 1 {
            let next = self.dir_files[(self.current_idx + 1) % total].clone();
            let prev = self.dir_files[(self.current_idx + total - 1) % total].clone();
            let mut queue = vec![next];
            if total > 2 {
                queue.push(prev);
            }
            image_cache::prefetch(queue, rotate);
        }
    }
}

impl Drop for ViewerState {
    fn drop(&mut self) {
        self.drop_osd_font();
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
}

/// Lists the images in the file's directory in natural ("file2" < "file10") order.
pub fn scan_directory_media(current_file: &Path) -> (Vec<PathBuf>, usize) {
    let single = || (vec![current_file.to_path_buf()], 0);
    let Some(entries) = current_file.parent().and_then(|p| std::fs::read_dir(p).ok()) else {
        return single();
    };

    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| probe_file(p).is_image_kind())
        .collect();
    if files.is_empty() {
        return single();
    }

    files.sort_by_cached_key(|p| natural_key(&p.file_name().unwrap_or_default().to_string_lossy()));
    let idx = files.iter().position(|p| same_path(p, current_file)).unwrap_or(0);
    (files, idx)
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
    fn natural_order() {
        let mut names = vec!["img10.jpg", "IMG2.jpg", "img1.jpg", "a.png", "img02.jpg"];
        names.sort_by_key(|n| natural_key(n));
        assert_eq!(names, ["a.png", "img1.jpg", "IMG2.jpg", "img02.jpg", "img10.jpg"]);
    }
}
