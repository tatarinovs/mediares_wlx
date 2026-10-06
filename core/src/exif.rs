//! EXIF metadata reader for JPEG, TIFF-based (TIFF/RAW) and HEIF-family (HEIC/AVIF) files.
//!
//! The metadata block is read into memory once and parsed from a slice, so every access is
//! bounds-checked and malformed offsets simply yield missing fields.

use std::cell::Cell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// TIFF-based files keep IFD0 and the Exif IFD near the start: this much is read first...
const TIFF_HEAD_BYTES: u64 = 64 * 1024;
/// ...and this much if their values lie further on.
const TIFF_SCAN_BYTES: u64 = 1024 * 1024;
const MAX_STRING_LEN: usize = 4096;

const TYPE_BYTE: u16 = 1;
const TYPE_ASCII: u16 = 2;
const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_RATIONAL: u16 = 5;
const TYPE_UNDEFINED: u16 = 7;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExifInfo {
    pub make: Option<String>,
    pub model: Option<String>,
    pub date_time: Option<String>,
    pub date_time_original: Option<String>,
    /// Formatted, e.g. `1/250` or `2.5`.
    pub exposure_time: Option<String>,
    pub f_number: Option<f64>,
    pub iso: Option<u32>,
    pub focal_length: Option<f64>,
    pub focal_length_35mm: Option<u32>,
    pub lens_model: Option<String>,
    pub flash_fired: Option<bool>,
    pub orientation: Option<u16>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub software: Option<String>,
    /// Decimal degrees, south negative.
    pub gps_latitude: Option<f64>,
    /// Decimal degrees, west negative.
    pub gps_longitude: Option<f64>,
}

impl ExifInfo {
    /// When the photo was taken (`DateTimeOriginal`), else when it was last written (`DateTime`).
    pub fn taken(&self) -> Option<&str> {
        self.date_time_original
            .as_deref()
            .or(self.date_time.as_deref())
    }

    /// Latitude and longitude in degrees (south and west negative).
    pub fn gps(&self) -> Option<(f64, f64)> {
        self.gps_latitude.zip(self.gps_longitude)
    }
}

/// Camera clock time from an EXIF `YYYY:MM:DD HH:MM:SS` string (no time zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExifDateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Parses `YYYY:MM:DD HH:MM:SS`; also accepts `-` as the date separator and a missing time part.
/// Unset dates (`0000:00:00 00:00:00`, blanks) and out-of-range values yield `None`.
pub fn parse_exif_datetime(s: &str) -> Option<ExifDateTime> {
    let s = s.trim();
    let (date, time) = s.split_once(' ').unwrap_or((s, "00:00:00"));
    let num = |part: Option<&str>| part?.trim().parse::<u16>().ok();
    let mut d = date.split([':', '-']);
    let mut t = time.trim().split(':');
    let (year, month, day) = (num(d.next())?, num(d.next())?, num(d.next())?);
    let (hour, minute) = (num(t.next())?, num(t.next())?);
    let second = num(t.next()).unwrap_or(0);
    let valid = (1..=9999).contains(&year)
        && (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour < 24
        && minute < 60
        && second < 60;
    valid.then_some(ExifDateTime {
        year,
        month: month as u8,
        day: day as u8,
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
    })
}

pub fn read_exif(path: &Path) -> Option<ExifInfo> {
    let mut file = File::open(path).ok()?;
    let mut head = [0u8; 12];
    let head_len = file.read(&mut head).ok()?;
    let head = &head[..head_len];
    let magic = head.get(..4)?;

    if crate::heif::is_heif(head) {
        // The container's own rotation (`irot` / `imir`) is authoritative and already applied by
        // the decoder; the EXIF tag only mirrors it.
        let mut info = parse_tiff(&crate::heif::read_exif_tiff(&mut file)?)?;
        info.orientation = None;
        return Some(info);
    }
    let block = if magic.starts_with(&[0xFF, 0xD8]) {
        read_jpeg_app1(&mut file)?
    } else if magic == b"II*\0" || magic == b"MM\0*" {
        // The head usually holds all the metadata (a Sony ARW's ends at about 43 KB); only if an
        // offset points past it is the longer scan read.
        let read_head = |file: &mut File, len: u64| {
            let mut data = Vec::new();
            file.seek(SeekFrom::Start(0)).ok()?;
            file.take(len).read_to_end(&mut data).ok()?;
            Some(data)
        };
        let head = read_head(&mut file, TIFF_HEAD_BYTES)?;
        let (info, cut_short) = parse_tiff_checked(&head);
        if !cut_short || head.len() < TIFF_HEAD_BYTES as usize {
            return info;
        }
        read_head(&mut file, TIFF_SCAN_BYTES)?
    } else {
        return None;
    };
    parse_tiff(&block)
}

pub fn read_orientation(path: &Path) -> Option<u16> {
    read_exif(path)?.orientation
}

/// Returns the TIFF payload of the first `Exif\0\0` APP1 segment of a JPEG file.
fn read_jpeg_app1(file: &mut File) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(2)).ok()?;
    let mut byte = [0u8; 1];
    loop {
        file.read_exact(&mut byte).ok()?;
        if byte[0] != 0xFF {
            return None;
        }
        // Skip fill bytes.
        while byte[0] == 0xFF {
            file.read_exact(&mut byte).ok()?;
        }
        let marker = byte[0];
        match marker {
            0xDA | 0xD9 => return None,
            0x01 | 0xD0..=0xD7 => continue,
            _ => {}
        }

        let mut len_buf = [0u8; 2];
        file.read_exact(&mut len_buf).ok()?;
        let payload_len = (u16::from_be_bytes(len_buf) as usize).checked_sub(2)?;

        let header = crate::jpeg::EXIF_HEADER;
        if marker == 0xE1 && payload_len > header.len() {
            let mut payload = vec![0u8; payload_len];
            file.read_exact(&mut payload).ok()?;
            if payload.starts_with(header) {
                payload.drain(..header.len());
                return Some(payload);
            }
        } else {
            file.seek(SeekFrom::Current(payload_len as i64)).ok()?;
        }
    }
}

