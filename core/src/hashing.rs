//! Perceptual hashing and aspect ratio calculation.

use image::imageops::{self, FilterType};
use image::{DynamicImage, GrayImage};

/// Hashes only need a few dozen pixels; large images are pre-shrunk once to this size.
const WORKING_SIZE: u32 = 256;

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

pub fn analyze(img: &DynamicImage) -> ImageAnalysis {
    let (width, height) = (img.width(), img.height());
    let gray = if width.max(height) > WORKING_SIZE {
        img.thumbnail(WORKING_SIZE, WORKING_SIZE).to_luma8()
    } else {
        img.to_luma8()
    };

    ImageAnalysis {
        width,
        height,
        aspect_ratio: compute_aspect_ratio(width, height),
        dhash: gradient_hash(&gray, 8, 8),
        coarse_hash: gradient_hash(&gray, 4, 8) as u32,
        phash: compute_phash(&gray),
    }
}

/// Difference hash: resize to (cols+1) x rows and compare horizontally adjacent pixels.
fn gradient_hash(gray: &GrayImage, cols: u32, rows: u32) -> u64 {
    let thumb = imageops::resize(gray, cols + 1, rows, FilterType::Triangle);
    let mut hash = 0u64;
    for y in 0..rows {
        for x in 0..cols {
            let left = thumb.get_pixel(x, y)[0];
            let right = thumb.get_pixel(x + 1, y)[0];
            hash = (hash << 1) | (left > right) as u64;
        }
    }
    hash
}

/// DCT hash: 8x8 low-frequency block of the 32x32 DCT-II, thresholded by the median of the AC terms.
fn compute_phash(gray: &GrayImage) -> u64 {
    const N: usize = 32;
    let thumb = imageops::resize(gray, N as u32, N as u32, FilterType::Triangle);

    let mut cos = [[0.0f64; N]; 8];
    for (k, row) in cos.iter_mut().enumerate() {
        for (n, c) in row.iter_mut().enumerate() {
            *c = (((2 * n + 1) * k) as f64 * std::f64::consts::PI / (2 * N) as f64).cos();
        }
    }
    let alpha = |k: usize| {
        if k == 0 {
            (1.0 / N as f64).sqrt()
        } else {
            (2.0 / N as f64).sqrt()
        }
    };

    let mut dct = [0.0f64; 64];
    for u in 0..8 {
        for v in 0..8 {
            let mut sum = 0.0;
            for (y, row) in thumb.rows().enumerate() {
                for (x, px) in row.enumerate() {
                    sum += px[0] as f64 * cos[u][y] * cos[v][x];
                }
            }
            dct[u * 8 + v] = sum * alpha(u) * alpha(v);
        }
    }

    let mut ac = dct[1..].to_vec();
    ac.sort_by(f64::total_cmp);
    let median = ac[ac.len() / 2];

    dct.iter()
        .fold(0u64, |hash, &c| (hash << 1) | (c > median) as u64)
}

pub fn compute_aspect_ratio(w: u32, h: u32) -> String {
    if h == 0 || w == 0 {
        return "Unknown".to_string();
    }

    let ratio = w as f64 / h as f64;
    const NAMED: &[(f64, f64, &str)] = &[
        (16.0 / 9.0, 0.02, "16:9"),
        (4.0 / 3.0, 0.02, "4:3"),
        (1.0, 0.01, "1:1"),
        (64.0 / 27.0, 0.03, "21:9"),
        (21.0 / 9.0, 0.03, "21:9"),
        (3.0 / 2.0, 0.02, "3:2"),
    ];
    if let Some(&(_, _, name)) = NAMED.iter().find(|(r, tol, _)| (ratio - r).abs() < *tol) {
        return name.to_string();
    }

    let g = gcd(w, h);
    let (rw, rh) = (w / g, h / g);
    if rw <= 20 && rh <= 20 {
        format!("{}:{}", rw, rh)
    } else {
        format!("{:.2}:1", ratio)
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_ratios() {
        assert_eq!(compute_aspect_ratio(1920, 1080), "16:9");
        assert_eq!(compute_aspect_ratio(100, 100), "1:1");
        assert_eq!(compute_aspect_ratio(6000, 4000), "3:2");
        assert_eq!(compute_aspect_ratio(500, 400), "5:4");
        assert_eq!(compute_aspect_ratio(0, 10), "Unknown");
    }

    #[test]
    fn large_and_downscaled_images_hash_alike() {
        // Blocky pattern with real structure (smooth gradients make pHash bits arbitrary).
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(1200, 800, |x, y| {
            let v = ((x / 150) * 7 + (y / 100) * 13) * 37 % 256;
            image::Rgb([v as u8, (255 - v) as u8, (v / 2) as u8])
        }));
        let small = img.resize_exact(300, 200, FilterType::Triangle);
        let (a, b) = (analyze(&img), analyze(&small));
        assert!(
            (a.dhash ^ b.dhash).count_ones() <= 4,
            "dhash distance {}",
            (a.dhash ^ b.dhash).count_ones()
        );
        assert!(
            (a.phash ^ b.phash).count_ones() <= 6,
            "phash distance {}",
            (a.phash ^ b.phash).count_ones()
        );
        assert_eq!((a.width, a.height), (1200, 800));
    }
}
