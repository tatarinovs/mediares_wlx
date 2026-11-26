//! Extra image metadata beyond standard EXIF: PNG/JPEG/TIFF DPI and embedded AI generation parameters.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use flate2::read::ZlibDecoder;

pub(crate) mod c2pa;

const MAX_SCAN_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHUNK_BYTES: usize = 2 * 1024 * 1024;
const MAX_DECOMPRESS_BYTES: u64 = 256 * 1024;
const MAX_CHUNKS: usize = 8192;
const MAX_AI_PARAM_BYTES: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageExtraMeta {
    pub dpi: Option<u32>,
    pub ai_parameters: Option<String>,
}

pub fn read_image_extra_meta(path: &Path) -> Option<ImageExtraMeta> {
    let mut file = File::open(path).ok()?;
    let mut head = [0u8; 12];
    let n = file.read(&mut head).ok()?;
    if n < 4 {
        return None;
    }
    let head = &head[..n];
    if n >= 8 && &head[..8] == b"\x89PNG\r\n\x1a\n" {
        read_png_extra(&mut file)
    } else if head[..3] == [0xFF, 0xD8, 0xFF] {
        read_jpeg_extra(&mut file)
    } else if head[..4] == *b"II*\0" || head[..4] == *b"MM\0*" {
        Some(ImageExtraMeta {
            dpi: crate::exif::read_tiff_dpi(&mut file).and_then(valid_dpi),
            ai_parameters: None,
        })
    } else {
        Some(ImageExtraMeta::default())
    }
}

fn valid_dpi(v: u32) -> Option<u32> {
    (1..=100_000).contains(&v).then_some(v)
}

fn normalize_ai_text(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len().min(MAX_AI_PARAM_BYTES));
    let mut in_space = true;
    for ch in s.chars() {
        let mapped = match ch {
            '\0' => continue,
            '\r' | '\n' | '\t' | ' ' => ' ',
            c if c.is_control() => continue,
            c => c,
        };
        if mapped == ' ' {
            if !in_space {
                if out.len() + 1 > MAX_AI_PARAM_BYTES {
                    break;
                }
                out.push(' ');
                in_space = true;
            }
        } else {
            if out.len() + mapped.len_utf8() > MAX_AI_PARAM_BYTES {
                break;
            }
            out.push(mapped);
            in_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    (!out.is_empty()).then_some(out)
}

fn looks_like_ai_params(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("steps:")
        || lower.contains("\"steps\":")
        || lower.contains("sampler:")
        || lower.contains("cfg scale:")
        || lower.contains("negative prompt:")
        || lower.contains("\"class_type\"")
}

fn read_png_extra(file: &mut File) -> Option<ImageExtraMeta> {
    file.seek(SeekFrom::Start(8)).ok()?;
    let mut r = BufReader::new(file);
    let mut dpi = None;
    let mut scanned = 0usize;

    let mut param_primary: Option<String> = None;
    let mut param_prompt: Option<String> = None;
    let mut param_workflow: Option<String> = None;
    let mut param_comment: Option<String> = None;
    let mut param_desc: Option<String> = None;
    let mut ai_software = false;
    let mut provenance = None;

    for _ in 0..MAX_CHUNKS {
        let mut hdr = [0u8; 8];
        if r.read_exact(&mut hdr).is_err() {
            break;
        }
        let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let kind = &hdr[4..8];
        if kind == b"IEND" {
            break;
        }
        let wants = matches!(kind, b"pHYs" | b"tEXt" | b"zTXt" | b"iTXt" | b"caBX");
        if !wants || len > MAX_CHUNK_BYTES || scanned + len > MAX_SCAN_BYTES {
            let skip = (len as u64).saturating_add(4);
            if r.seek(SeekFrom::Current(skip as i64)).is_err() {
                break;
            }
            continue;
        }
        scanned += len;
        let mut data = vec![0u8; len];
        if r.read_exact(&mut data).is_err() {
            break;
        }
        let mut crc = [0u8; 4];
        if r.read_exact(&mut crc).is_err() {
            break;
        }

        if kind == b"caBX" {
            provenance = c2pa::ai_source(&data);
            continue;
        }
        if kind == b"pHYs" && data.len() == 9 && data[8] == 1 {
            let ppu_x = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            if ppu_x > 0 {
                dpi = valid_dpi(((ppu_x as f64) * 0.0254).round() as u32);
            }
            continue;
        }

        let parsed = match kind {
            b"tEXt" => parse_png_text(&data),
            b"zTXt" => parse_png_ztxt(&data),
            b"iTXt" => parse_png_itxt(&data),
            _ => None,
        };
        if let Some((key, val)) = parsed {
            if key.eq_ignore_ascii_case("software") {
                let lower = val.to_ascii_lowercase();
                if lower.contains("novelai")
                    || lower.contains("midjourney")
                    || lower.contains("stable diffusion")
                    || lower.contains("comfyui")
                    || lower.contains("automatic1111")
                    || lower.contains("fooocus")
                {
                    ai_software = true;
                }
            } else if key.eq_ignore_ascii_case("parameters") && param_primary.is_none() {
                param_primary = normalize_ai_text(&val);
            } else if key.eq_ignore_ascii_case("prompt") && param_prompt.is_none() {
                param_prompt = normalize_ai_text(&val);
            } else if key.eq_ignore_ascii_case("workflow") && param_workflow.is_none() {
                param_workflow = normalize_ai_text(&val);
            } else if key.eq_ignore_ascii_case("comment") && param_comment.is_none() {
                param_comment = normalize_ai_text(&val).filter(|s| looks_like_ai_params(s));
            } else if key.eq_ignore_ascii_case("description") && param_desc.is_none() {
                param_desc = normalize_ai_text(&val);
            }
        }
    }

    let ai_parameters = param_primary
        .or(param_prompt)
        .or(param_workflow)
        .or(param_comment)
        .or_else(|| param_desc.filter(|d| ai_software || looks_like_ai_params(d)))
        .or(provenance);

    Some(ImageExtraMeta { dpi, ai_parameters })
}

fn latin1_to_string(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        s.to_string()
    } else {
        bytes.iter().map(|&b| b as char).collect()
    }
}

