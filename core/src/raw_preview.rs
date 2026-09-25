//! Extraction of embedded JPEG previews from RAW camera images (CR2, NEF, ARW, DNG, etc.)

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Extract embedded JPEG preview bytes from a RAW file.
pub fn extract_raw_preview(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len() as usize;
    if file_len < 1024 {
        return None;
    }

    // Try TIFF-based IFD parsing first (CR2, NEF, ARW, DNG, ORF, RW2, PEF)
    if let Some(jpeg) = extract_tiff_ifd_jpeg(&mut file, file_len) {
        return Some(jpeg);
    }

    // Fallback: search for largest JPEG stream (FF D8 FF ... FF D9)
    extract_largest_jpeg_stream(&mut file, file_len)
}

fn extract_tiff_ifd_jpeg(file: &mut File, file_len: usize) -> Option<Vec<u8>> {
    let _ = file.seek(SeekFrom::Start(0));
    let mut header = [0u8; 8];
    if file.read_exact(&mut header).is_err() {
        return None;
    }

    let is_le = match &header[0..4] {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };

    let read_u16 = |buf: &[u8]| -> u16 {
        if is_le {
            u16::from_le_bytes([buf[0], buf[1]])
        } else {
            u16::from_be_bytes([buf[0], buf[1]])
        }
    };

    let read_u32 = |buf: &[u8]| -> u32 {
        if is_le {
            u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
        } else {
            u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]])
        }
    };

    let mut ifd_offset = read_u32(&header[4..8]) as u64;
    let mut best_jpeg: Option<(u64, u32)> = None;

    // Traverse up to 8 IFDs
    for _ in 0..8 {
        if ifd_offset == 0 || ifd_offset + 2 >= file_len as u64 {
            break;
        }

        if file.seek(SeekFrom::Start(ifd_offset)).is_err() {
            break;
        }

        let mut num_entries_buf = [0u8; 2];
        if file.read_exact(&mut num_entries_buf).is_err() {
            break;
        }
        let num_entries = read_u16(&num_entries_buf) as usize;
        if num_entries > 500 {
            break;
        }

        let mut entries_data = vec![0u8; num_entries * 12];
        if file.read_exact(&mut entries_data).is_err() {
            break;
        }

        let mut jpeg_offset: Option<u32> = None;
        let mut jpeg_len: Option<u32> = None;
        let mut sub_ifd_offset: Option<u32> = None;

        for chunk in entries_data.chunks_exact(12) {
            let tag = read_u16(&chunk[0..2]);
            let value_offset = read_u32(&chunk[8..12]);

            match tag {
                0x0201 => jpeg_offset = Some(value_offset), // JPEGInterchangeFormat
                0x0202 => jpeg_len = Some(value_offset),    // JPEGInterchangeFormatLength
                0x014A => sub_ifd_offset = Some(value_offset), // SubIFDs
                _ => {}
            }
        }

        if let (Some(off), Some(len)) = (jpeg_offset, jpeg_len) {
            if off as usize + len as usize <= file_len && len > 50_000 {
                if let Some((_, best_len)) = best_jpeg {
                    if len > best_len {
                        best_jpeg = Some((off as u64, len));
                    }
                } else {
                    best_jpeg = Some((off as u64, len));
                }
            }
        }

        // Read next IFD offset
        let mut next_ifd_buf = [0u8; 4];
        if file.read_exact(&mut next_ifd_buf).is_err() {
            break;
        }
        let next_ifd = read_u32(&next_ifd_buf) as u64;

        if next_ifd != 0 {
            ifd_offset = next_ifd;
        } else if let Some(sub_off) = sub_ifd_offset.take() {
            ifd_offset = sub_off as u64;
        } else {
            break;
        }
    }

    if let Some((off, len)) = best_jpeg {
        let mut buf = vec![0u8; len as usize];
        if file.seek(SeekFrom::Start(off)).is_ok() && file.read_exact(&mut buf).is_ok() {
            if buf.starts_with(&[0xFF, 0xD8, 0xFF]) {
                return Some(buf);
            }
        }
    }

    None
}

fn extract_largest_jpeg_stream(file: &mut File, file_len: usize) -> Option<Vec<u8>> {
    // Read up to 16MB from file
    let max_read = file_len.min(16 * 1024 * 1024);
    let mut data = vec![0u8; max_read];
    let _ = file.seek(SeekFrom::Start(0));
    file.read_exact(&mut data).ok()?;

    let mut largest: Option<(usize, usize)> = None;
    let mut start_idx = 0;

    while let Some(soi) = memchr::memmem::find(&data[start_idx..], &[0xFF, 0xD8, 0xFF]) {
        let abs_soi = start_idx + soi;
        // Search for EOI (FF D9) after SOI
        if let Some(eoi) = memchr::memmem::find(&data[abs_soi + 2..], &[0xFF, 0xD9]) {
            let abs_eoi = abs_soi + 2 + eoi + 2;
            let len = abs_eoi - abs_soi;
            if len > 32_768 {
                if let Some((_, best_len)) = largest {
                    if len > best_len {
                        largest = Some((abs_soi, len));
                    }
                } else {
                    largest = Some((abs_soi, len));
                }
            }
            start_idx = abs_eoi;
        } else {
            break;
        }
    }

    if let Some((offset, length)) = largest {
        Some(data[offset..offset + length].to_vec())
    } else {
        None
    }
}
