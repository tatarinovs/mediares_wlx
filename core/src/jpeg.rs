//! Minimal JPEG stream inspection used to locate embedded previews inside RAW/PSD files.

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
    fn lossless_jpeg_is_rejected() {
        let j = [0xFF, 0xD8, 0xFF, 0xC3, 0x00, 0x02, 0xFF, 0xD9];
        assert!(!is_decodable(&j));
    }
}
