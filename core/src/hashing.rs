//! Perceptual hashing and aspect ratio calculation.

use image::imageops::{self, FilterType};
use image::{DynamicImage, GrayImage};

/// Hashes only need a few dozen pixels; large images are pre-shrunk once to this size.
pub(crate) const WORKING_SIZE: u32 = 128;

#[derive(Debug, Clone)]
pub struct ImageAnalysis {
    pub width: u32,
    pub height: u32,
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

    pub fn aspect_ratio(&self) -> String {
        compute_aspect_ratio(self.width, self.height)
    }
}

pub fn analyze(img: &DynamicImage) -> ImageAnalysis {
    let (width, height) = (img.width(), img.height());
    let gray = if width.max(height) > WORKING_SIZE {
        crate::image_decode::thumbnail(img, WORKING_SIZE, WORKING_SIZE).to_luma8()
    } else {
        img.to_luma8()
    };
    analyze_gray(&gray, width, height)
}

pub fn analyze_gray(gray: &GrayImage, width: u32, height: u32) -> ImageAnalysis {
    ImageAnalysis {
        width,
        height,
        dhash: gradient_hash(gray, 8, 8),
        coarse_hash: gradient_hash(gray, 4, 8) as u32,
        phash: compute_phash(gray),
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
    let owned;
    let thumb = if gray.width() == N as u32 && gray.height() == N as u32 {
        gray
    } else {
        owned = imageops::resize(gray, N as u32, N as u32, FilterType::Triangle);
        &owned
    };
    let cos = phash_cosines();
    let mut horizontal = [[0.0f32; 8]; N];
    for (y, row) in thumb.as_raw().as_chunks::<N>().0.iter().enumerate() {
        for (x, &px) in row.iter().enumerate() {
            let p = px as f32;
            let row_cos = &cos[x];
            for v in 0..8 {
                horizontal[y][v] += p * row_cos[v];
            }
        }
    }
    let alpha = |k: usize| {
        if k == 0 {
            (1.0f32 / N as f32).sqrt()
        } else {
            (2.0f32 / N as f32).sqrt()
        }
    };

    let mut dct = [0.0f32; 64];
    for u in 0..8 {
        let au = alpha(u);
        for v in 0..8 {
            let mut sum = 0.0f32;
            for y in 0..N {
                sum += horizontal[y][v] * cos[y][u];
            }
            dct[u * 8 + v] = sum * au * alpha(v);
        }
    }

    let mut ac = [0.0f32; 63];
    ac.copy_from_slice(&dct[1..]);
    let (_, &mut median, _) = ac.select_nth_unstable_by(31, f32::total_cmp);

    dct.iter()
        .fold(0u64, |hash, &c| (hash << 1) | (c > median) as u64)
}

fn phash_cosines() -> &'static [[f32; 8]; 32] {
    static COS: std::sync::OnceLock<[[f32; 8]; 32]> = std::sync::OnceLock::new();
    COS.get_or_init(|| {
        let mut cos = [[0.0f32; 8]; 32];
        for (n, row) in cos.iter_mut().enumerate() {
            for (k, c) in row.iter_mut().enumerate() {
                *c = (((2 * n + 1) * k) as f64 * std::f64::consts::PI / 64.0).cos() as f32;
            }
        }
        cos
    })
}

/// Longest side the sharpness is measured at: values compare between photos of any size.
pub const SHARPNESS_SIZE: u32 = 1024;

/// Sharpness of a picture fitted into [`SHARPNESS_SIZE`]: the variance of its Laplacian (edge
/// response). Blurred and shaken shots score low, crisp ones high; there is no fixed scale, it
/// is for sorting photos of one scene.
pub fn sharpness(img: &DynamicImage) -> u32 {
    let (w, h) = (img.width(), img.height());
    let gray = if w.max(h) > SHARPNESS_SIZE {
        crate::image_decode::thumbnail(img, SHARPNESS_SIZE, SHARPNESS_SIZE).to_luma8()
    } else {
        img.to_luma8()
    };
    sharpness_gray(&gray)
}

pub fn sharpness_gray(gray: &GrayImage) -> u32 {
    laplacian_variance(gray).round().min(u32::MAX as f64) as u32
}

fn laplacian_variance(gray: &GrayImage) -> f64 {
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    if w < 3 || h < 3 {
        return 0.0;
    }
    let px = gray.as_raw();
    let (mut sum, mut sum_sq) = (0i64, 0i64);
    for y in 1..h - 1 {
        let (up, row, down) = (
            &px[(y - 1) * w..y * w],
            &px[y * w..(y + 1) * w],
            &px[(y + 1) * w..(y + 2) * w],
        );
        for x in 1..w - 1 {
            let lap = 4 * i32::from(row[x])
                - i32::from(row[x - 1])
                - i32::from(row[x + 1])
                - i32::from(up[x])
                - i32::from(down[x]);
            let lap = i64::from(lap);
            sum += lap;
            sum_sq += lap * lap;
        }
    }
    let n = ((w - 2) * (h - 2)) as f64;
    let mean = sum as f64 / n;
    sum_sq as f64 / n - mean * mean
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
    fn phash_is_deterministic_and_invariant_to_uniform_scaling() {
        let mut seed = 0x9E37_79B9u32;
        let pattern = GrayImage::from_fn(64, 64, |_, _| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            image::Luma([((seed >> 24) as u8) / 2])
        });
        let h1 = compute_phash(&pattern);
        let h2 = compute_phash(&pattern);
        assert_eq!(h1, h2);
        assert_ne!(h1, 0);
        // Exact 2x uniform scaling preserves DCT median comparisons.
        let scaled = GrayImage::from_fn(64, 64, |x, y| {
            image::Luma([pattern.get_pixel(x, y)[0] * 2])
        });
        assert_eq!(h1, compute_phash(&scaled));
    }

    #[test]
    fn aspect_ratios() {
        assert_eq!(compute_aspect_ratio(1920, 1080), "16:9");
        assert_eq!(compute_aspect_ratio(100, 100), "1:1");
        assert_eq!(compute_aspect_ratio(6000, 4000), "3:2");
        assert_eq!(compute_aspect_ratio(500, 400), "5:4");
        assert_eq!(compute_aspect_ratio(0, 10), "Unknown");
    }

    #[test]
    fn blur_lowers_sharpness() {
        let sharp = DynamicImage::ImageLuma8(GrayImage::from_fn(400, 300, |x, y| {
            image::Luma([if (x / 4 + y / 4) % 2 == 0 { 30 } else { 220 }])
        }));
        let blurred = sharp.blur(3.0);
        let flat = DynamicImage::ImageLuma8(GrayImage::from_pixel(400, 300, image::Luma([128])));
        let (s, b) = (sharpness(&sharp), sharpness(&blurred));
        assert!(s > 10 * b.max(1), "sharp {s}, blurred {b}");
        assert_eq!(sharpness(&flat), 0);
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