/// Parses a TIFF structure (`II*\0` / `MM\0*` header) from memory.
fn parse_tiff(data: &[u8]) -> Option<ExifInfo> {
    parse_tiff_checked(data).0
}

/// [`parse_tiff`], and whether something it looked for lay past the end of `data` (a file's
/// head, which may then be read further).
fn parse_tiff_checked(data: &[u8]) -> (Option<ExifInfo>, bool) {
    let le = match data.get(..4) {
        Some(b"II*\0") => true,
        Some(b"MM\0*") => false,
        _ => return (None, false),
    };
    let tiff = Tiff {
        data,
        rd: Endian(le),
        cut_short: Cell::new(false),
    };
    let info = tiff.parse();
    (info, tiff.cut_short.get())
}

/// Offsets of the Exif and GPS IFDs referenced from IFD0.
#[derive(Default)]
struct SubIfds {
    exif: Option<usize>,
    gps: Option<usize>,
}

/// Byte order of a TIFF structure: little-endian (`II`) when true.
#[derive(Clone, Copy)]
pub(crate) struct Endian(pub bool);

impl Endian {
    pub fn u16(self, b: &[u8]) -> u16 {
        let v = [b[0], b[1]];
        if self.0 {
            u16::from_le_bytes(v)
        } else {
            u16::from_be_bytes(v)
        }
    }

    pub fn u32(self, b: &[u8]) -> u32 {
        let v = [b[0], b[1], b[2], b[3]];
        if self.0 {
            u32::from_le_bytes(v)
        } else {
            u32::from_be_bytes(v)
        }
    }
}

struct Tiff<'a> {
    data: &'a [u8],
    rd: Endian,
    /// A read went past the end of `data`.
    cut_short: Cell<bool>,
}

struct Entry<'a> {
    tag: u16,
    typ: u16,
    count: u32,
    value: &'a [u8],
}

impl<'a> Tiff<'a> {
    /// IFD0, then the Exif and GPS IFDs it points to.
    fn parse(&self) -> Option<ExifInfo> {
        let ifd0 = self.u32(4)? as usize;
        let mut info = ExifInfo::default();
        let mut subs = SubIfds::default();
        self.parse_ifd(ifd0, &mut info, &mut subs);
        if let Some(offset) = subs.exif {
            self.parse_ifd(offset, &mut info, &mut SubIfds::default());
        }
        if let Some(offset) = subs.gps {
            self.parse_gps_ifd(offset, &mut info);
        }
        Some(info)
    }

