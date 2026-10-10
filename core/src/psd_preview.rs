//! PSD/PSB decoding: Section 5 merged composite, falling back to the embedded JPEG thumbnail.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use image::{DynamicImage, RgbaImage};

use crate::image_decode::{decode_bytes, MAX_PIXELS};
use crate::jpeg;

const PALETTE_LEN: usize = 768;
const MAX_RESOURCE_SECTION: usize = 8 * 1024 * 1024;
const SIGNATURE_SCAN_BYTES: u64 = 8 * 1024 * 1024;
const MIN_FALLBACK_JPEG: usize = 8 * 1024;
/// Photoshop's own limit.
const MAX_CHANNELS: usize = 56;

const MODE_GRAYSCALE: u16 = 1;
const MODE_INDEXED: u16 = 2;
const MODE_RGB: u16 = 3;
const MODE_CMYK: u16 = 4;

/// Load the PSD image (merged composite image, or the embedded JPEG thumbnail).
pub fn load_psd_image(path: &Path) -> Option<DynamicImage> {
    decode_psd_composite(path).or_else(|| decode_bytes(&extract_psd_preview(path)?))
}

/// Extract the embedded JPEG thumbnail (resource 0x0409/0x0410) as raw JPEG bytes.
pub fn extract_psd_preview(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 26];
    file.read_exact(&mut header).ok()?;
    if &header[0..4] != b"8BPS" {
        return None;
    }

    // Skip Color Mode Data.
    let color_data_len = read_u32(&mut file)?;
    file.seek(SeekFrom::Current(color_data_len as i64)).ok()?;

    // Image Resources section.
    let res_section_len = (read_u32(&mut file)? as usize).min(MAX_RESOURCE_SECTION);
    let mut res = vec![0u8; res_section_len];
    file.read_exact(&mut res).ok()?;
    if let Some(jpeg) = find_thumbnail_resource(&res) {
        return Some(jpeg.to_vec());
    }

    // Fallback: look for any JPEG stream near the start of the file.
    let mut raw = Vec::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(SIGNATURE_SCAN_BYTES).read_to_end(&mut raw).ok()?;
    jpeg::find_largest(&raw, MIN_FALLBACK_JPEG).map(<[u8]>::to_vec)
}

fn find_thumbnail_resource(res: &[u8]) -> Option<&[u8]> {
    let mut idx = 0;
    while idx + 12 <= res.len() {
        if &res[idx..idx + 4] != b"8BIM" {
            idx += 1;
            continue;
        }
        let res_id = u16::from_be_bytes([res[idx + 4], res[idx + 5]]);
        // Pascal name, padded so that (length byte + name) is even.
        let name_len = res[idx + 6] as usize;
        let data_offset = idx + 6 + ((name_len + 2) & !1);
        let size_bytes = res.get(data_offset..data_offset + 4)?;
        let size = u32::from_be_bytes([size_bytes[0], size_bytes[1], size_bytes[2], size_bytes[3]])
            as usize;
        let start = data_offset + 4;
        let end = start.checked_add(size)?;

        // 28-byte thumbnail header followed by JFIF data.
        if matches!(res_id, 0x0409 | 0x0410) && size > 28 && end <= res.len() {
            let jfif = &res[start + 28..end];
            if jfif.starts_with(&[0xFF, 0xD8, 0xFF]) {
                return Some(jfif);
            }
        }
        idx = start + ((size + 1) & !1);
    }
    None
}

fn read_u32(r: &mut impl Read) -> Option<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).ok()?;
    Some(u32::from_be_bytes(b))
}

