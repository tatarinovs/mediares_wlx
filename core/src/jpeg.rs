//! Minimal JPEG stream inspection: embedded previews inside RAW/PSD files, image size.

use std::io::{BufRead, Seek, SeekFrom};

/// What an `Exif` APP1 segment starts with, before its TIFF block.
pub const EXIF_HEADER: &[u8] = b"Exif\0\0";

/// Width and height from the SOF segment of a baseline/extended/progressive JPEG, reading only the
/// header segments: the `image` decoder loads the whole file just to report them.
pub fn read_dimensions(r: &mut (impl BufRead + Seek)) -> Option<(u32, u32)> {
    read_frame_header(r).map(|f| (f.width, f.height))
}

/// The SOF segment of a DCT JPEG: size, bits per sample and number of components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub width: u32,
    pub height: u32,
    pub precision: u8,
    pub components: u8,
}

/// [`read_dimensions`] with the sample precision and the component count.
pub fn read_frame_header(r: &mut (impl BufRead + Seek)) -> Option<FrameHeader> {
    let mut word = [0u8; 2];
    r.read_exact(&mut word).ok()?;
    if word != [0xFF, 0xD8] {
        return None;
    }
    loop {
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte).ok()?;
        if byte[0] != 0xFF {
            return None;
        }
        while byte[0] == 0xFF {
            r.read_exact(&mut byte).ok()?;
        }
        match byte[0] {
            0x01 | 0xD0..=0xD7 => continue,
            0xC0..=0xC2 => {
                // Length, precision, height, width, component count.
                let mut sof = [0u8; 8];
                r.read_exact(&mut sof).ok()?;
                let h = u16::from_be_bytes([sof[3], sof[4]]);
                let w = u16::from_be_bytes([sof[5], sof[6]]);
                // Height 0 means it comes later in a DNL segment.
                return (w > 0 && h > 0).then_some(FrameHeader {
                    width: w.into(),
                    height: h.into(),
                    precision: sof[2],
                    components: sof[7],
                });
            }
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xDA | 0xD9 => return None,
            _ => {
                r.read_exact(&mut word).ok()?;
                let len = u16::from_be_bytes(word);
                if len < 2 {
                    return None;
                }
                r.seek(SeekFrom::Current(i64::from(len) - 2)).ok()?;
            }
        }
    }
}

/// Length of the JPEG stream that starts at `data[0]` (SOI).
///
/// Walks marker segments instead of searching for the first `FF D9`, so EXIF thumbnails
/// nested inside APP1 do not terminate the stream early.
pub fn stream_len(data: &[u8]) -> Option<usize> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    loop {
        let (marker, next) = read_marker(data, i)?;
        i = next;
        match marker {
            0xD9 => return Some(i),
            0x01 | 0xD0..=0xD7 => {}
            0xDA => {
                i = skip_segment(data, i)?;
                // Entropy-coded data: FF 00 is stuffing, FF D0..D7 are restart markers.
                loop {
                    i += memchr::memchr(0xFF, data.get(i..)?)?;
                    match *data.get(i + 1)? {
                        0x00 | 0xD0..=0xD7 => i += 2,
                        0xFF => i += 1,
                        _ => break,
                    }
                }
            }
            _ => i = skip_segment(data, i)?,
        }
    }
}

/// True if the stream is baseline/extended/progressive DCT — i.e. something `image` can decode
/// (lossless JPEG used for DNG/CR2 raw data is rejected).
pub fn is_decodable(data: &[u8]) -> bool {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return false;
    }
    let mut i = 2;
    while let Some((marker, next)) = read_marker(data, i) {
        match marker {
            0xC0..=0xC2 => return true,
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xDA | 0xD9 => return false,
            0x01 | 0xD0..=0xD7 => i = next,
            _ => match skip_segment(data, next) {
                Some(n) => i = n,
                None => return false,
            },
        }
    }
    false
}