    fn bytes(&self, offset: usize, len: usize) -> Option<&'a [u8]> {
        let end = offset.checked_add(len)?;
        if end > self.data.len() {
            self.cut_short.set(true);
        }
        self.data.get(offset..end)
    }

    fn u32(&self, offset: usize) -> Option<u32> {
        self.bytes(offset, 4).map(|b| self.rd.u32(b))
    }

    /// Decodes the 12-byte IFD entry at `offset`; values up to 4 bytes are stored inline.
    fn entry(&self, offset: usize) -> Option<Entry<'a>> {
        let raw = self.bytes(offset, 12)?;
        let tag = self.rd.u16(&raw[0..2]);
        let typ = self.rd.u16(&raw[2..4]);
        let count = self.rd.u32(&raw[4..8]);
        let unit = match typ {
            TYPE_BYTE | TYPE_ASCII | TYPE_UNDEFINED => 1,
            TYPE_SHORT => 2,
            TYPE_LONG => 4,
            TYPE_RATIONAL => 8,
            _ => {
                return Some(Entry {
                    tag,
                    typ,
                    count,
                    value: &[],
                })
            }
        };
        let size = (count as usize).checked_mul(unit)?;
        let value = if size <= 4 {
            &raw[8..8 + size]
        } else {
            self.bytes(self.rd.u32(&raw[8..12]) as usize, size)?
        };
        Some(Entry {
            tag,
            typ,
            count,
            value,
        })
    }

    fn uint(&self, e: &Entry) -> Option<u32> {
        match e.typ {
            TYPE_BYTE => e.value.first().map(|&b| b as u32),
            TYPE_SHORT if e.value.len() >= 2 => Some(self.rd.u16(e.value) as u32),
            TYPE_LONG if e.value.len() >= 4 => Some(self.rd.u32(e.value)),
            _ => None,
        }
    }

    fn rational(&self, e: &Entry) -> Option<(u32, u32)> {
        self.rational_at(e, 0)
    }

    /// The `index`-th value of a RATIONAL array.
    fn rational_at(&self, e: &Entry, index: usize) -> Option<(u32, u32)> {
        if e.typ != TYPE_RATIONAL {
            return None;
        }
        let v = e.value.get(index * 8..index * 8 + 8)?;
        Some((self.rd.u32(&v[0..4]), self.rd.u32(&v[4..8])))
    }

    fn ascii(&self, e: &Entry) -> Option<String> {
        if e.typ != TYPE_ASCII || e.count == 0 {
            return None;
        }
        let raw = &e.value[..e.value.len().min(MAX_STRING_LEN)];
        let s = String::from_utf8_lossy(raw);
        let s = s.trim_end_matches('\0').trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    fn parse_ifd(&self, offset: usize, info: &mut ExifInfo, subs: &mut SubIfds) {
        let Some(count) = self.bytes(offset, 2).map(|b| self.rd.u16(b)) else {
            return;
        };
        for i in 0..count as usize {
            let Some(e) = self.entry(offset + 2 + i * 12) else {
                break;
            };
            match e.tag {
                0x010F => info.make = self.ascii(&e),
                0x0110 => info.model = self.ascii(&e),
                0x0112 => info.orientation = self.uint(&e).map(|v| v as u16),
                0x0131 => info.software = self.ascii(&e),
                0x0132 => info.date_time = self.ascii(&e),
                0x8769 => subs.exif = self.uint(&e).map(|v| v as usize),
                0x8825 => subs.gps = self.uint(&e).map(|v| v as usize),
                0x829A => info.exposure_time = self.rational(&e).and_then(format_exposure),
                0x829D => info.f_number = self.rational(&e).and_then(ratio),
                0x8827 => info.iso = self.uint(&e),
                0x9003 => info.date_time_original = self.ascii(&e),
                0x9209 => info.flash_fired = self.uint(&e).map(|v| v & 1 != 0),
                0x920A => info.focal_length = self.rational(&e).and_then(ratio),
                0xA002 => info.width = self.uint(&e),
                0xA003 => info.height = self.uint(&e),
                0xA405 => info.focal_length_35mm = self.uint(&e),
                0xA434 => info.lens_model = self.ascii(&e),
                _ => {}
            }
        }
    }

    fn parse_gps_ifd(&self, offset: usize, info: &mut ExifInfo) {
        let Some(count) = self.bytes(offset, 2).map(|b| self.rd.u16(b)) else {
            return;
        };
        let (mut lat_ref, mut lon_ref, mut lat, mut lon) = (None, None, None, None);
        for i in 0..count as usize {
            let Some(e) = self.entry(offset + 2 + i * 12) else {
                break;
            };
            match e.tag {
                0x0001 => lat_ref = self.ascii(&e),
                0x0002 => lat = self.degrees(&e),
                0x0003 => lon_ref = self.ascii(&e),
                0x0004 => lon = self.degrees(&e),
                _ => {}
            }
        }
        let signed = |value: Option<f64>, reference: Option<String>, negative: &str, limit: f64| {
            let value = value.filter(|v| *v <= limit)?;
            Some(
                if reference
                    .as_deref()
                    .is_some_and(|r| r.eq_ignore_ascii_case(negative))
                {
                    -value
                } else {
                    value
                },
            )
        };
        info.gps_latitude = signed(lat, lat_ref, "S", 90.0);
        info.gps_longitude = signed(lon, lon_ref, "W", 180.0);
        // Cameras without a fix often write zeros; a lone half is useless either way.
        if info
            .gps_latitude
            .zip(info.gps_longitude)
            .is_none_or(|(a, b)| a == 0.0 && b == 0.0)
        {
            info.gps_latitude = None;
            info.gps_longitude = None;
        }
    }

    /// Degrees/minutes/seconds as three RATIONALs, to decimal degrees.
    fn degrees(&self, e: &Entry) -> Option<f64> {
        let part = |i| self.rational_at(e, i).and_then(ratio);
        Some(part(0)? + part(1)? / 60.0 + part(2)? / 3600.0)
    }
}

