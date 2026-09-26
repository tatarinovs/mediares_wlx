//! EXIF metadata reader for JPEG and TIFF-based (TIFF/RAW) files.
//!
//! The metadata block is read into memory once and parsed from a slice, so every access is
//! bounds-checked and malformed offsets simply yield missing fields.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// TIFF-based files keep IFD0 and the Exif IFD near the start; this is how much we scan.
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
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;

    let block = if magic.starts_with(&[0xFF, 0xD8]) {
        read_jpeg_app1(&mut file)?
    } else if &magic == b"II*\0" || &magic == b"MM\0*" {
        let mut data = Vec::new();
        file.seek(SeekFrom::Start(0)).ok()?;
        file.take(TIFF_SCAN_BYTES).read_to_end(&mut data).ok()?;
        data
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

        if marker == 0xE1 && payload_len > 6 {
            let mut payload = vec![0u8; payload_len];
            file.read_exact(&mut payload).ok()?;
            if payload.starts_with(b"Exif\0\0") {
                payload.drain(..6);
                return Some(payload);
            }
        } else {
            file.seek(SeekFrom::Current(payload_len as i64)).ok()?;
        }
    }
}

/// Parses a TIFF structure (`II*\0` / `MM\0*` header) from memory.
fn parse_tiff(data: &[u8]) -> Option<ExifInfo> {
    let le = match data.get(..4)? {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return None,
    };
    let tiff = Tiff { data, le };
    let ifd0 = tiff.u32(4)? as usize;

    let mut info = ExifInfo::default();
    let mut subs = SubIfds::default();
    tiff.parse_ifd(ifd0, &mut info, &mut subs);
    if let Some(offset) = subs.exif {
        tiff.parse_ifd(offset, &mut info, &mut SubIfds::default());
    }
    if let Some(offset) = subs.gps {
        tiff.parse_gps_ifd(offset, &mut info);
    }
    Some(info)
}

/// Offsets of the Exif and GPS IFDs referenced from IFD0.
#[derive(Default)]
struct SubIfds {
    exif: Option<usize>,
    gps: Option<usize>,
}

struct Tiff<'a> {
    data: &'a [u8],
    le: bool,
}

struct Entry<'a> {
    tag: u16,
    typ: u16,
    count: u32,
    value: &'a [u8],
}

impl<'a> Tiff<'a> {
    fn bytes(&self, offset: usize, len: usize) -> Option<&'a [u8]> {
        self.data.get(offset..offset.checked_add(len)?)
    }

    fn u16_at(&self, b: &[u8]) -> u16 {
        let v = [b[0], b[1]];
        if self.le {
            u16::from_le_bytes(v)
        } else {
            u16::from_be_bytes(v)
        }
    }

    fn u32_at(&self, b: &[u8]) -> u32 {
        let v = [b[0], b[1], b[2], b[3]];
        if self.le {
            u32::from_le_bytes(v)
        } else {
            u32::from_be_bytes(v)
        }
    }

    fn u32(&self, offset: usize) -> Option<u32> {
        self.bytes(offset, 4).map(|b| self.u32_at(b))
    }

    /// Decodes the 12-byte IFD entry at `offset`; values up to 4 bytes are stored inline.
    fn entry(&self, offset: usize) -> Option<Entry<'a>> {
        let raw = self.bytes(offset, 12)?;
        let tag = self.u16_at(&raw[0..2]);
        let typ = self.u16_at(&raw[2..4]);
        let count = self.u32_at(&raw[4..8]);
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
            self.bytes(self.u32_at(&raw[8..12]) as usize, size)?
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
            TYPE_SHORT if e.value.len() >= 2 => Some(self.u16_at(e.value) as u32),
            TYPE_LONG if e.value.len() >= 4 => Some(self.u32_at(e.value)),
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
        Some((self.u32_at(&v[0..4]), self.u32_at(&v[4..8])))
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
        let Some(count) = self.bytes(offset, 2).map(|b| self.u16_at(b)) else {
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
        let Some(count) = self.bytes(offset, 2).map(|b| self.u16_at(b)) else {
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
}