fn zlib_decompress_string(compressed: &[u8], utf8_expected: bool) -> Option<String> {
    let mut decoder = ZlibDecoder::new(compressed).take(MAX_DECOMPRESS_BYTES);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    Some(if utf8_expected {
        String::from_utf8_lossy(&out).into_owned()
    } else {
        latin1_to_string(&out)
    })
}

fn parse_png_text(data: &[u8]) -> Option<(String, String)> {
    let nul = data.iter().position(|&b| b == 0)?;
    let key = latin1_to_string(&data[..nul]);
    let val = latin1_to_string(&data[nul + 1..]);
    Some((key, val))
}

fn parse_png_ztxt(data: &[u8]) -> Option<(String, String)> {
    let nul = data.iter().position(|&b| b == 0)?;
    let key = latin1_to_string(&data[..nul]);
    let &method = data.get(nul + 1)?;
    if method != 0 {
        return None;
    }
    let val = zlib_decompress_string(data.get(nul + 2..)?, false)?;
    Some((key, val))
}

fn parse_png_itxt(data: &[u8]) -> Option<(String, String)> {
    let nul1 = data.iter().position(|&b| b == 0)?;
    let key = String::from_utf8_lossy(&data[..nul1]).into_owned();
    let &comp_flag = data.get(nul1 + 1)?;
    let &comp_method = data.get(nul1 + 2)?;
    let rest = data.get(nul1 + 3..)?;
    let nul2 = rest.iter().position(|&b| b == 0)?;
    let rest = rest.get(nul2 + 1..)?;
    let nul3 = rest.iter().position(|&b| b == 0)?;
    let text_bytes = rest.get(nul3 + 1..)?;
    let val = match (comp_flag, comp_method) {
        (0, _) => String::from_utf8_lossy(text_bytes).into_owned(),
        (1, 0) => zlib_decompress_string(text_bytes, true)?,
        _ => return None,
    };
    Some((key, val))
}