fn ratio((num, den): (u32, u32)) -> Option<f64> {
    (den != 0).then(|| num as f64 / den as f64)
}

fn format_exposure((num, den): (u32, u32)) -> Option<String> {
    if num == 0 || den == 0 {
        return None;
    }
    Some(if num >= den {
        format!("{:.1}", num as f64 / den as f64)
    } else if den % num == 0 {
        format!("1/{}", den / num)
    } else {
        format!("1/{}", (den as f64 / num as f64).round() as u32)
    })
}

/// A little-endian TIFF block (the payload of a JPEG `Exif` APP1 segment) with the fields of
/// `info` a photo library cares about. `orientation` replaces the source's value: pixels that were
/// already turned upright must be saved with 1. `width` / `height` must be the size of the image the
/// block goes with.
pub fn build_exif(info: &ExifInfo, orientation: u16) -> Vec<u8> {
    let mut ifd0 = Vec::new();
    let mut exif = Vec::new();
    let mut gps = Vec::new();
    ascii_entry(&mut ifd0, 0x010F, &info.make);
    ascii_entry(&mut ifd0, 0x0110, &info.model);
    ifd0.push(short_entry(0x0112, orientation));
    ascii_entry(&mut ifd0, 0x0131, &info.software);
    ascii_entry(&mut ifd0, 0x0132, &info.date_time);

    if let Some(r) = info.exposure_time.as_deref().and_then(parse_exposure) {
        exif.push(rational_entry(0x829A, &[r]));
    }
    if let Some(f) = info.f_number {
        exif.push(rational_entry(0x829D, &[tenths(f)]));
    }
    if let Some(iso) = info.iso {
        exif.push(match u16::try_from(iso) {
            Ok(v) => short_entry(0x8827, v),
            Err(_) => long_entry(0x8827, iso),
        });
    }
    ascii_entry(&mut exif, 0x9003, &info.date_time_original);
    if let Some(fired) = info.flash_fired {
        exif.push(short_entry(0x9209, fired as u16));
    }
    if let Some(f) = info.focal_length {
        exif.push(rational_entry(0x920A, &[tenths(f)]));
    }
    if let Some(f) = info.focal_length_35mm.and_then(|f| u16::try_from(f).ok()) {
        exif.push(short_entry(0xA405, f));
    }
    ascii_entry(&mut exif, 0xA434, &info.lens_model);
    if let Some((w, h)) = info.width.zip(info.height) {
        exif.push(long_entry(0xA002, w));
        exif.push(long_entry(0xA003, h));
    }

    if let Some((lat, lon)) = info.gps_latitude.zip(info.gps_longitude) {
        let reference =
            |v: f64, pos: &str, neg: &str| Some(if v < 0.0 { neg } else { pos }.to_string());
        ascii_entry(&mut gps, 0x0001, &reference(lat, "N", "S"));
        gps.push(rational_entry(0x0002, &dms(lat.abs())));
        ascii_entry(&mut gps, 0x0003, &reference(lon, "E", "W"));
        gps.push(rational_entry(0x0004, &dms(lon.abs())));
    }

    // Sub-IFD pointers are inline LONGs, so every IFD's size is known before their values.
    if !exif.is_empty() {
        ifd0.push(long_entry(0x8769, 0));
    }
    if !gps.is_empty() {
        ifd0.push(long_entry(0x8825, 0));
    }
    let ifd0_at = 8;
    let exif_at = ifd0_at + ifd_size(&ifd0);
    let gps_at = exif_at + if exif.is_empty() { 0 } else { ifd_size(&exif) };
    for e in &mut ifd0 {
        match e.tag {
            0x8769 => e.value = (exif_at as u32).to_le_bytes().to_vec(),
            0x8825 => e.value = (gps_at as u32).to_le_bytes().to_vec(),
            _ => {}
        }
    }

    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&(ifd0_at as u32).to_le_bytes());
    write_ifd(&mut out, ifd0);
    if !exif.is_empty() {
        write_ifd(&mut out, exif);
    }
    if !gps.is_empty() {
        write_ifd(&mut out, gps);
    }
    out
}

