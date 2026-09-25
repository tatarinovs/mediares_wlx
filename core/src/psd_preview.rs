//! PSD thumbnail / composite preview extraction.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Extract thumbnail / composite preview from PSD.
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

                if res_id == 0x0410 && payload_end <= res_data.len() && data_size > 28 {
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