fn read_jpeg_extra(file: &mut File) -> Option<ImageExtraMeta> {
    file.seek(SeekFrom::Start(2)).ok()?;
    let mut r = BufReader::new(file);
    let mut jfif_dpi = None;
    let mut exif_dpi = None;
    let mut user_comment = None;
    let mut com_text = None;
    let mut credentials = c2pa::JpegManifest::default();
    let mut scanned = 0usize;

    for _ in 0..128 {
        let mut b = [0u8; 1];
        if r.read_exact(&mut b).is_err() {
            break;
        }
        if b[0] != 0xFF {
            break;
        }
        let mut marker = 0xFF;
        while marker == 0xFF {
            if r.read_exact(&mut b).is_err() {
                break;
            }
            marker = b[0];
        }
        if marker == 0xFF || marker == 0x00 || marker == 0xD9 || marker == 0xDA {
            break;
        }
        if matches!(marker, 0xD0..=0xD8 | 0x01) {
            continue;
        }
        let mut len_buf = [0u8; 2];
        if r.read_exact(&mut len_buf).is_err() {
            break;
        }
        let seg_len = u16::from_be_bytes(len_buf) as usize;
        if seg_len < 2 {
            break;
        }
        let payload_len = seg_len - 2;
        let wants = matches!(marker, 0xE0 | 0xE1 | 0xEB | 0xFE);
        if !wants || payload_len > MAX_CHUNK_BYTES || scanned + payload_len > MAX_SCAN_BYTES {
            if r.seek(SeekFrom::Current(payload_len as i64)).is_err() {
                break;
            }
            continue;
        }
        scanned += payload_len;
        let mut payload = vec![0u8; payload_len];
        if r.read_exact(&mut payload).is_err() {
            break;
        }

        if marker == 0xEB {
            credentials.push(&payload);
        } else if marker == 0xE0
            && payload.len() >= 14
            && payload.starts_with(b"JFIF\0")
            && jfif_dpi.is_none()
        {
            let units = payload[7];
            let x_den = u16::from_be_bytes([payload[8], payload[9]]);
            if x_den > 0 {
                jfif_dpi = match units {
                    1 => valid_dpi(u32::from(x_den)),
                    2 => valid_dpi((f64::from(x_den) * 2.54).round() as u32),
                    _ => None,
                };
            }
        } else if marker == 0xE1 && payload.starts_with(b"Exif\0\0") {
            let (dpi, uc) = parse_tiff_dpi_and_comment(&payload[6..]);
            exif_dpi = exif_dpi.or(dpi);
            if user_comment.is_none() {
                user_comment = uc.filter(|s| looks_like_ai_params(s));
            }
        } else if marker == 0xFE && com_text.is_none() {
            let s = String::from_utf8_lossy(&payload);
            com_text = normalize_ai_text(&s).filter(|s| looks_like_ai_params(s));
        }
    }

    Some(ImageExtraMeta {
        dpi: exif_dpi.or(jfif_dpi),
        ai_parameters: user_comment
            .or(com_text)
            .or_else(|| credentials.ai_source()),
    })
}

