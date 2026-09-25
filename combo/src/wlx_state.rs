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
    pub file_path: PathBuf,
    pub media_type: MediaType,
    pub image: Option<DecodedImage>,
    pub zoom_factor: f32,
    pub pan_x: i32,
    pub pan_y: i32,
}

impl ViewerState {
    pub fn new(hwnd: HWND, file_path: &Path) -> Self {
        let media_type = probe_file(file_path);
        let mut state = Self {
            hwnd,
            file_path: file_path.to_path_buf(),
            media_type,
            image: None,
            zoom_factor: 1.0,
            pan_x: 0,
            pan_y: 0,
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
}