struct OutEntry {
    tag: u16,
    typ: u16,
    count: u32,
    /// Little-endian value bytes.
    value: Vec<u8>,
}

fn ascii_entry(ifd: &mut Vec<OutEntry>, tag: u16, text: &Option<String>) {
    if let Some(text) = text.as_deref().filter(|t| !t.is_empty()) {
        let mut value = text.as_bytes()[..text.len().min(MAX_STRING_LEN)].to_vec();
        value.push(0);
        ifd.push(OutEntry {
            tag,
            typ: TYPE_ASCII,
            count: value.len() as u32,
            value,
        });
    }
}

fn short_entry(tag: u16, v: u16) -> OutEntry {
    OutEntry {
        tag,
        typ: TYPE_SHORT,
        count: 1,
        value: v.to_le_bytes().to_vec(),
    }
}

fn long_entry(tag: u16, v: u32) -> OutEntry {
    OutEntry {
        tag,
        typ: TYPE_LONG,
        count: 1,
        value: v.to_le_bytes().to_vec(),
    }
}

fn rational_entry(tag: u16, values: &[(u32, u32)]) -> OutEntry {
    OutEntry {
        tag,
        typ: TYPE_RATIONAL,
        count: values.len() as u32,
        value: values
            .iter()
            .flat_map(|&(n, d)| n.to_le_bytes().into_iter().chain(d.to_le_bytes()))
            .collect(),
    }
}

/// Values over 4 bytes go after the entries, each at an even offset.
fn external_len(e: &OutEntry) -> usize {
    if e.value.len() > 4 {
        e.value.len().next_multiple_of(2)
    } else {
        0
    }
}

fn ifd_size(entries: &[OutEntry]) -> usize {
    2 + entries.len() * 12 + 4 + entries.iter().map(external_len).sum::<usize>()
}