/// Decode the Section 5 merged composite image of an 8/16-bit Grayscale/Indexed/RGB/CMYK PSD or PSB.
fn decode_psd_composite(path: &Path) -> Option<DynamicImage> {
    let file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut reader = BufReader::new(file);

    let mut header = [0u8; 26];
    reader.read_exact(&mut header).ok()?;
    if &header[0..4] != b"8BPS" {
        return None;
    }
    let is_psb = match u16::from_be_bytes([header[4], header[5]]) {
        1 => false,
        2 => true,
        _ => return None,
    };
    let channels = u16::from_be_bytes([header[12], header[13]]) as usize;
    let height = u32::from_be_bytes([header[14], header[15], header[16], header[17]]);
    let width = u32::from_be_bytes([header[18], header[19], header[20], header[21]]);
    let depth = u16::from_be_bytes([header[22], header[23]]);
    let color_mode = u16::from_be_bytes([header[24], header[25]]);

    // Bitmap (1-bit) and 32-bit float documents use layouts we don't decode; the thumbnail is used instead.
    let bytes_per_sample = match depth {
        8 => 1,
        16 => 2,
        _ => return None,
    };
    if !matches!(
        color_mode,
        MODE_GRAYSCALE | MODE_INDEXED | MODE_RGB | MODE_CMYK
    ) {
        return None;
    }
    if width == 0 || height == 0 || channels == 0 || channels > MAX_CHANNELS {
        return None;
    }
    let num_pixels = (width as u64) * (height as u64);
    if num_pixels > MAX_PIXELS {
        return None;
    }
    let num_pixels = num_pixels as usize;
    let (w, h) = (width as usize, height as usize);

    // Section 2: Color Mode Data (palette for indexed images).
    let color_data_len = read_u32(&mut reader)? as usize;
    let mut palette = Vec::new();
    if color_mode == MODE_INDEXED {
        if color_data_len < PALETTE_LEN {
            return None;
        }
        palette.resize(PALETTE_LEN, 0);
        reader.read_exact(&mut palette).ok()?;
        reader
            .seek(SeekFrom::Current((color_data_len - PALETTE_LEN) as i64))
            .ok()?;
    } else {
        reader.seek(SeekFrom::Current(color_data_len as i64)).ok()?;
    }

    // Section 3: Image Resources.
    let res_len = read_u32(&mut reader)?;
    reader.seek(SeekFrom::Current(res_len as i64)).ok()?;

    // Section 4: Layer and Mask Information. Only the sign of the layer count is needed: negative
    // means the first extra channel of the merged image is its transparency. Otherwise extra
    // channels are the user's saved selections and must not make the picture see-through.
    let read_len = |reader: &mut BufReader<File>| -> Option<u64> {
        if is_psb {
            let mut b = [0u8; 8];
            reader.read_exact(&mut b).ok()?;
            Some(u64::from_be_bytes(b))
        } else {
            read_u32(reader).map(u64::from)
        }
    };
    let layer_len = read_len(&mut reader)?;
    let layers_start = reader.stream_position().ok()?;
    let mut transparent = false;
    if layer_len > 0 {
        let info_len = read_len(&mut reader)?;
        if info_len >= 2 {
            let mut count = [0u8; 2];
            reader.read_exact(&mut count).ok()?;
            transparent = i16::from_be_bytes(count) < 0;
        }
    }
    reader
        .seek(SeekFrom::Start(layers_start.checked_add(layer_len)?))
        .ok()?;

    // Section 5: Image Data.
    let mut comp_buf = [0u8; 2];
    reader.read_exact(&mut comp_buf).ok()?;
    let compression = u16::from_be_bytes(comp_buf);
    let row_bytes = w * bytes_per_sample;
    let remaining = file_len.saturating_sub(reader.stream_position().ok()?);

    // The color channels, plus the transparency if the document has one; the rest are skipped.
    let color_channels = match color_mode {
        MODE_RGB => 3,
        MODE_CMYK => 4,
        _ => 1,
    };
    let active = channels.min(color_channels + usize::from(transparent));
    // Reserved only once the file is known to hold that much data.
    let new_planes = || -> Vec<Vec<u8>> { vec![Vec::with_capacity(num_pixels); active] };

    let planes = match compression {
        0 => {
            let plane_bytes = (num_pixels * bytes_per_sample) as u64;
            if (channels as u64) * plane_bytes > remaining {
                return None;
            }
            let mut planes = new_planes();
            let mut row = vec![0u8; row_bytes];
            for plane in planes.iter_mut() {
                for _ in 0..h {
                    reader.read_exact(&mut row).ok()?;
                    push_row(plane, &row, bytes_per_sample);
                }
            }
            planes
        }
        1 => {
            let entry_size = if is_psb { 4 } else { 2 };
            // Checked before allocating: the header's sizes may be anything.
            let table_len = (channels as u64) * (h as u64) * (entry_size as u64);
            if table_len > remaining {
                return None;
            }
            let mut table = vec![0u8; table_len as usize];
            reader.read_exact(&mut table).ok()?;
            let line_lens: Vec<usize> = table
                .chunks_exact(entry_size)
                .map(|c| c.iter().fold(0usize, |acc, &b| (acc << 8) | b as usize))
                .collect();
            let total: u64 = line_lens.iter().map(|&l| l as u64).sum();
            if total + table.len() as u64 > remaining {
                return None;
            }

            let mut planes = new_planes();
            let mut packed = Vec::new();
            let mut unpacked = Vec::with_capacity(row_bytes);
            for (c, lens) in line_lens.chunks_exact(h).enumerate() {
                let Some(plane) = planes.get_mut(c) else {
                    let skip: usize = lens.iter().sum();
                    reader.seek(SeekFrom::Current(skip as i64)).ok()?;
                    continue;
                };
                for &len in lens {
                    packed.resize(len, 0);
                    reader.read_exact(&mut packed).ok()?;
                    decode_packbits(&packed, row_bytes, &mut unpacked);
                    push_row(plane, &unpacked, bytes_per_sample);
                }
            }
            planes
        }
        // ZIP-compressed composites are not supported.
        _ => return None,
    };

    let rgba = compose_rgba(color_mode, &planes, &palette, num_pixels);
    RgbaImage::from_raw(width, height, rgba).map(DynamicImage::ImageRgba8)
}

