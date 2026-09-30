//! Lossless rotation of JPEG files: only the EXIF Orientation tag changes, the compressed image
//! data is left byte for byte as it was.

use std::fs::OpenOptions;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use crate::exif::{build_exif, Endian, ExifInfo};

const TAG_ORIENTATION: u16 = 0x0112;
const TYPE_SHORT: u16 = 3;

/// EXIF orientation codes as (mirrored, quarter turns clockwise): the stored picture is mirrored
/// left to right first, then turned.
const CODES: [[u16; 4]; 2] = [[1, 6, 3, 8], [2, 7, 4, 5]];

/// The orientation that shows the picture turned by `quarter_turns` clockwise more than
/// `orientation` does (unknown codes count as 1).
pub fn turned(orientation: u16, quarter_turns: u8) -> u16 {
    let (mirror, turns) = CODES
        .iter()
        .enumerate()
        .find_map(|(m, row)| row.iter().position(|&c| c == orientation).map(|k| (m, k)))
        .unwrap_or((0, 0));
    CODES[mirror][(turns + usize::from(quarter_turns)) % 4]
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

/// The TIFF block with IFD0's Orientation set to `orientation`. An existing entry is changed in
/// place (same length); otherwise IFD0 is copied to the end with the entry added and the header
/// pointed at the copy — all other data keeps its offsets.
fn tiff_with_orientation(tiff: &[u8], orientation: u16) -> Option<Vec<u8>> {
    let rd = match tiff.get(..4)? {
        b"II*\0" => Endian(true),
        b"MM\0*" => Endian(false),
        _ => return None,
    };
    let put16 = |v: u16| {
        if rd.0 {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    };
    let put32 = |v: u32| {
        if rd.0 {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    };
    let ifd0 = rd.u32(tiff.get(4..8)?) as usize;
    let count = rd.u16(tiff.get(ifd0..ifd0 + 2)?) as usize;
    let entries_end = ifd0 + 2 + count * 12;
    let entries = tiff.get(ifd0 + 2..entries_end)?;
    let next_ifd = tiff.get(entries_end..entries_end + 4)?;

    let mut out = tiff.to_vec();
    let tag_of = |e: &[u8]| rd.u16(&e[..2]);
    if let Some(i) = entries
        .chunks_exact(12)
        .position(|e| tag_of(e) == TAG_ORIENTATION)
    {
        let at = ifd0 + 2 + i * 12;
        out[at + 2..at + 12].copy_from_slice(
            &[
                &put16(TYPE_SHORT)[..],
                &put32(1),
                &put16(orientation),
                &[0, 0],
            ]
            .concat(),
        );
        return Some(out);
    }

    let mut entry = Vec::with_capacity(12);
    entry.extend_from_slice(&put16(TAG_ORIENTATION));
    entry.extend_from_slice(&put16(TYPE_SHORT));
    entry.extend_from_slice(&put32(1));
    entry.extend_from_slice(&put16(orientation));
    entry.extend_from_slice(&[0, 0]);
    // Entries stay sorted by tag, as TIFF requires.
    let insert_at = entries
        .chunks_exact(12)
        .position(|e| tag_of(e) > TAG_ORIENTATION)
        .unwrap_or(count);

    if out.len() % 2 == 1 {
        out.push(0); // IFDs start on a word boundary
    }
    let new_ifd0 = u32::try_from(out.len()).ok()?;
    out.extend_from_slice(&put16(u16::try_from(count + 1).ok()?));
    out.extend_from_slice(&entries[..insert_at * 12]);
    out.extend_from_slice(&entry);
    out.extend_from_slice(&entries[insert_at * 12..]);
    out.extend_from_slice(next_ifd);
    out[4..8].copy_from_slice(&put32(new_ifd0));
    Some(out)
}

/// How a JPEG gets its new orientation.
#[derive(Debug)]
enum Rewrite {
    /// The EXIF payload at `offset` is replaced by one of the same length.
    Patch { offset: usize, payload: Vec<u8> },
    /// The whole file is replaced (EXIF added or grown).
    Whole(Vec<u8>),
}

fn rewrite(jpeg: &[u8], orientation: u16) -> io::Result<Rewrite> {
    if !jpeg.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Err(invalid("not a JPEG file"));
    }
    let Some(range) = crate::jpeg::exif_range(jpeg) else {
        let tiff = build_exif(&ExifInfo::default(), orientation);
        return crate::jpeg::with_exif(jpeg, &tiff)
            .map(Rewrite::Whole)
            .ok_or_else(|| invalid("unexpected JPEG structure"));
    };
    let tiff = tiff_with_orientation(&jpeg[range.clone()], orientation)
        .ok_or_else(|| invalid("damaged EXIF block"))?;
    if tiff.len() == range.len() {
        return Ok(Rewrite::Patch {
            offset: range.start,
            payload: tiff,
        });
    }
    crate::jpeg::with_exif(jpeg, &tiff)
        .map(Rewrite::Whole)
        .ok_or_else(|| invalid("EXIF block too large"))
}

/// Sets the EXIF orientation of the JPEG at `path`. The file is overwritten in place, so its
/// creation time, attributes and permissions stay; when the whole file has to be rewritten, a
/// full copy is kept next to it until the write has succeeded.
pub fn set_jpeg_orientation(path: &Path, orientation: u16) -> io::Result<()> {
    let jpeg = std::fs::read(path)?;
    match rewrite(&jpeg, orientation)? {
        Rewrite::Patch { offset, payload } => {
            let mut file = OpenOptions::new().write(true).open(path)?;
            file.seek(SeekFrom::Start(offset as u64))?;
            file.write_all(&payload)?;
            file.sync_all()
        }
        Rewrite::Whole(bytes) => {
            let mut backup = path.as_os_str().to_owned();
            backup.push(".mediares-backup");
            std::fs::write(&backup, &jpeg)?;
            let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::remove_file(&backup)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turning_composes_with_mirroring() {
        assert_eq!(turned(1, 1), 6);
        assert_eq!(turned(6, 1), 3);
        assert_eq!(turned(8, 1), 1);
        assert_eq!(turned(1, 3), 8);
        assert_eq!(turned(2, 1), 7);
        assert_eq!(turned(5, 1), 2);
        assert_eq!(turned(0, 2), 3);
        for o in 1..=8 {
            assert_eq!(turned(turned(o, 1), 3), o);
        }
    }

    fn jpeg_with(tiff: Option<&[u8]>) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        j.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0xAA, 0xBB]); // APP0
        if let Some(tiff) = tiff {
            j.extend_from_slice(&[0xFF, 0xE1]);
            j.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
            j.extend_from_slice(b"Exif\0\0");
            j.extend_from_slice(tiff);
        }
        j.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xD9]);
        j
    }

    fn orientation_of(jpeg: &[u8]) -> Option<u16> {
        let path = std::env::temp_dir().join(format!(
            "mediares_{}_{}.jpg",
            std::process::id(),
            jpeg.len()
        ));
        std::fs::write(&path, jpeg).unwrap();
        let o = crate::exif::read_orientation(&path);
        std::fs::remove_file(&path).ok();
        o
    }

    #[test]
    fn existing_tag_is_patched_in_place() {
        let info = ExifInfo {
            make: Some("Cam".into()),
            ..Default::default()
        };
        let jpeg = jpeg_with(Some(&build_exif(&info, 1)));
        let Rewrite::Patch { offset, payload } = rewrite(&jpeg, 6).unwrap() else {
            panic!("expected an in-place patch");
        };
        let mut patched = jpeg.clone();
        patched[offset..offset + payload.len()].copy_from_slice(&payload);
        assert_eq!(patched.len(), jpeg.len());
        assert_eq!(orientation_of(&patched), Some(6));
        assert!(patched.ends_with(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xD9]));
    }

    #[test]
    fn missing_tag_is_added_and_other_fields_kept() {
        // Big-endian TIFF, IFD0 with only Make (tag 0x010F, ASCII "Cam\0" inline).
        let mut tiff = b"MM\0*\0\0\0\x08\0\x01".to_vec();
        tiff.extend_from_slice(&[0x01, 0x0F, 0, 2, 0, 0, 0, 4]);
        tiff.extend_from_slice(b"Cam\0");
        tiff.extend_from_slice(&[0, 0, 0, 0]);
        let Rewrite::Whole(out) = rewrite(&jpeg_with(Some(&tiff)), 8).unwrap() else {
            panic!("expected a rewritten file");
        };
        assert_eq!(orientation_of(&out), Some(8));
        let path = std::env::temp_dir().join(format!("mediares_{}_make.jpg", std::process::id()));
        std::fs::write(&path, &out).unwrap();
        let make = crate::exif::read_exif(&path).and_then(|e| e.make);
        std::fs::remove_file(&path).ok();
        assert_eq!(make.as_deref(), Some("Cam"));
    }

    #[test]
    fn exif_is_created_when_absent() {
        let Rewrite::Whole(out) = rewrite(&jpeg_with(None), 3).unwrap() else {
            panic!("expected a rewritten file");
        };
        assert_eq!(orientation_of(&out), Some(3));
        assert!(rewrite(b"GIF89a", 3).is_err());
    }
}