/// Appends an IFD (entries sorted by tag, as TIFF requires) and its values; no next IFD.
fn write_ifd(out: &mut Vec<u8>, mut entries: Vec<OutEntry>) {
    entries.sort_by_key(|e| e.tag);
    let mut data_at = out.len() + 2 + entries.len() * 12 + 4;
    let mut data = Vec::new();
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for e in &entries {
        out.extend_from_slice(&e.tag.to_le_bytes());
        out.extend_from_slice(&e.typ.to_le_bytes());
        out.extend_from_slice(&e.count.to_le_bytes());
        if e.value.len() <= 4 {
            let mut inline = [0u8; 4];
            inline[..e.value.len()].copy_from_slice(&e.value);
            out.extend_from_slice(&inline);
        } else {
            out.extend_from_slice(&(data_at as u32).to_le_bytes());
            data.extend_from_slice(&e.value);
            data.resize(data.len().next_multiple_of(2), 0);
            data_at += external_len(e);
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&data);
}

/// Back from the display form of [`format_exposure`]: "1/250" or "2.5" (seconds).
fn parse_exposure(s: &str) -> Option<(u32, u32)> {
    match s.split_once('/') {
        Some((n, d)) => Some((n.trim().parse().ok()?, d.trim().parse().ok()?)),
        None => Some(tenths(s.trim().parse().ok()?)),
    }
    .filter(|&(n, d)| n > 0 && d > 0)
}

fn tenths(v: f64) -> (u32, u32) {
    ((v * 10.0).round().clamp(0.0, u32::MAX as f64) as u32, 10)
}

/// Decimal degrees as degrees / minutes / hundredths of a second.
fn dms(v: f64) -> [(u32, u32); 3] {
    let degrees = v.trunc();
    let minutes = ((v - degrees) * 60.0).trunc();
    let seconds = ((v - degrees) * 60.0 - minutes) * 60.0;
    [
        (degrees as u32, 1),
        (minutes as u32, 1),
        ((seconds * 100.0).round() as u32, 100),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Big-endian TIFF with IFD0 {Orientation SHORT=6, ExifIFD} and Exif IFD
    /// {ExposureTime 0/1, FNumber 28/10, ISO SHORT=400}.
    fn be_tiff() -> Vec<u8> {
        let mut t = b"MM\0*".to_vec();
        t.extend_from_slice(&8u32.to_be_bytes());
        // IFD0 at 8: 2 entries
        t.extend_from_slice(&2u16.to_be_bytes());
        t.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0]);
        let exif_ifd = 8 + 2 + 2 * 12 + 4;
        t.extend_from_slice(&[0x87, 0x69, 0, 4, 0, 0, 0, 1]);
        t.extend_from_slice(&(exif_ifd as u32).to_be_bytes());
        t.extend_from_slice(&0u32.to_be_bytes());
        // Exif IFD: 3 entries, rationals stored after it
        let data_off = exif_ifd + 2 + 3 * 12 + 4;
        t.extend_from_slice(&3u16.to_be_bytes());
        t.extend_from_slice(&[0x82, 0x9A, 0, 5, 0, 0, 0, 1]);
        t.extend_from_slice(&(data_off as u32).to_be_bytes());
        t.extend_from_slice(&[0x82, 0x9D, 0, 5, 0, 0, 0, 1]);
        t.extend_from_slice(&(data_off as u32 + 8).to_be_bytes());
        t.extend_from_slice(&[0x88, 0x27, 0, 3, 0, 0, 0, 1, 0x01, 0x90, 0, 0]);
        t.extend_from_slice(&0u32.to_be_bytes());
        t.extend_from_slice(&0u32.to_be_bytes());
        t.extend_from_slice(&1u32.to_be_bytes());
        t.extend_from_slice(&28u32.to_be_bytes());
        t.extend_from_slice(&10u32.to_be_bytes());
        t
    }

    #[test]
    fn big_endian_shorts_and_zero_exposure() {
        let info = parse_tiff(&be_tiff()).expect("valid TIFF");
        assert_eq!(info.orientation, Some(6));
        assert_eq!(info.iso, Some(400));
        assert_eq!(info.exposure_time, None);
        assert_eq!(info.f_number, Some(2.8));
    }

    /// The Exif IFD past the first read: the file is read further and the ISO still found.
    #[test]
    fn exif_ifd_beyond_the_head_is_found() {
        let far = 100_000usize;
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&[0x69, 0x87, 4, 0, 1, 0, 0, 0]);
        t.extend_from_slice(&(far as u32).to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        t.resize(far, 0);
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&[0x27, 0x88, 3, 0, 1, 0, 0, 0, 0x90, 0x01, 0, 0]);
        t.extend_from_slice(&0u32.to_le_bytes());
        assert!(parse_tiff_checked(&t[..TIFF_HEAD_BYTES as usize]).1);
        assert!(!parse_tiff_checked(&t).1);

        let path = std::env::temp_dir().join(format!("mediares_{}_far.tif", std::process::id()));
        std::fs::write(&path, &t).unwrap();
        let iso = read_exif(&path).and_then(|e| e.iso);
        std::fs::remove_file(&path).ok();
        assert_eq!(iso, Some(400));
    }

    #[test]
    fn exposure_formatting() {
        assert_eq!(format_exposure((1, 250)).as_deref(), Some("1/250"));
        assert_eq!(format_exposure((10, 2500)).as_deref(), Some("1/250"));
        assert_eq!(format_exposure((3, 1000)).as_deref(), Some("1/333"));
        assert_eq!(format_exposure((5, 2)).as_deref(), Some("2.5"));
        assert_eq!(format_exposure((0, 1)), None);
        assert_eq!(format_exposure((1, 0)), None);
    }

    #[test]
    fn datetime_parsing() {
        let dt = |y, mo, d, h, mi, s| {
            Some(ExifDateTime {
                year: y,
                month: mo,
                day: d,
                hour: h,
                minute: mi,
                second: s,
            })
        };
        assert_eq!(
            parse_exif_datetime("2024:05:01 12:34:56"),
            dt(2024, 5, 1, 12, 34, 56)
        );
        assert_eq!(
            parse_exif_datetime("2024-05-01 07:08:09"),
            dt(2024, 5, 1, 7, 8, 9)
        );
        assert_eq!(parse_exif_datetime("2024:05:01"), dt(2024, 5, 1, 0, 0, 0));
        assert_eq!(parse_exif_datetime("0000:00:00 00:00:00"), None);
        assert_eq!(parse_exif_datetime("    :  :     :  :  "), None);
        assert_eq!(parse_exif_datetime("2024:13:01 00:00:00"), None);
        assert_eq!(parse_exif_datetime("2024:05:01 24:00:00"), None);
    }

    #[test]
    fn truncated_data_is_harmless() {
        let t = be_tiff();
        for len in 0..t.len() {
            let _ = parse_tiff(&t[..len]);
        }
    }

    #[test]
    fn written_exif_reads_back() {
        let info = ExifInfo {
            make: Some("SONY".into()),
            model: Some("ILCE-7RM4".into()),
            date_time: Some("2024:09:02 14:33:10".into()),
            date_time_original: Some("2024:09:02 14:33:09".into()),
            exposure_time: Some("1/250".into()),
            f_number: Some(2.8),
            iso: Some(400),
            focal_length: Some(50.0),
            focal_length_35mm: Some(75),
            lens_model: Some("FE 50mm F1.8".into()),
            flash_fired: Some(false),
            orientation: Some(6),
            software: Some("ILCE-7RM4 v2.0".into()),
            gps_latitude: Some(55.751244),
            gps_longitude: Some(-37.618423),
            ..Default::default()
        };
        let back = parse_tiff(&build_exif(&info, 1)).unwrap();
        let lat = back.gps_latitude.unwrap();
        let lon = back.gps_longitude.unwrap();
        assert!((lat - 55.751244).abs() < 1e-5 && (lon + 37.618423).abs() < 1e-5);
        assert_eq!(
            back,
            ExifInfo {
                orientation: Some(1),
                gps_latitude: back.gps_latitude,
                gps_longitude: back.gps_longitude,
                ..info
            }
        );
    }

    #[test]
    fn minimal_exif_has_only_orientation() {
        let back = parse_tiff(&build_exif(&ExifInfo::default(), 3)).unwrap();
        assert_eq!(
            back,
            ExifInfo {
                orientation: Some(3),
                ..Default::default()
            }
        );
        assert_eq!(parse_exposure("2.5"), Some((25, 10)));
        assert_eq!(parse_exposure("0"), None);
    }
}
