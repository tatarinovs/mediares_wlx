//! PSD thumbnail / composite preview extraction and decoding.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use image::DynamicImage;

/// Load the PSD image (merged composite image, thumbnail, or embedded JPEG preview).
pub fn load_psd_image(path: &Path) -> Option<DynamicImage> {
    // 1. Try decoding the Section 5 merged composite image (highest quality full resolution)
    if let Some(img) = decode_psd_composite(path) {
        return Some(img);
    }

    // 2. Fallback: try extracting embedded JPEG thumbnail or stream
    if let Some(bytes) = extract_psd_preview(path) {
        if let Ok(img) = image::load_from_memory(&bytes) {
            return Some(img);
        }
    }

    None
}

/// Extract thumbnail / composite preview from PSD as raw JPEG bytes (fallback path).
pub fn extract_psd_preview(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len() as usize;
    if file_len < 26 {
        return None;
    }

    let mut header = [0u8; 26];
    file.read_exact(&mut header).ok()?;

    // Check signature '8BPS'
    if &header[0..4] != b"8BPS" {
        return None;
    }

    // Skip ColorModeData
    let mut color_data_len_buf = [0u8; 4];
    file.read_exact(&mut color_data_len_buf).ok()?;
    let color_data_len = u32::from_be_bytes(color_data_len_buf) as u64;
    file.seek(SeekFrom::Current(color_data_len as i64)).ok()?;

    // Image Resources Section
    let mut res_section_len_buf = [0u8; 4];
    file.read_exact(&mut res_section_len_buf).ok()?;
    let res_section_len = u32::from_be_bytes(res_section_len_buf) as usize;

    let max_res = res_section_len.min(8 * 1024 * 1024);
    let mut res_data = vec![0u8; max_res];
    file.read_exact(&mut res_data).ok()?;

    // Find resource 0x0410 (Photoshop 5.0 thumbnail) or 0x0409
    let mut idx = 0;
    while idx + 12 <= res_data.len() {
        if &res_data[idx..idx + 4] == b"8BIM" {
            let res_id = u16::from_be_bytes([res_data[idx + 4], res_data[idx + 5]]);
            let name_len = res_data[idx + 6] as usize;
            let name_padding = if (name_len + 1) % 2 != 0 { 1 } else { 0 };
            let data_offset = idx + 7 + name_len + name_padding;

            if data_offset + 4 <= res_data.len() {
                let data_size = u32::from_be_bytes([
                    res_data[data_offset],
                    res_data[data_offset + 1],
                    res_data[data_offset + 2],
                    res_data[data_offset + 3],
                ]) as usize;
                let payload_start = data_offset + 4;
                let payload_end = payload_start + data_size;

                if (res_id == 0x0410 || res_id == 0x0409) && payload_end <= res_data.len() && data_size > 28 {
                    // Resource 0x0410 format: 28-byte thumbnail header followed by raw JFIF JPEG
                    let jfif_bytes = &res_data[payload_start + 28..payload_end];
                    if jfif_bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
                        return Some(jfif_bytes.to_vec());
                    }
                }

                let padded_size = (data_size + 1) & !1;
                idx = payload_start + padded_size;
                continue;
            }
        }
        idx += 1;
    }

    // Fallback: search for JPEG signature inside the file
    let _ = file.seek(SeekFrom::Start(0));
    let mut raw_bytes = Vec::new();
    let _ = file.take(8 * 1024 * 1024).read_to_end(&mut raw_bytes);
    if let Some(soi) = memchr::memmem::find(&raw_bytes, &[0xFF, 0xD8, 0xFF]) {
        if let Some(eoi) = memchr::memmem::find(&raw_bytes[soi + 2..], &[0xFF, 0xD9]) {
            let end = soi + 2 + eoi + 2;
            let len = end - soi;
            if len > 8192 {
                return Some(raw_bytes[soi..end].to_vec());
            }
        }
    }

    None
}