/// Appends one row of samples to an 8-bit plane (16-bit samples keep their high byte).
fn push_row(plane: &mut Vec<u8>, row: &[u8], bytes_per_sample: usize) {
    if bytes_per_sample == 1 {
        plane.extend_from_slice(row);
    } else {
        plane.extend(row.chunks_exact(2).map(|s| s[0]));
    }
}

fn compose_rgba(color_mode: u16, planes: &[Vec<u8>], palette: &[u8], num_pixels: usize) -> Vec<u8> {
    let plane = |i: usize| planes.get(i).unwrap_or(&planes[0]).as_slice();
    let alpha = |i: usize| planes.get(i).map(Vec::as_slice);
    let mut rgba = vec![0u8; num_pixels * 4];

    let fill = |rgba: &mut [u8], a: Option<&[u8]>, f: &dyn Fn(usize) -> [u8; 3]| {
        for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
            px[..3].copy_from_slice(&f(i));
            px[3] = a.map_or(255, |a| a[i]);
        }
    };

    match color_mode {
        MODE_RGB => {
            let (r, g, b) = (plane(0), plane(1), plane(2));
            fill(&mut rgba, alpha(3), &|i| [r[i], g[i], b[i]]);
        }
        MODE_CMYK => {
            // Photoshop stores CMYK inverted (255 = no ink), so R = C' * K' / 255.
            let (c, m, y, k) = (plane(0), plane(1), plane(2), plane(3));
            let mix = |v: u8, k: u8| ((v as u32 * k as u32) / 255) as u8;
            fill(&mut rgba, alpha(4), &|i| {
                [mix(c[i], k[i]), mix(m[i], k[i]), mix(y[i], k[i])]
            });
        }
        MODE_GRAYSCALE => {
            let g = plane(0);
            fill(&mut rgba, alpha(1), &|i| [g[i]; 3]);
        }
        MODE_INDEXED => {
            let idx = plane(0);
            fill(&mut rgba, None, &|i| {
                let v = idx[i] as usize;
                [palette[v], palette[256 + v], palette[512 + v]]
            });
        }
        _ => {}
    }
    rgba
}

