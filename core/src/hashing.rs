//! Perceptual hashing and aspect ratio calculation.

use image::{image_dimensions, imageops::FilterType, DynamicImage, GenericImageView};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ImageAnalysis {
    pub width: u32,
    pub height: u32,
    pub aspect_ratio: String,
    pub dhash: u64,
    pub phash: u64,
    pub coarse_hash: u32,
}

impl ImageAnalysis {
    pub fn dhash_hex(&self) -> String {
        format!("{:016x}", self.dhash)
    }

    pub fn phash_hex(&self) -> String {
        format!("{:016x}", self.phash)
    }

    pub fn coarse_hash_hex(&self) -> String {
        format!("{:08x}", self.coarse_hash)
    }

    pub fn dimensions_str(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

pub fn analyze_dynamic_image(img: &DynamicImage) -> ImageAnalysis {
    let (width, height) = img.dimensions();
    let aspect_ratio = compute_aspect_ratio(width, height);
    let dhash = compute_dhash(img);
    let coarse_hash = compute_coarse_hash(img);
    let phash = compute_phash(img);

    ImageAnalysis {
        width,
        height,
        aspect_ratio,
        dhash,
        phash,
        coarse_hash,
    }
}

pub fn analyze_image(path: &Path) -> Result<ImageAnalysis, String> {
    let img = image::open(path).map_err(|e| format!("Failed to open image: {}", e))?;
    Ok(analyze_dynamic_image(&img))
}

pub fn analyze_image_from_memory(bytes: &[u8]) -> Result<ImageAnalysis, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("Failed to decode image: {}", e))?;
    Ok(analyze_dynamic_image(&img))
}

pub fn get_image_dimensions_fast(path: &Path) -> Option<(u32, u32, String)> {
    if let Ok((w, h)) = image_dimensions(path) {
        let aspect = compute_aspect_ratio(w, h);
        Some((w, h, aspect))
    } else {
        None
    }
}

pub fn compute_dhash(img: &DynamicImage) -> u64 {
    let thumb = img.resize_exact(9, 8, FilterType::Triangle).to_luma8();
    let mut hash: u64 = 0;

    for y in 0..8 {
        for x in 0..8 {
            let left = thumb.get_pixel(x, y)[0];
            let right = thumb.get_pixel(x + 1, y)[0];
            hash = (hash << 1) | (if left > right { 1 } else { 0 });
        }
    }

    hash
}

pub fn compute_coarse_hash(img: &DynamicImage) -> u32 {
    let thumb = img.resize_exact(5, 8, FilterType::Triangle).to_luma8();
    let mut hash: u32 = 0;

    for y in 0..8 {
        for x in 0..4 {
            let left = thumb.get_pixel(x, y)[0];
            let right = thumb.get_pixel(x + 1, y)[0];
            hash = (hash << 1) | (if left > right { 1 } else { 0 });
        }
    }

    hash
}

pub fn compute_phash(img: &DynamicImage) -> u64 {
    let thumb = img.resize_exact(32, 32, FilterType::Triangle).to_luma8();

    let mut matrix = [[0.0f64; 32]; 32];
    for (y, row) in matrix.iter_mut().enumerate() {
        for (x, val) in row.iter_mut().enumerate() {
            *val = thumb.get_pixel(x as u32, y as u32)[0] as f64;
        }
    }

    let mut dct_8x8 = [0.0f64; 64];
    let pi = std::f64::consts::PI;

    for u in 0..8 {
        for v in 0..8 {
            let mut sum = 0.0;
            for (x, row) in matrix.iter().enumerate().take(32) {
                let cos_x = (((2 * x + 1) * u) as f64 * pi / 64.0).cos();
                for (y, &val) in row.iter().enumerate().take(32) {
                    let cos_y = (((2 * y + 1) * v) as f64 * pi / 64.0).cos();
                    sum += val * cos_x * cos_y;
                }
            }

            let alpha_u = if u == 0 {
                1.0 / 32.0f64.sqrt()
            } else {
                (2.0 / 32.0f64).sqrt()
            };
            let alpha_v = if v == 0 {
                1.0 / 32.0f64.sqrt()
            } else {
                (2.0 / 32.0f64).sqrt()
            };
            dct_8x8[u * 8 + v] = sum * alpha_u * alpha_v;
        }
    }

    let mut non_dc: Vec<f64> = dct_8x8[1..].to_vec();
    non_dc.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if non_dc.len().is_multiple_of(2) {
        (non_dc[non_dc.len() / 2 - 1] + non_dc[non_dc.len() / 2]) / 2.0
    } else {
        non_dc[non_dc.len() / 2]
    };

    let mut hash: u64 = 0;
    for &coeff in &dct_8x8 {
        hash = (hash << 1) | (if coeff > median { 1 } else { 0 });
    }

    hash
}

pub fn compute_aspect_ratio(w: u32, h: u32) -> String {
    if h == 0 || w == 0 {
        return "Unknown".to_string();
    }

    let gcd = gcd(w, h);
    let rw = w / gcd;
    let rh = h / gcd;

    let ratio = w as f64 / h as f64;
    if (ratio - 16.0 / 9.0).abs() < 0.02 {
        "16:9".to_string()
    } else if (ratio - 4.0 / 3.0).abs() < 0.02 {
        "4:3".to_string()
    } else if (ratio - 1.0).abs() < 0.01 {
        "1:1".to_string()
    } else if (ratio - 64.0 / 27.0).abs() < 0.03 || (ratio - 21.0 / 9.0).abs() < 0.03 {
        "21:9".to_string()
    } else if (ratio - 3.0 / 2.0).abs() < 0.02 {
        "3:2".to_string()
    } else if rw <= 20 && rh <= 20 {
        format!("{}:{}", rw, rh)
    } else {
        format!("{:.2}:1", ratio)
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}
