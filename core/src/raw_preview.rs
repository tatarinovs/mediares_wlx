//! Extraction of embedded JPEG previews from RAW camera images (CR2, NEF, ARW, DNG, ...).

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::exif::Endian;
use crate::jpeg;

const MIN_PREVIEW_LEN: usize = 32 * 1024;
const MAX_IFDS: usize = 32;
const MAX_IFD_ENTRIES: usize = 1000;
const MAX_SUB_IFDS: usize = 16;
/// Enough to reach the SOF marker past large APP segments when validating a candidate.
const HEADER_PROBE_LEN: usize = 256 * 1024;
const SIGNATURE_SCAN_BYTES: u64 = 16 * 1024 * 1024;

/// Extract the largest decodable embedded JPEG preview from a RAW file.
pub fn extract_raw_preview(path: &Path) -> Option<Vec<u8>> {
    extract_raw_preview_for(path, None)
}

/// The smallest embedded preview whose longer side is at least `side` (the largest if none is
/// that big); with `None`, the largest. A thumbnail then reads and decodes a 1616 px preview
/// instead of the full-size one.
pub fn extract_raw_preview_for(path: &Path, side: Option<u32>) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    if file_len < 1024 {
        return None;
    }
    tiff_preview(&mut file, file_len, side).or_else(|| signature_preview(&mut file))
}

/// Structured path: walk IFD0, its chain and SubIFDs collecting JPEG candidates
/// (`JPEGInterchangeFormat` or single-strip JPEG-compressed images as in CR2).
fn tiff_preview(file: &mut File, file_len: u64, side: Option<u32>) -> Option<Vec<u8>> {
    let mut header = [0u8; 8];
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_exact(&mut header).ok()?;
    let le = match &header[0..4] {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let rd = Endian(le);

    let mut candidates: Vec<(u64, u64)> = Vec::new();
    let mut queue = VecDeque::from([rd.u32(&header[4..8]) as u64]);
    let mut visited = Vec::new();

    while let Some(offset) = queue.pop_front() {
        if offset == 0
            || offset + 2 > file_len
            || visited.contains(&offset)
            || visited.len() >= MAX_IFDS
        {
            continue;
        }
        visited.push(offset);

        let Some(ifd) = read_ifd(file, offset, rd) else {
            continue;
        };
        let pair =
            |a: Option<u64>, b: Option<u64>| a.zip(b).filter(|&(o, l)| l > 0 && o + l <= file_len);

        if let Some(c) = pair(ifd.jpeg_offset, ifd.jpeg_length) {
            candidates.push(c);
        }
        if matches!(ifd.compression, Some(6 | 7)) {
            if let Some(c) = pair(ifd.strip_offset, ifd.strip_length) {
                candidates.push(c);
            }
        }
        queue.extend(ifd.sub_ifds);
        queue.push_back(ifd.next);
    }

    candidates.sort_by_key(|&(_, len)| std::cmp::Reverse(len));
    candidates.dedup();
    candidates.retain(|&(_, len)| len as usize >= MIN_PREVIEW_LEN);
    if let Some(side) = side {
        // Smallest first; one too small for `side` is passed over.
        let big_enough = |(w, h): (u32, u32)| w.max(h) >= side;
        if let Some(preview) = candidates
            .iter()
            .rev()
            .find_map(|&(off, len)| read_candidate(file, off, len as usize, Some(&big_enough)))
        {
            return Some(preview);
        }
    }
    candidates
        .into_iter()
        .find_map(|(off, len)| read_candidate(file, off, len as usize, None))
}

/// The JPEG at `offset`, if it is decodable (and its size passes `accept`).
fn read_candidate(
    file: &mut File,
    offset: u64,
    len: usize,
    accept: Option<&dyn Fn((u32, u32)) -> bool>,
) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0u8; len.min(HEADER_PROBE_LEN)];
    file.read_exact(&mut buf).ok()?;
    if !jpeg::is_decodable(&buf) {
        return None;
    }
    if let Some(accept) = accept {
        let size = jpeg::read_dimensions(&mut std::io::Cursor::new(&buf[..]))?;
        if !accept(size) {
            return None;
        }
    }
    buf.resize(len, 0);
    file.read_exact(&mut buf[HEADER_PROBE_LEN.min(len)..])
        .ok()?;
    Some(buf)
}

/// Fallback: scan the head of the file for the largest complete JPEG stream (e.g. CR3, RAF).
fn signature_preview(file: &mut File) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(SIGNATURE_SCAN_BYTES)
        .read_to_end(&mut data)
        .ok()?;
    jpeg::find_largest(&data, MIN_PREVIEW_LEN).map(<[u8]>::to_vec)
}

/// First value of a SHORT/LONG entry stored inline in the 4-byte value field.
fn inline_uint(rd: Endian, typ: u16, value: &[u8]) -> Option<u64> {
    match typ {
        3 => Some(rd.u16(value) as u64),
        4 | 13 => Some(rd.u32(value) as u64),
        _ => None,
    }
}

#[derive(Default)]
struct Ifd {
    jpeg_offset: Option<u64>,
    jpeg_length: Option<u64>,
    strip_offset: Option<u64>,
    strip_length: Option<u64>,
    compression: Option<u64>,
    sub_ifds: Vec<u64>,
    next: u64,
}

fn read_ifd(file: &mut File, offset: u64, rd: Endian) -> Option<Ifd> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut count_buf = [0u8; 2];
    file.read_exact(&mut count_buf).ok()?;
    let count = rd.u16(&count_buf) as usize;
    if count > MAX_IFD_ENTRIES {
        return None;
    }
    let mut entries = vec![0u8; count * 12 + 4];
    file.read_exact(&mut entries).ok()?;

    let mut ifd = Ifd {
        next: rd.u32(&entries[count * 12..]) as u64,
        ..Ifd::default()
    };
    let mut sub_ifd_array = None;

    for e in entries[..count * 12].chunks_exact(12) {
        let tag = rd.u16(&e[0..2]);
        let typ = rd.u16(&e[2..4]);
        let n = rd.u32(&e[4..8]);
        let value = &e[8..12];
        // Multi-valued strip tables mean a tiled/multi-strip image, not a single JPEG.
        let single = || {
            if n == 1 {
                inline_uint(rd, typ, value)
            } else {
                None
            }
        };
        match tag {
            0x0103 => ifd.compression = single(),
            0x0111 => ifd.strip_offset = single(),
            0x0117 => ifd.strip_length = single(),
            0x0201 => ifd.jpeg_offset = single(),
            0x0202 => ifd.jpeg_length = single(),
            0x014A if n == 1 => ifd.sub_ifds.extend(inline_uint(rd, typ, value)),
            0x014A if n > 1 => {
                sub_ifd_array = Some((rd.u32(value) as u64, (n as usize).min(MAX_SUB_IFDS)))
            }
            _ => {}
        }
    }

    if let Some((array_offset, n)) = sub_ifd_array {
        let mut buf = vec![0u8; n * 4];
        if file.seek(SeekFrom::Start(array_offset)).is_ok() && file.read_exact(&mut buf).is_ok() {
            ifd.sub_ifds
                .extend(buf.chunks_exact(4).map(|c| rd.u32(c) as u64));
        }
    }
    Some(ifd)
}