/// Finds the largest complete, decodable JPEG stream (at least `min_len` bytes) inside `data`.
pub fn find_largest(data: &[u8], min_len: usize) -> Option<&[u8]> {
    let mut best: Option<&[u8]> = None;
    let mut pos = 0;
    while let Some(found) = memchr::memmem::find(&data[pos..], &[0xFF, 0xD8, 0xFF]) {
        let start = pos + found;
        match stream_len(&data[start..]) {
            Some(len) => {
                let jpeg = &data[start..start + len];
                if len >= min_len && best.is_none_or(|b| len > b.len()) && is_decodable(jpeg) {
                    best = Some(jpeg);
                }
                pos = start + len;
            }
            None => pos = start + 3,
        }
    }
    best
}

/// The JPEG with its `Exif` APP1 segments replaced by one holding `tiff`, placed right after SOI
/// as the EXIF standard wants. The image data is copied untouched. `None` if the stream is not a
/// JPEG or `tiff` does not fit into one segment.
pub fn with_exif(jpeg: &[u8], tiff: &[u8]) -> Option<Vec<u8>> {
    replace_metadata(jpeg, tiff, |marker, payload| {
        marker == 0xE1 && payload.starts_with(EXIF_HEADER)
    })
}

/// [`with_exif`] that also drops the other descriptive metadata: XMP and any other APP1, IPTC /
/// Photoshop (APP13), Ducky (APP12) and comments. What decoding needs stays: JFIF, the ICC
/// profile, the Adobe color transform, MPF and the like.
pub fn with_only_exif(jpeg: &[u8], tiff: &[u8]) -> Option<Vec<u8>> {
    replace_metadata(jpeg, tiff, |marker, _| {
        matches!(marker, 0xE1 | 0xEC | 0xED | 0xFE)
    })
}

/// The JPEG with a new `Exif` segment holding `tiff` and without the header segments `drop`
/// (marker, payload) picks.
fn replace_metadata(jpeg: &[u8], tiff: &[u8], drop: impl Fn(u8, &[u8]) -> bool) -> Option<Vec<u8>> {
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let segment_len = u16::try_from(2 + EXIF_HEADER.len() + tiff.len()).ok()?;
    let mut out = Vec::with_capacity(jpeg.len() + tiff.len() + 10);
    out.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE1]);
    out.extend_from_slice(&segment_len.to_be_bytes());
    out.extend_from_slice(EXIF_HEADER);
    out.extend_from_slice(tiff);

    // Header segments up to the first scan; everything from there on is copied as is.
    let mut i = 2;
    loop {
        let (marker, next) = read_marker(jpeg, i)?;
        match marker {
            0x01 | 0xD0..=0xD7 => out.extend_from_slice(&jpeg[i..next]),
            0xDA | 0xD9 => {
                out.extend_from_slice(&jpeg[i..]);
                return Some(out);
            }
            _ => {
                let end = skip_segment(jpeg, next)?;
                if !drop(marker, &jpeg[next + 2..end]) {
                    out.extend_from_slice(&jpeg[i..end]);
                }
                i = end;
                continue;
            }
        }
        i = next;
    }
}

/// Where the TIFF payload of the first `Exif` APP1 segment lies in `jpeg` (header segments only).
pub fn exif_range(jpeg: &[u8]) -> Option<std::ops::Range<usize>> {
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    loop {
        let (marker, next) = read_marker(jpeg, i)?;
        match marker {
            0x01 | 0xD0..=0xD7 => i = next,
            0xDA | 0xD9 => return None,
            _ => {
                let end = skip_segment(jpeg, next)?;
                let payload = next + 2..end;
                if marker == 0xE1 && jpeg[payload.clone()].starts_with(EXIF_HEADER) {
                    return Some(payload.start + EXIF_HEADER.len()..end);
                }
                i = end;
            }
        }
    }
}

/// Reads the marker at `i` (skipping fill bytes); returns the marker code and the index after it.
fn read_marker(data: &[u8], mut i: usize) -> Option<(u8, usize)> {
    if *data.get(i)? != 0xFF {
        return None;
    }
    while *data.get(i)? == 0xFF {
        i += 1;
    }
    Some((data[i], i + 1))
}