/// Decode the Section 5 merged composite image of a PSD or PSB file.
fn decode_psd_composite(path: &Path) -> Option<DynamicImage> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);

    let mut header = [0u8; 26];
    reader.read_exact(&mut header).ok()?;

    if &header[0..4] != b"8BPS" {
        return None;
    }

    let version = u16::from_be_bytes([header[4], header[5]]);
    if version != 1 && version != 2 {
        return None;
    }
    let is_psb = version == 2;

    let channels = u16::from_be_bytes([header[12], header[13]]);
    let height = u32::from_be_bytes([header[14], header[15], header[16], header[17]]);
    let width = u32::from_be_bytes([header[18], header[19], header[20], header[21]]);
    let depth = u16::from_be_bytes([header[22], header[23]]);
    let color_mode = u16::from_be_bytes([header[24], header[25]]);

    if width == 0 || height == 0 || channels == 0 {
        return None;
    }

    // Guard against unreasonably large dimensions causing OOM
    let num_pixels = (width as usize).checked_mul(height as usize)?;
    if num_pixels > 100_000_000 {
        return None;
    }

    // Supported color modes:
    // 0 = Bitmap, 1 = Grayscale, 2 = Indexed, 3 = RGB, 4 = CMYK
    if color_mode > 4 {
        return None;
    }

    // Section 2: Color Mode Data
    let mut len4 = [0u8; 4];
    reader.read_exact(&mut len4).ok()?;
    let color_data_len = u32::from_be_bytes(len4) as usize;
    let mut palette = Vec::new();
    if color_mode == 2 && color_data_len >= 768 {
        palette.resize(color_data_len, 0);
        reader.read_exact(&mut palette).ok()?;
    } else if color_data_len > 0 {
        reader.seek(SeekFrom::Current(color_data_len as i64)).ok()?;
    }

    // Section 3: Image Resources
    reader.read_exact(&mut len4).ok()?;
    let res_len = u32::from_be_bytes(len4) as u64;
    if res_len > 0 {
        reader.seek(SeekFrom::Current(res_len as i64)).ok()?;
    }

    // Section 4: Layer and Mask Information
    let layer_len: u64 = if is_psb {
        let mut len8 = [0u8; 8];
        reader.read_exact(&mut len8).ok()?;
        u64::from_be_bytes(len8)
    } else {
        reader.read_exact(&mut len4).ok()?;
        u32::from_be_bytes(len4) as u64
    };
    if layer_len > 0 {
        reader.seek(SeekFrom::Current(layer_len as i64)).ok()?;
    }

    // Section 5: Image Data
    let mut comp_buf = [0u8; 2];
    reader.read_exact(&mut comp_buf).ok()?;
    let compression = u16::from_be_bytes(comp_buf);

    let bytes_per_sample = if depth == 16 { 2 } else { 1 };
    let expected_scanline_bytes = (width as usize).checked_mul(bytes_per_sample)?;

    // We only need at most 5 channels (CMYK + Alpha or RGBA)
    let active_channels = (channels as usize).min(5);
    let mut channel_data: Vec<Vec<u8>> = Vec::with_capacity(active_channels);
    for _ in 0..active_channels {
        channel_data.push(vec![0u8; num_pixels]);
    }

    match compression {
        0 => {
            // Raw planar
            let channel_bytes_count = num_pixels.checked_mul(bytes_per_sample)?;
            let mut raw_buf = vec![0u8; channel_bytes_count];

            for c in 0..channels as usize {
                if c < active_channels {
                    reader.read_exact(&mut raw_buf).ok()?;
                    if depth == 16 {
                        for i in 0..num_pixels {
                            channel_data[c][i] = raw_buf[i * 2]; // Take high byte
                        }
                    } else {
                        channel_data[c].copy_from_slice(&raw_buf);
                    }
                } else {
                    reader.seek(SeekFrom::Current(channel_bytes_count as i64)).ok()?;
                }
            }
        }
        1 => {
            // RLE (PackBits)
            let count_entries = (channels as usize).checked_mul(height as usize)?;
            let count_table_bytes = count_entries.checked_mul(if is_psb { 4 } else { 2 })?;
            let mut count_table_buf = vec![0u8; count_table_bytes];
            reader.read_exact(&mut count_table_buf).ok()?;

            let mut scanline_lens = Vec::with_capacity(count_entries);
            if is_psb {
                for chunk in count_table_buf.chunks_exact(4) {
                    scanline_lens.push(u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as usize);
                }
            } else {
                for chunk in count_table_buf.chunks_exact(2) {
                    scanline_lens.push(u16::from_be_bytes([chunk[0], chunk[1]]) as usize);
                }
            }

            let mut compressed_buf = Vec::new();

            for c in 0..channels as usize {
                for y in 0..height as usize {
                    let idx = c * (height as usize) + y;
                    let line_len = scanline_lens.get(idx).copied().unwrap_or(0);

                    if c < active_channels {
                        compressed_buf.resize(line_len, 0);
                        reader.read_exact(&mut compressed_buf).ok()?;

                        let unpacked = decode_packbits(&compressed_buf, expected_scanline_bytes);
                        let row_offset = y * (width as usize);

                        if depth == 16 {
                            for x in 0..width as usize {
                                channel_data[c][row_offset + x] = unpacked[x * 2];
                            }
                        } else {
                            for x in 0..width as usize {
                                channel_data[c][row_offset + x] = unpacked[x];
                            }
                        }
                    } else {
                        reader.seek(SeekFrom::Current(line_len as i64)).ok()?;
                    }
                }
            }
        }
        _ => return None, // ZIP or unsupported compression
    }

    // Assemble pixels into RGBA
    let mut rgba = vec![0u8; num_pixels * 4];

    match color_mode {
        3 => {
            // RGB
            let r_ch = &channel_data[0];
            let g_ch = channel_data.get(1).map(|v| v.as_slice()).unwrap_or(r_ch);
            let b_ch = channel_data.get(2).map(|v| v.as_slice()).unwrap_or(r_ch);
            let has_alpha = channels >= 4 && channel_data.len() >= 4;
            let a_ch = if has_alpha { Some(&channel_data[3]) } else { None };

            for i in 0..num_pixels {
                let offset = i * 4;
                rgba[offset] = r_ch[i];
                rgba[offset + 1] = g_ch[i];
                rgba[offset + 2] = b_ch[i];
                rgba[offset + 3] = if let Some(a) = a_ch { a[i] } else { 255 };
            }
        }
        4 => {
            // CMYK (Adobe Photoshop inverted convention)
            let c_ch = &channel_data[0];
            let m_ch = channel_data.get(1).map(|v| v.as_slice()).unwrap_or(c_ch);
            let y_ch = channel_data.get(2).map(|v| v.as_slice()).unwrap_or(c_ch);
            let k_ch = channel_data.get(3).map(|v| v.as_slice()).unwrap_or(c_ch);
            let has_alpha = channels >= 5 && channel_data.len() >= 5;
            let a_ch = if has_alpha { Some(&channel_data[4]) } else { None };

            for i in 0..num_pixels {
                let c = c_ch[i] as u32;
                let m = m_ch[i] as u32;
                let y = y_ch[i] as u32;
                let k = k_ch[i] as u32;

                let offset = i * 4;
                rgba[offset] = ((c * k) / 255) as u8;
                rgba[offset + 1] = ((m * k) / 255) as u8;
                rgba[offset + 2] = ((y * k) / 255) as u8;
                rgba[offset + 3] = if let Some(a) = a_ch { a[i] } else { 255 };
            }
        }
        1 => {
            // Grayscale
            let gray_ch = &channel_data[0];
            let has_alpha = channels >= 2 && channel_data.len() >= 2;
            let a_ch = if has_alpha { Some(&channel_data[1]) } else { None };

            for i in 0..num_pixels {
                let g = gray_ch[i];
                let offset = i * 4;
                rgba[offset] = g;
                rgba[offset + 1] = g;
                rgba[offset + 2] = g;
                rgba[offset + 3] = if let Some(a) = a_ch { a[i] } else { 255 };
            }
        }
        2 => {
            // Indexed
            let idx_ch = &channel_data[0];
            for i in 0..num_pixels {
                let val = idx_ch[i] as usize;
                let offset = i * 4;
                if palette.len() >= 768 {
                    rgba[offset] = palette[val];
                    rgba[offset + 1] = palette[256 + val];
                    rgba[offset + 2] = palette[512 + val];
                } else {
                    rgba[offset] = val as u8;
                    rgba[offset + 1] = val as u8;
                    rgba[offset + 2] = val as u8;
                }
                rgba[offset + 3] = 255;
            }
        }
        0 => {
            // Bitmap (1-bit monochrome)
            let raw_ch = &channel_data[0];
            for i in 0..num_pixels {
                let val = if raw_ch[i] != 0 { 255 } else { 0 };
                let offset = i * 4;
                rgba[offset] = val;
                rgba[offset + 1] = val;
                rgba[offset + 2] = val;
                rgba[offset + 3] = 255;
            }
        }
        _ => return None,
    }

    image::RgbaImage::from_raw(width, height, rgba).map(DynamicImage::ImageRgba8)
}

