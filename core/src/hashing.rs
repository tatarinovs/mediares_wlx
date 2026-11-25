//! Perceptual hashing and aspect ratio calculation.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

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

const XXH_PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87;
const XXH_PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const XXH_PRIME64_3: u64 = 0x1656_67B1_9E37_79F9;
const XXH_PRIME64_4: u64 = 0x85EB_CA77_C2B2_AE63;
const XXH_PRIME64_5: u64 = 0x27D4_EB2F_1656_67C5;
const PARTIAL_HASH_WINDOW: u64 = 64 * 1024;

#[inline]
fn xxh64_round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(XXH_PRIME64_2))
        .rotate_left(31)
        .wrapping_mul(XXH_PRIME64_1)
}

#[inline]
fn xxh64_merge_round(acc: u64, val: u64) -> u64 {
    (acc ^ xxh64_round(0, val))
        .wrapping_mul(XXH_PRIME64_1)
        .wrapping_add(XXH_PRIME64_4)
}

#[inline]
fn xxh64_avalanche(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(XXH_PRIME64_2);
    h ^= h >> 29;
    h = h.wrapping_mul(XXH_PRIME64_3);
    h ^ (h >> 32)
}

#[inline]
fn read_u64_le(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

#[inline]
fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

/// Streaming 64-bit xxHash64 implementation.
pub struct XxHash64 {
    total_len: u64,
    v1: u64,
    v2: u64,
    v3: u64,
    v4: u64,
    mem: [u8; 32],
    memsize: usize,
    seed: u64,
}

impl XxHash64 {
    pub fn new(seed: u64) -> Self {
        Self {
            total_len: 0,
            v1: seed.wrapping_add(XXH_PRIME64_1).wrapping_add(XXH_PRIME64_2),
            v2: seed.wrapping_add(XXH_PRIME64_2),
            v3: seed,
            v4: seed.wrapping_sub(XXH_PRIME64_1),
            mem: [0; 32],
            memsize: 0,
            seed,
        }
    }

    pub fn update(&mut self, mut input: &[u8]) {
        self.total_len = self.total_len.wrapping_add(input.len() as u64);

        if self.memsize + input.len() < 32 {
            self.mem[self.memsize..self.memsize + input.len()].copy_from_slice(input);
            self.memsize += input.len();
            return;
        }

        if self.memsize > 0 {
            let fill = 32 - self.memsize;
            self.mem[self.memsize..].copy_from_slice(&input[..fill]);
            self.v1 = xxh64_round(self.v1, read_u64_le(&self.mem[0..8]));
            self.v2 = xxh64_round(self.v2, read_u64_le(&self.mem[8..16]));
            self.v3 = xxh64_round(self.v3, read_u64_le(&self.mem[16..24]));
            self.v4 = xxh64_round(self.v4, read_u64_le(&self.mem[24..32]));
            input = &input[fill..];
            self.memsize = 0;
        }

        while input.len() >= 32 {
            self.v1 = xxh64_round(self.v1, read_u64_le(&input[0..8]));
            self.v2 = xxh64_round(self.v2, read_u64_le(&input[8..16]));
            self.v3 = xxh64_round(self.v3, read_u64_le(&input[16..24]));
            self.v4 = xxh64_round(self.v4, read_u64_le(&input[24..32]));
            input = &input[32..];
        }

        if !input.is_empty() {
            self.mem[..input.len()].copy_from_slice(input);
            self.memsize = input.len();
        }
    }

    pub fn finish(&self) -> u64 {
        let mut h = if self.total_len >= 32 {
            let mut h = self
                .v1
                .rotate_left(1)
                .wrapping_add(self.v2.rotate_left(7))
                .wrapping_add(self.v3.rotate_left(12))
                .wrapping_add(self.v4.rotate_left(18));
            h = xxh64_merge_round(h, self.v1);
            h = xxh64_merge_round(h, self.v2);
            h = xxh64_merge_round(h, self.v3);
            xxh64_merge_round(h, self.v4)
        } else {
            self.seed.wrapping_add(XXH_PRIME64_5)
        };

        h = h.wrapping_add(self.total_len);

        let mut rest = &self.mem[..self.memsize];
        while rest.len() >= 8 {
            let k1 = xxh64_round(0, read_u64_le(rest));
            h ^= k1;
            h = h
                .rotate_left(27)
                .wrapping_mul(XXH_PRIME64_1)
                .wrapping_add(XXH_PRIME64_4);
            rest = &rest[8..];
        }
        if rest.len() >= 4 {
            h ^= u64::from(read_u32_le(rest)).wrapping_mul(XXH_PRIME64_1);
            h = h
                .rotate_left(23)
                .wrapping_mul(XXH_PRIME64_2)
                .wrapping_add(XXH_PRIME64_3);
            rest = &rest[4..];
        }
        for &b in rest {
            h ^= u64::from(b).wrapping_mul(XXH_PRIME64_5);
            h = h.rotate_left(11).wrapping_mul(XXH_PRIME64_1);
        }

        xxh64_avalanche(h)
    }
}

pub fn xxhash64(data: &[u8], seed: u64) -> u64 {
    let mut h = XxHash64::new(seed);
    h.update(data);
    h.finish()
}

/// Fast 64-bit partial content hash: xxHash64 of `file_size || first 64 KiB || last 64 KiB`.
/// Reads the whole file when `size <= 128 KiB`. Returns `None` on empty or unreadable files.
pub fn compute_partial_hash(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }

    let mut hasher = XxHash64::new(0);
    hasher.update(&len.to_le_bytes());

    let mut buf = vec![0u8; PARTIAL_HASH_WINDOW as usize];
    if len <= PARTIAL_HASH_WINDOW * 2 {
        let mut remaining = len as usize;
        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            file.read_exact(&mut buf[..to_read]).ok()?;
            hasher.update(&buf[..to_read]);
            remaining -= to_read;
        }
    } else {
        file.read_exact(&mut buf).ok()?;
        hasher.update(&buf);
        file.seek(SeekFrom::Start(len - PARTIAL_HASH_WINDOW)).ok()?;
        file.read_exact(&mut buf).ok()?;
        hasher.update(&buf);
    }

    Some(hasher.finish())
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
        let scaled =
            GrayImage::from_fn(64, 64, |x, y| image::Luma([pattern.get_pixel(x, y)[0] * 2]));
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

    #[test]
    fn xxhash64_matches_reference_vectors() {
        assert_eq!(xxhash64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxhash64(b"", 0x9E37_79B1), 0xAC75_FDA2_929B_17EF);
        assert_eq!(xxhash64(b"abc", 0), 0x44BC_2CF5_AD77_0999);
        let long: Vec<u8> = (0..256u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        let one_shot = xxhash64(&long, 0x1234_5678);
        let mut stream = XxHash64::new(0x1234_5678);
        for chunk in long.chunks(7) {
            stream.update(chunk);
        }
        assert_eq!(stream.finish(), one_shot);
    }

    #[test]
    fn partial_hash_distinguishes_head_tail_size_and_empty() {
        let dir = std::env::temp_dir().join(format!("mediares_phash_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let empty = dir.join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(compute_partial_hash(&empty), None);

        let small_a = dir.join("small_a.bin");
        let small_b = dir.join("small_b.bin");
        std::fs::write(&small_a, b"hello world").unwrap();
        std::fs::write(&small_b, b"hello world").unwrap();
        assert_eq!(
            compute_partial_hash(&small_a),
            compute_partial_hash(&small_b)
        );

        let big_a = dir.join("big_a.bin");
        let big_b = dir.join("big_b.bin");
        let mut data = vec![0x5Au8; 200 * 1024];
        std::fs::write(&big_a, &data).unwrap();
        let h_a = compute_partial_hash(&big_a).unwrap();

        // Middle byte outside head/tail 64 KiB windows does not change partial hash.
        data[100 * 1024] ^= 0xFF;
        std::fs::write(&big_b, &data).unwrap();
        assert_eq!(compute_partial_hash(&big_b), Some(h_a));

        // Changing first or last 64 KiB alters partial hash.
        data[10] ^= 0x01;
        std::fs::write(&big_b, &data).unwrap();
        assert_ne!(compute_partial_hash(&big_b), Some(h_a));

        data[10] ^= 0x01;
        let last = data.len() - 10;
        data[last] ^= 0x01;
        std::fs::write(&big_b, &data).unwrap();
        assert_ne!(compute_partial_hash(&big_b), Some(h_a));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