fn parse_tiff_dpi_and_comment(tiff: &[u8]) -> (Option<u32>, Option<String>) {
    if tiff.len() < 8 {
        return (None, None);
    }
    let le = match &tiff[..4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return (None, None),
    };
    let u16_at = |off: usize| -> Option<u16> {
        let b: [u8; 2] = tiff.get(off..off + 2)?.try_into().ok()?;
        Some(if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |off: usize| -> Option<u32> {
        let b: [u8; 4] = tiff.get(off..off + 4)?.try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };

    let Some(ifd0_off) = u32_at(4).map(|v| v as usize) else {
        return (None, None);
    };
    let Some(count) = u16_at(ifd0_off).map(|v| v as usize) else {
        return (None, None);
    };

    let mut x_res: Option<f64> = None;
    let mut res_unit: Option<u16> = None;
    let mut exif_ifd: Option<usize> = None;

    for i in 0..count.min(256) {
        let entry = ifd0_off + 2 + i * 12;
        let Some(tag) = u16_at(entry) else { break };
        let Some(typ) = u16_at(entry + 2) else { break };
        let Some(cnt) = u32_at(entry + 4) else { break };
        let Some(val_or_off) = u32_at(entry + 8) else {
            break;
        };
        match tag {
            0x011A if typ == 5 && cnt >= 1 => {
                let off = val_or_off as usize;
                if let (Some(num), Some(den)) = (u32_at(off), u32_at(off + 4)) {
                    if den != 0 {
                        x_res = Some(f64::from(num) / f64::from(den));
                    }
                }
            }
            0x0128 if typ == 3 && cnt >= 1 => {
                res_unit = u16_at(entry + 8);
            }
            0x8769 if cnt >= 1 => {
                exif_ifd = Some(val_or_off as usize);
            }
            _ => {}
        }
    }

    let dpi = x_res.and_then(|res| {
        if res <= 0.0 {
            return None;
        }
        let val = match res_unit.unwrap_or(2) {
            2 => res.round() as u32,
            3 => (res * 2.54).round() as u32,
            _ => return None,
        };
        valid_dpi(val)
    });

    let mut comment = None;
    if let Some(exif_off) = exif_ifd {
        if let Some(ecount) = u16_at(exif_off).map(|v| v as usize) {
            for i in 0..ecount.min(256) {
                let entry = exif_off + 2 + i * 12;
                let Some(tag) = u16_at(entry) else { break };
                let Some(cnt) = u32_at(entry + 4) else { break };
                let Some(val_or_off) = u32_at(entry + 8) else {
                    break;
                };
                if tag == 0x9286 && cnt > 8 {
                    let off = val_or_off as usize;
                    let len = (cnt as usize).min(64 * 1024);
                    if let Some(raw) = tiff.get(off..off + len) {
                        comment = decode_exif_user_comment(raw);
                    }
                    break;
                }
            }
        }
    }

    (dpi, comment)
}

fn decode_exif_user_comment(raw: &[u8]) -> Option<String> {
    if raw.starts_with(b"ASCII\0\0\0") {
        let s = String::from_utf8_lossy(&raw[8..]);
        return normalize_ai_text(&s);
    }
    if raw.starts_with(b"UNICODE\0") {
        let bytes = &raw[8..];
        let is_le = !bytes.starts_with(&[0xFE, 0xFF])
            && (bytes.starts_with(&[0xFF, 0xFE])
                || bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .take(32)
                    .filter(|c| c[1] == 0 && c[0] != 0)
                    .count()
                    >= bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .take(32)
                        .filter(|c| c[0] == 0 && c[1] != 0)
                        .count());
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| {
                if is_le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .skip_while(|&u| u == 0xFEFF)
            .collect();
        return normalize_ai_text(&String::from_utf16_lossy(&units));
    }
    if raw.starts_with(b"\0\0\0\0\0\0\0\0") {
        return None;
    }
    normalize_ai_text(&String::from_utf8_lossy(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn png_chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + body.len());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out.extend_from_slice(&[0; 4]);
        out
    }

    #[test]
    fn reads_png_dpi_and_ai_parameters_across_text_chunks() {
        let dir = std::env::temp_dir().join(format!("mediares_img_meta_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let png_path = dir.join("gen.png");
        let mut png = Vec::from(&b"\x89PNG\r\n\x1a\n"[..]);
        // 300 DPI = 11811 pixels per meter
        let mut phys = Vec::new();
        phys.extend_from_slice(&11811u32.to_be_bytes());
        phys.extend_from_slice(&11811u32.to_be_bytes());
        phys.push(1);
        png.extend_from_slice(&png_chunk(b"pHYs", &phys));
        png.extend_from_slice(&png_chunk(
            b"tEXt",
            b"parameters\0a cat\nSteps: 20, Sampler: Euler a",
        ));
        png.extend_from_slice(&png_chunk(b"IEND", &[]));
        std::fs::write(&png_path, &png).unwrap();

        let meta = read_image_extra_meta(&png_path).unwrap();
        assert_eq!(meta.dpi, Some(300));
        assert_eq!(
            meta.ai_parameters.as_deref(),
            Some("a cat Steps: 20, Sampler: Euler a")
        );

        // Compressed zTXt + iTXt
        let z_path = dir.join("z.png");
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"{\"1\":{\"class_type\":\"KSampler\"}}")
            .unwrap();
        let compressed = enc.finish().unwrap();
        let mut ztxt = Vec::from(&b"prompt\0\0"[..]);
        ztxt.extend_from_slice(&compressed);
        let mut zpng = Vec::from(&b"\x89PNG\r\n\x1a\n"[..]);
        zpng.extend_from_slice(&png_chunk(b"zTXt", &ztxt));
        zpng.extend_from_slice(&png_chunk(b"IEND", &[]));
        std::fs::write(&z_path, &zpng).unwrap();
        let zmeta = read_image_extra_meta(&z_path).unwrap();
        assert_eq!(zmeta.dpi, None);
        assert_eq!(
            zmeta.ai_parameters.as_deref(),
            Some("{\"1\":{\"class_type\":\"KSampler\"}}")
        );

        let itxt_path = dir.join("i.png");
        let mut ipng = Vec::from(&b"\x89PNG\r\n\x1a\n"[..]);
        ipng.extend_from_slice(&png_chunk(b"tEXt", b"Comment\0Summer 2024"));
        std::fs::write(&itxt_path, &ipng).unwrap();
        assert_eq!(
            read_image_extra_meta(&itxt_path).unwrap().ai_parameters,
            None
        );
        ipng.extend_from_slice(&png_chunk(b"iTXt", b"Comment\0\0\0\0\0{\"steps\": 28}"));
        ipng.extend_from_slice(&png_chunk(b"IEND", &[]));
        std::fs::write(&itxt_path, &ipng).unwrap();
        let imeta = read_image_extra_meta(&itxt_path).unwrap();
        assert_eq!(imeta.ai_parameters.as_deref(), Some("{\"steps\": 28}"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_jpeg_jfif_dpi_exif_user_comment_and_com() {
        let dir = std::env::temp_dir().join(format!("mediares_jpg_meta_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jpg");

        // Build minimal APP0 JFIF (150 DPI) + APP1 Exif (300 DPI + UNICODE UserComment)
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset = 8
                                                     // IFD0: 3 entries (0x011A XResolution, 0x0128 ResolutionUnit, 0x8769 ExifIFD)
        let ifd0_count = 3u16;
        let after_ifd0 = 8 + 2 + usize::from(ifd0_count) * 12 + 4;
        let xres_off = after_ifd0 as u32;
        let exif_ifd_off = after_ifd0 + 8;
        let uc_off = exif_ifd_off + 2 + 12 + 4;
        tiff.extend_from_slice(&ifd0_count.to_le_bytes());
        // 0x011A: RATIONAL (5), count 1, offset xres_off
        tiff.extend_from_slice(&0x011Au16.to_le_bytes());
        tiff.extend_from_slice(&5u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&xres_off.to_le_bytes());
        // 0x0128: SHORT (3), count 1, value 2 (inch)
        tiff.extend_from_slice(&0x0128u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&2u32.to_le_bytes());
        // 0x8769: LONG (4), count 1, offset exif_ifd_off
        tiff.extend_from_slice(&0x8769u16.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&(exif_ifd_off as u32).to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD
                                                     // XResolution = 300/1
        tiff.extend_from_slice(&300u32.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        // Exif IFD: 1 entry (0x9286 UserComment)
        let mut uc = Vec::from(&b"UNICODE\0"[..]);
        for u in "Steps: 30".encode_utf16() {
            uc.extend_from_slice(&u.to_le_bytes());
        }
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x9286u16.to_le_bytes());
        tiff.extend_from_slice(&7u16.to_le_bytes());
        tiff.extend_from_slice(&(uc.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&(uc_off as u32).to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&uc);

        let mut jpg = vec![0xFF, 0xD8];
        // APP0 JFIF (150 dpi)
        let jfif = b"JFIF\0\x01\x01\x01\x00\x96\x00\x96\x00\x00";
        jpg.extend_from_slice(&[0xFF, 0xE0]);
        jpg.extend_from_slice(&((jfif.len() + 2) as u16).to_be_bytes());
        jpg.extend_from_slice(jfif);
        // APP1 Exif
        let mut app1 = Vec::from(&b"Exif\0\0"[..]);
        app1.extend_from_slice(&tiff);
        jpg.extend_from_slice(&[0xFF, 0xE1]);
        jpg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        jpg.extend_from_slice(&app1);
        jpg.extend_from_slice(&[0xFF, 0xD9]);
        std::fs::write(&path, &jpg).unwrap();

        let meta = read_image_extra_meta(&path).unwrap();
        assert_eq!(meta.dpi, Some(300));
        assert_eq!(meta.ai_parameters.as_deref(), Some("Steps: 30"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