/// Decode standard PackBits / Apple RLE compression into target buffer.
fn decode_packbits(input: &[u8], expected_len: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(expected_len);
    let mut i = 0;
    while i < input.len() && output.len() < expected_len {
        let b = input[i] as i8;
        i += 1;
        if b >= 0 {
            let count = (b as usize) + 1;
            let end = (i + count).min(input.len());
            let take = (end - i).min(expected_len - output.len());
            output.extend_from_slice(&input[i..i + take]);
            i += count;
        } else if b != -128 {
            if i < input.len() {
                let count = (1 - (b as i16)) as usize;
                let val = input[i];
                i += 1;
                let take = count.min(expected_len - output.len());
                output.resize(output.len() + take, val);
            }
        }
    }
    if output.len() < expected_len {
        output.resize(expected_len, 0);
    }
    output
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_decode_psd_2x2_and_rgba() {
        let p2 = Path::new(r"d:\PROJECT\media_dup_finder_wdx\test_2x2.psd");
        if p2.exists() {
            let img = load_psd_image(p2).expect("Failed to load test_2x2.psd");
            assert_eq!(img.width(), 2);
            assert_eq!(img.height(), 2);
            let rgba = img.to_rgba8();
            // Pixel (0,0): Red (255, 0, 0, 255)
            assert_eq!(rgba.get_pixel(0, 0).0, [255, 0, 0, 255]);
            // Pixel (0,1): Green (0, 255, 0, 255)
            assert_eq!(rgba.get_pixel(0, 1).0, [0, 255, 0, 255]);
        }

        let prgba = Path::new(r"d:\PROJECT\media_dup_finder_wdx\test_rgba.psd");
        if prgba.exists() {
            let img = load_psd_image(prgba).expect("Failed to load test_rgba.psd");
            assert_eq!(img.width(), 2);
            assert_eq!(img.height(), 2);
            let rgba = img.to_rgba8();
            // Pixel (1,0): transparent alpha 0
            assert_eq!(rgba.get_pixel(1, 0).0, [255, 0, 0, 0]);
            // Pixel (0,1): semi-transparent alpha 128
            assert_eq!(rgba.get_pixel(0, 1).0, [0, 255, 0, 128]);
        }
    }
}