/// Skips a length-prefixed segment whose length field starts at `i`.
fn skip_segment(data: &[u8], i: usize) -> Option<usize> {
    let len = u16::from_be_bytes([*data.get(i)?, *data.get(i + 1)?]) as usize;
    if len < 2 || i + len > data.len() {
        return None;
    }
    Some(i + len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SOI, APP1 with a nested tiny "thumbnail" SOI..EOI, SOF0, SOS, scan data, EOI.
    fn sample_jpeg() -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        let thumb = [0xFF, 0xD8, 0xFF, 0xD9];
        j.extend_from_slice(&[0xFF, 0xE1, 0x00, (2 + thumb.len()) as u8]);
        j.extend_from_slice(&thumb);
        j.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x03, 0x08]);
        j.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        j.extend_from_slice(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56]);
        j.extend_from_slice(&[0xFF, 0xD9]);
        j
    }

    #[test]
    fn nested_thumbnail_does_not_truncate_stream() {
        let j = sample_jpeg();
        assert_eq!(stream_len(&j), Some(j.len()));
        assert!(is_decodable(&j));
    }

    #[test]
    fn finds_embedded_stream() {
        let j = sample_jpeg();
        let mut blob = vec![0u8; 100];
        blob.extend_from_slice(&j);
        blob.extend_from_slice(&[1, 2, 3]);
        assert_eq!(find_largest(&blob, 1), Some(&j[..]));
    }

    #[test]
    fn dimensions_from_sof_past_app_segments() {
        let mut j = vec![0xFF, 0xD8];
        j.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x06, 0xFF, 0xC0, 0x00, 0x00]); // APP1, fake SOF inside
        j.extend_from_slice(&[0xFF, 0xFF, 0xC2, 0x00, 0x0B, 0x08, 0x0F, 0xA0, 0x17, 0x70]);
        j.extend_from_slice(&[0x03, 0x01, 0x22, 0x00]);
        assert_eq!(
            read_dimensions(&mut std::io::Cursor::new(&j)),
            Some((6000, 4000))
        );

        let lossless = [
            0xFF, 0xD8, 0xFF, 0xC3, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x10,
        ];
        assert_eq!(read_dimensions(&mut std::io::Cursor::new(&lossless)), None);
        assert_eq!(read_dimensions(&mut std::io::Cursor::new(b"GIF89a")), None);
    }

    #[test]
    fn lossless_jpeg_is_rejected() {
        let j = [0xFF, 0xD8, 0xFF, 0xC3, 0x00, 0x02, 0xFF, 0xD9];
        assert!(!is_decodable(&j));
    }

    #[test]
    fn exif_is_replaced_and_image_data_kept() {
        let mut j = vec![0xFF, 0xD8];
        j.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0xAA, 0xBB]); // APP0
        j.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x0A]); // old Exif
        j.extend_from_slice(b"Exif\0\0MM");
        j.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xD9]);

        let out = with_exif(&j, b"II*\0TIFF").unwrap();
        let mut expected = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x10];
        expected.extend_from_slice(b"Exif\0\0II*\0TIFF");
        expected.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0xAA, 0xBB]);
        expected.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xD9]);
        assert_eq!(out, expected);
        assert_eq!(with_exif(b"not a jpeg", b""), None);
    }

    #[test]
    fn only_exif_drops_xmp_iptc_and_comments() {
        let mut j = vec![0xFF, 0xD8];
        j.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0xAA, 0xBB]); // APP0 kept
        j.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x06, b'h', b't', b't', b'p']); // XMP
        j.extend_from_slice(&[0xFF, 0xE2, 0x00, 0x04, 0xCC, 0xDD]); // ICC kept
        j.extend_from_slice(&[0xFF, 0xED, 0x00, 0x03, 0x01]); // IPTC
        j.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x03, b'c']); // comment
        j.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xD9]);
        let out = with_only_exif(&j, b"II*\0TIFF").unwrap();
        let mut expected = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x10];
        expected.extend_from_slice(b"Exif\0\0II*\0TIFF");
        expected.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0xAA, 0xBB]);
        expected.extend_from_slice(&[0xFF, 0xE2, 0x00, 0x04, 0xCC, 0xDD]);
        expected.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xD9]);
        assert_eq!(out, expected);
    }
}