/// Decode one PackBits-compressed row into `out`, padded/truncated to exactly `expected_len` bytes.
fn decode_packbits(input: &[u8], expected_len: usize, out: &mut Vec<u8>) {
    out.clear();
    let mut i = 0;
    while i < input.len() && out.len() < expected_len {
        let n = input[i] as i8;
        i += 1;
        let room = expected_len - out.len();
        if n >= 0 {
            let end = (i + n as usize + 1).min(input.len());
            let take = (end - i).min(room);
            out.extend_from_slice(&input[i..i + take]);
            i = end;
        } else if n != -128 {
            let Some(&val) = input.get(i) else { break };
            i += 1;
            let count = (1 - n as isize) as usize;
            out.resize(out.len() + count.min(room), val);
        }
    }
    out.resize(expected_len, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PSD_2X2: &[u8] = &[
        56, 66, 80, 83, 0, 1, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 2, 0, 0, 0, 2, 0, 8, 0, 3, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 3, 0, 3, 0, 3, 0, 3, 0, 3, 0, 3, 1, 255, 255, 1, 0, 0,
        1, 0, 0, 1, 255, 255, 1, 0, 0, 1, 0, 0,
    ];

    const TEST_PSD_RGBA: &[u8] = &[
        56, 66, 80, 83, 0, 1, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 2, 0, 0, 0, 2, 0, 8, 0, 3, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 3, 0, 3, 0, 3, 0, 3, 0, 3, 0, 3, 0, 3, 0, 3, 1, 255,
        255, 1, 0, 0, 1, 0, 0, 1, 255, 255, 1, 0, 0, 1, 0, 0, 1, 255, 0, 1, 128, 255,
    ];

    fn load(name: &str, bytes: &[u8]) -> Option<DynamicImage> {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, bytes).expect("write test psd");
        let img = load_psd_image(&path);
        let _ = std::fs::remove_file(&path);
        img
    }

    #[test]
    fn decodes_rgb_composite() {
        let rgba = load("mediares_test_2x2.psd", TEST_PSD_2X2)
            .expect("2x2 psd")
            .to_rgba8();
        assert_eq!(rgba.dimensions(), (2, 2));
        assert_eq!(rgba.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(rgba.get_pixel(0, 1).0, [0, 255, 0, 255]);
    }

    /// `TEST_PSD_RGBA` with a layer section whose layer count is negative (-1): the 4th channel
    /// is the merged transparency.
    fn transparent_rgba() -> Vec<u8> {
        let mut psd = TEST_PSD_RGBA.to_vec();
        // Layer section length (offset 34), then: layer info length 2, layer count -1, no global
        // layer mask.
        psd[34..38].copy_from_slice(&10u32.to_be_bytes());
        psd.splice(38..38, [0, 0, 0, 2, 0xFF, 0xFF, 0, 0, 0, 0]);
        psd
    }

    #[test]
    fn decodes_rgba_composite() {
        let rgba = load("mediares_test_rgba.psd", &transparent_rgba())
            .expect("rgba psd")
            .to_rgba8();
        assert_eq!(rgba.get_pixel(1, 0).0, [255, 0, 0, 0]);
        assert_eq!(rgba.get_pixel(0, 1).0, [0, 255, 0, 128]);
    }

    /// Without layers saying so, an extra channel is a saved selection: the picture stays opaque.
    #[test]
    fn saved_selection_is_not_transparency() {
        let rgba = load("mediares_test_selection.psd", TEST_PSD_RGBA)
            .expect("rgba psd")
            .to_rgba8();
        assert_eq!(rgba.get_pixel(1, 0).0, [255, 0, 0, 255]);
        assert_eq!(rgba.get_pixel(0, 1).0, [0, 255, 0, 255]);
    }

    #[test]
    fn rejects_unsupported_depth_and_truncation() {
        let mut deep = TEST_PSD_2X2.to_vec();
        deep[23] = 32;
        assert!(load("mediares_test_32bit.psd", &deep).is_none());
        for len in 0..TEST_PSD_2X2.len() {
            let _ = load("mediares_test_trunc.psd", &TEST_PSD_2X2[..len]);
        }
    }

    /// A tiny RLE file whose header claims 64 M rows of 56 channels: rejected without allocating
    /// the line table (several GB), which would abort the process.
    #[test]
    fn huge_rle_header_is_rejected_cheaply() {
        let mut psd = TEST_PSD_2X2.to_vec();
        psd[12..14].copy_from_slice(&56u16.to_be_bytes());
        psd[14..18].copy_from_slice(&64_000_000u32.to_be_bytes());
        psd[18..22].copy_from_slice(&1u32.to_be_bytes());
        assert!(load("mediares_test_huge.psd", &psd).is_none());
        psd[12..14].copy_from_slice(&57u16.to_be_bytes());
        assert!(load("mediares_test_channels.psd", &psd).is_none());
    }

    #[test]
    fn packbits_pads_and_truncates() {
        let mut out = Vec::new();
        decode_packbits(&[0xFE, 7, 0, 9], 5, &mut out);
        assert_eq!(out, [7, 7, 7, 9, 0]);
        decode_packbits(&[0x02, 1, 2, 3, 0xFD, 4], 4, &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
    }
}
