//! The EXIF block of HEIF-family files (HEIC, AVIF): an item of type `Exif` located through the
//! `meta` box's item info (`iinf`) and item location (`iloc`) tables.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// Where the `meta` box is looked for; it sits right after `ftyp` in practice.
const META_SCAN_BYTES: u64 = 1024 * 1024;
const MAX_EXIF_BYTES: u64 = 1024 * 1024;

/// Whether the file starts with an ISO-BMFF `ftyp` box of a HEIF brand.
pub fn is_heif(head: &[u8]) -> bool {
    head.get(4..8) == Some(b"ftyp")
        && head.get(8..12).is_some_and(|brand| {
            [
                b"heic", b"heix", b"heim", b"heis", b"hevc", b"mif1", b"msf1", b"avif", b"avis",
            ]
            .iter()
            .any(|b| brand == *b)
        })
}

/// The TIFF payload (from its `II*\0` / `MM\0*` header on) of the file's `Exif` item.
pub fn read_exif_tiff(file: &mut File) -> Option<Vec<u8>> {
    let mut head = Vec::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.by_ref()
        .take(META_SCAN_BYTES)
        .read_to_end(&mut head)
        .ok()?;
    let meta = children(&head).find(|b| b.kind == *b"meta")?;
    let meta = meta.body.get(4..)?; // full box: version + flags
    let id = exif_item_id(children(meta).find(|b| b.kind == *b"iinf")?.body)?;
    let (offset, len) = item_extent(children(meta).find(|b| b.kind == *b"iloc")?.body, id)?;
    if !(4..=MAX_EXIF_BYTES).contains(&len) {
        return None;
    }
    let mut data = vec![0u8; len as usize];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut data).ok()?;
    // The item starts with the offset of the TIFF header past these four bytes (it skips an
    // optional `Exif\0\0` prefix).
    let skip = u32::from_be_bytes(data[..4].try_into().ok()?) as usize;
    let start = 4usize.checked_add(skip)?;
    (start < data.len()).then(|| data.split_off(start))
}

struct Atom<'a> {
    kind: [u8; 4],
    body: &'a [u8],
}

/// The boxes laid out one after another in `data`; stops at the first malformed one.
fn children(mut data: &[u8]) -> impl Iterator<Item = Atom<'_>> {
    std::iter::from_fn(move || {
        let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as u64;
        let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
        let (header, size) = match size {
            0 => (8, data.len() as u64),
            1 => (16, u64::from_be_bytes(data.get(8..16)?.try_into().ok()?)),
            n => (8, n),
        };
        let size = usize::try_from(size).ok().filter(|&s| s >= header)?;
        let body = data.get(header..size)?;
        data = &data[size..];
        Some(Atom { kind, body })
    })
}

/// Reads big-endian unsigned numbers of 0, 2, 4 or 8 bytes.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn uint(&mut self, bytes: usize) -> Option<u64> {
        let field = self.data.get(self.pos..self.pos.checked_add(bytes)?)?;
        self.pos += bytes;
        Some(field.iter().fold(0, |acc, &b| (acc << 8) | u64::from(b)))
    }
}

/// The id of the first `infe` entry of type `Exif`.
fn exif_item_id(iinf: &[u8]) -> Option<u32> {
    let version = *iinf.first()?;
    let entries = iinf.get(if version == 0 { 6 } else { 8 }..)?;
    children(entries)
        .filter(|b| b.kind == *b"infe")
        .find_map(|infe| {
            let version = *infe.body.first()?;
            let mut r = Reader {
                data: infe.body,
                pos: 4,
            };
            let id = match version {
                2 => r.uint(2)?,
                3 => r.uint(4)?,
                _ => return None,
            };
            r.uint(2)?; // protection index
            let kind = infe.body.get(r.pos..r.pos + 4)?;
            (kind == b"Exif").then_some(id as u32)
        })
}

/// File offset and length of item `id`'s first extent (file-offset construction only).
fn item_extent(iloc: &[u8], id: u32) -> Option<(u64, u64)> {
    let version = *iloc.first()?;
    let mut r = Reader { data: iloc, pos: 4 };
    let sizes = r.uint(2)?;
    let offset_size = (sizes >> 12) as usize;
    let length_size = (sizes >> 8 & 0xF) as usize;
    let base_size = (sizes >> 4 & 0xF) as usize;
    let index_size = if version >= 1 {
        (sizes & 0xF) as usize
    } else {
        0
    };
    let count = r.uint(if version < 2 { 2 } else { 4 })?;
    for _ in 0..count {
        let item = r.uint(if version < 2 { 2 } else { 4 })?;
        let method = if version >= 1 { r.uint(2)? & 0xF } else { 0 };
        r.uint(2)?; // data reference index
        let base = r.uint(base_size)?;
        let extents = r.uint(2)?;
        let mut first = None;
        for _ in 0..extents {
            r.uint(index_size)?;
            let offset = r.uint(offset_size)?;
            let length = r.uint(length_size)?;
            first.get_or_insert((offset, length));
        }
        if item == u64::from(id) {
            let (offset, length) = first?;
            return (method == 0).then_some((base.checked_add(offset)?, length));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn finds_exif_item() {
        let tiff = b"MM\0*\0\0\0\x08";
        let mut infe = vec![2, 0, 0, 0, 0, 7, 0, 0];
        infe.extend_from_slice(b"Exif");
        let mut iinf = vec![0, 0, 0, 0, 0, 1];
        iinf.extend(boxed(b"infe", &infe));

        let ftyp = boxed(b"ftyp", b"heic\0\0\0\0mif1heic");
        // iloc v0, offset/length 4 bytes, no base; one item, one extent; offset patched below.
        let mut iloc = vec![0, 0, 0, 0, 0x44, 0x00, 0, 1, 0, 7, 0, 0, 0, 1];
        iloc.extend_from_slice(&[0; 4]);
        iloc.extend_from_slice(&(6 + 4 + tiff.len() as u32).to_be_bytes());
        let meta_len = 8 + 4 + 8 + iinf.len() + 8 + iloc.len();
        let item_offset = (ftyp.len() + meta_len) as u32;
        iloc[14..18].copy_from_slice(&item_offset.to_be_bytes());

        let mut meta = vec![0, 0, 0, 0];
        meta.extend(boxed(b"iinf", &iinf));
        meta.extend(boxed(b"iloc", &iloc));
        let mut file_bytes = ftyp;
        file_bytes.extend(boxed(b"meta", &meta));
        file_bytes.extend_from_slice(&[0, 0, 0, 6]);
        file_bytes.extend_from_slice(b"Exif\0\0");
        file_bytes.extend_from_slice(tiff);
        assert!(is_heif(&file_bytes));

        let path = std::env::temp_dir().join(format!("mediares_{}_t.heic", std::process::id()));
        File::create(&path).unwrap().write_all(&file_bytes).unwrap();
        let got = read_exif_tiff(&mut File::open(&path).unwrap());
        std::fs::remove_file(&path).ok();
        assert_eq!(got.as_deref(), Some(&tiff[..]));
    }
}
