//! Per-HWND viewer state management for Total Commander Lister.

use std::path::{Path, PathBuf};
use windows::Win32::Foundation::HWND;
use mediares_core::probe::{probe_file, MediaType};

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub bgra_pixels: Vec<u8>,
}

pub struct ViewerState {
    pub hwnd: HWND,
    pub parent_hwnd: HWND,
    pub file_path: PathBuf,
    pub media_type: MediaType,
    pub image: Option<DecodedImage>,
    pub zoom_factor: f32,
    pub pan_x: i32,
    pub pan_y: i32,
    pub dir_files: Vec<PathBuf>,
    pub current_idx: usize,
}

impl ViewerState {
    pub fn new(hwnd: HWND, parent_hwnd: HWND, file_path: &Path) -> Self {
        let (dir_files, current_idx) = scan_directory_media(file_path);
        let media_type = probe_file(file_path);
        let mut state = Self {
            hwnd,
            parent_hwnd,
            file_path: file_path.to_path_buf(),
            media_type,
            image: None,
            zoom_factor: 1.0,
            pan_x: 0,
            pan_y: 0,
            dir_files,
            current_idx,
        };
        state.load_media();
        state
    }

    pub fn load_media(&mut self) {
        self.image = None;
        if self.media_type.is_image_kind() {
            self.image = crate::image_view::load_image_for_display(&self.file_path, self.media_type);
        }
    }

    pub fn set_file(&mut self, path: &Path) {
        self.file_path = path.to_path_buf();
        self.media_type = probe_file(&self.file_path);
        self.zoom_factor = 1.0;
        self.pan_x = 0;
        self.pan_y = 0;

        if let Some(pos) = self.dir_files.iter().position(|p| p == path) {
            self.current_idx = pos;
        } else {
            let (files, idx) = scan_directory_media(path);
            self.dir_files = files;
            self.current_idx = idx;
        }
        self.load_media();
    }
}

pub fn scan_directory_media(current_file: &Path) -> (Vec<PathBuf>, usize) {
    let parent = match current_file.parent() {
        Some(p) => p,
        None => return (vec![current_file.to_path_buf()], 0),
    };

    let Ok(entries) = std::fs::read_dir(parent) else {
        return (vec![current_file.to_path_buf()], 0);
    };

    let mut files = Vec::new();
    for entry in entries.flatten() {
        if let Ok(ft) = entry.file_type() {
            if ft.is_file() {
                let p = entry.path();
                let m = probe_file(&p);
                if m.is_image_kind() {
                    files.push(p);
                }
            }
        }
    }

    files.sort_by(|a, b| {
        let name_a = a.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let name_b = b.file_name().and_then(|n| n.to_str()).unwrap_or("");
        alphanumeric_sort_compare(name_a, name_b)
    });

    let current_idx = files.iter().position(|p| p == current_file).unwrap_or(0);
    (files, current_idx)
}

fn alphanumeric_sort_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let mut chars_a = a.chars().peekable();
    let mut chars_b = b.chars().peekable();

    while let (Some(&ca), Some(&cb)) = (chars_a.peek(), chars_b.peek()) {
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let mut num_a: u64 = 0;
            while let Some(&c) = chars_a.peek() {
                if c.is_ascii_digit() {
                    num_a = num_a.saturating_mul(10).saturating_add(c.to_digit(10).unwrap() as u64);
                    chars_a.next();
                } else {
                    break;
                }
            }

            let mut num_b: u64 = 0;
            while let Some(&c) = chars_b.peek() {
                if c.is_ascii_digit() {
                    num_b = num_b.saturating_mul(10).saturating_add(c.to_digit(10).unwrap() as u64);
                    chars_b.next();
                } else {
                    break;
                }
            }

            match num_a.cmp(&num_b) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            }
        } else {
            let la = ca.to_ascii_lowercase();
            let lb = cb.to_ascii_lowercase();
            match la.cmp(&lb) {
                std::cmp::Ordering::Equal => {
                    chars_a.next();
                    chars_b.next();
                }
                ord => return ord,
            }
        }
    }

    chars_a.peek().is_some().cmp(&chars_b.peek().is_some())
}
