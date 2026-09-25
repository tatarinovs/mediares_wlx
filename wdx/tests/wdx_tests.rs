use std::os::raw::{c_char, c_void};
use std::path::PathBuf;
use mediares_wdx::*;
use mediares_core::tc_api::*;

#[test]
fn test_supported_fields_enumeration() {
    let mut field_name = [0 as c_char; 128];
    let mut units = [0 as c_char; 128];

    // Field 0: Image_dHash
    let f0 = unsafe { ContentGetSupportedField(0, field_name.as_mut_ptr(), units.as_mut_ptr(), 128) };
    assert_eq!(f0, FT_STRINGW);
    let name0 = unsafe { std::ffi::CStr::from_ptr(field_name.as_ptr()).to_str().unwrap() };
    assert_eq!(name0, "Image_dHash");

    // Field 7: Video_Duration_Sec
    let f7 = unsafe { ContentGetSupportedField(7, field_name.as_mut_ptr(), units.as_mut_ptr(), 128) };
    assert_eq!(f7, FT_NUMERIC_32);
    let name7 = unsafe { std::ffi::CStr::from_ptr(field_name.as_ptr()).to_str().unwrap() };
    assert_eq!(name7, "Video_Duration_Sec");

    // Field 9: Media_Type
    let f9 = unsafe { ContentGetSupportedField(9, field_name.as_mut_ptr(), units.as_mut_ptr(), 128) };
    assert_eq!(f9, FT_STRINGW);
    let name9 = unsafe { std::ffi::CStr::from_ptr(field_name.as_ptr()).to_str().unwrap() };
    assert_eq!(name9, "Media_Type");

    // Field 10: Plugin_Version
    let f10 = unsafe { ContentGetSupportedField(10, field_name.as_mut_ptr(), units.as_mut_ptr(), 128) };
    assert_eq!(f10, FT_STRINGW);
    let name10 = unsafe { std::ffi::CStr::from_ptr(field_name.as_ptr()).to_str().unwrap() };
    assert_eq!(name10, "Plugin_Version");

    // Fields 11..14: audio, appended after the original ones (indices are stored by TC).
    for (index, name, kind) in [
        (11, "Audio_Fingerprint", FT_STRINGW),
        (12, "Audio_PCM_Hash", FT_STRINGW),
        (13, "Audio_Duration_Sec", FT_NUMERIC_32),
        (14, "Audio_Artist_Title", FT_STRINGW),
    ] {
        let f = unsafe { ContentGetSupportedField(index, field_name.as_mut_ptr(), units.as_mut_ptr(), 128) };
        assert_eq!(f, kind);
        let got = unsafe { std::ffi::CStr::from_ptr(field_name.as_ptr()).to_str().unwrap() };
        assert_eq!(got, name);
    }

    // Field 30: out of bounds
    let f15 = unsafe { ContentGetSupportedField(30, field_name.as_mut_ptr(), units.as_mut_ptr(), 128) };
    assert_eq!(f15, FT_NOMOREFIELDS);
}

#[test]
fn test_detect_string() {
    let mut buf = [0 as c_char; 512];
    unsafe {
        ContentGetDetectString(buf.as_mut_ptr(), 512);
    }
    let detect = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()).to_str().unwrap() };
    assert!(detect.contains(r#"EXT="JPG""#));
    assert!(detect.contains(r#"EXT="PNG""#));
    assert!(detect.contains(r#"EXT="MP4""#));
    assert!(detect.contains(r#"EXT="MKV""#));
    assert!(detect.contains(r#"EXT="CR2""#));
    assert!(detect.contains(r#"EXT="PSD""#));
    // Every RAW/PSD extension the core can decode must be advertised.
    assert!(detect.contains(r#"EXT="CR3""#));
    assert!(detect.contains(r#"EXT="PSB""#));
    assert!(detect.contains(r#"EXT="MP3""#));
    assert!(detect.contains(r#"EXT="FLAC""#));
    assert!(!detect.contains(r#"EXT="M3U""#), "playlists have no content fields");
}

#[test]
fn test_image_hashes_and_duplicate_matching() {
    use mediares_core::image::{self, DynamicImage, Rgb, RgbImage};
    use std::fs;

    let test_dir = PathBuf::from("target/test_images");
    let _ = fs::create_dir_all(&test_dir);

    let img1_path = test_dir.join("orig.png");
    let img2_path = test_dir.join("scaled.png");
    let img3_path = test_dir.join("different.png");

    let mut img1 = RgbImage::new(120, 120);
    for (x, y, pixel) in img1.enumerate_pixels_mut() {
        let dist = (((x as i32 - 60).pow(2) + (y as i32 - 60).pow(2)) as f64).sqrt();
        if dist < 40.0 {
            *pixel = Rgb([200, 50, 50]);
        } else {
            *pixel = Rgb([50, 50, 200]);
        }
    }
    img1.save(&img1_path).unwrap();

    let dyn1 = DynamicImage::ImageRgb8(img1);
    let dyn2 = dyn1.resize_exact(60, 60, image::imageops::FilterType::Triangle);
    dyn2.save(&img2_path).unwrap();

    let mut img3 = RgbImage::new(120, 120);
    for (x, _y, pixel) in img3.enumerate_pixels_mut() {
        if x < 60 {
            *pixel = Rgb([0, 255, 0]);
        } else {
            *pixel = Rgb([255, 255, 0]);
        }
    }
    img3.save(&img3_path).unwrap();

    let read_field = |path: &std::path::Path, field: i32| -> (i32, String) {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = [0u16; 256];
        let res = unsafe {
            ContentGetValueW(
                wide.as_ptr(),
                field,
                0,
                buf.as_mut_ptr() as *mut c_void,
                512,
                0,
            )
        };
        let s = String::from_utf16_lossy(&buf);
        let s = s.trim_matches('\0').to_string();
        (res, s)
    };

    let (res1, dhash1) = read_field(&img1_path, 0);
    let (res2, dhash2) = read_field(&img2_path, 0);
    let (res3, dhash3) = read_field(&img3_path, 0);

    assert_eq!(res1, FT_STRINGW);
    assert_eq!(res2, FT_STRINGW);
    assert_eq!(res3, FT_STRINGW);

    assert_eq!(dhash1, dhash2, "Original and scaled image should match dHash");
    assert_ne!(dhash1, dhash3, "Different images must not match dHash");

    let (_, dim1) = read_field(&img1_path, 3);
    assert_eq!(dim1, "120x120");

    let (_, dim2) = read_field(&img2_path, 3);
    assert_eq!(dim2, "60x60");

    let (_, asp1) = read_field(&img1_path, 4);
    assert_eq!(asp1, "1:1");

    let (_, mtype) = read_field(&img1_path, 9);
    assert_eq!(mtype, "Image");

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_delay_if_slow_for_uncached_video_raw_and_psd() {
    use std::os::windows::ffi::OsStrExt;
    for filename in &["target/test_dummy.mp4", "target/test_dummy.cr2", "target/test_dummy.psd"] {
        let path = PathBuf::from(filename);
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();

        let mut buf = [0u16; 128];
        let res = unsafe {
            ContentGetValueW(
                wide.as_ptr(),
                0, // dHash
                0,
                buf.as_mut_ptr() as *mut c_void,
                256,
                CONTENT_DELAYIFSLOW,
            )
        };

        assert_eq!(
            res, FT_DELAYED,
            "Uncached file {} with CONTENT_DELAYIFSLOW must return FT_DELAYED", filename
        );
    }
}

#[test]
fn test_stop_get_value_cancellation() {
    assert!(!mediares_core::wdx_api::is_stop_requested());
    unsafe {
        let dummy = [0u16; 1];
        ContentStopGetValueW(dummy.as_ptr());
    }
    assert!(mediares_core::wdx_api::is_stop_requested());
    mediares_core::wdx_api::reset_stop_flag();
    assert!(!mediares_core::wdx_api::is_stop_requested());
}

#[test]
fn test_corrupt_and_invalid_files_safety() {
    use std::os::windows::ffi::OsStrExt;
    let non_existent = PathBuf::from("target/definitely_not_a_file.xyz");
    let wide: Vec<u16> = non_existent
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();

    let mut buf = [0u16; 128];
    let res =
        unsafe { ContentGetValueW(wide.as_ptr(), 0, 0, buf.as_mut_ptr() as *mut c_void, 256, 0) };

    assert!(res == FT_FIELDEMPTY || res == FT_FILEERROR);
}

/// 16-bit PCM WAV: a chord whose pitch changes every second, after `lead_silence` seconds.
fn write_wav(path: &std::path::Path, rate: u32, seconds: u32, lead_silence: u32, pitch_shift: f32) {
    write_wav_tagged(path, rate, seconds, lead_silence, pitch_shift, &[]);
}

/// Same, with a RIFF `LIST/INFO` chunk of (id, text) tags.
fn write_wav_tagged(path: &std::path::Path, rate: u32, seconds: u32, lead_silence: u32, pitch_shift: f32, info: &[(&[u8; 4], &str)]) {
    let total = rate * (seconds + lead_silence);
    let mut data = Vec::with_capacity(total as usize * 2);
    for i in 0..total {
        let v = if i < lead_silence * rate {
            0.0
        } else {
            // Time from the start of the sound (exact), so padding doesn't change the samples.
            let t = (i - lead_silence * rate) as f32 / rate as f32;
            let f = (220.0 + 55.0 * (t as u32 % 7) as f32) * pitch_shift;
            0.3 * (2.0 * std::f32::consts::PI * f * t).sin() + 0.2 * (2.0 * std::f32::consts::PI * f * 2.5 * t).sin()
        };
        data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
    }
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for v in [16u32.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec()] {
        b.extend_from_slice(&v);
    }
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&data);
    if !info.is_empty() {
        let mut list = b"INFO".to_vec();
        for (id, text) in info {
            let mut value = text.as_bytes().to_vec();
            value.push(0);
            list.extend_from_slice(*id);
            list.extend_from_slice(&(value.len() as u32).to_le_bytes());
            if value.len() % 2 == 1 {
                value.push(0);
            }
            list.extend_from_slice(&value);
        }
        b.extend_from_slice(b"LIST");
        b.extend_from_slice(&(list.len() as u32).to_le_bytes());
        b.extend_from_slice(&list);
    }
    let riff_len = (b.len() - 8) as u32;
    b[4..8].copy_from_slice(&riff_len.to_le_bytes());
    std::fs::write(path, b).unwrap();
}

#[test]
fn test_audio_fields() {
    use std::os::windows::ffi::OsStrExt;
    let dir = std::env::temp_dir().join("mediares_wdx_test_audio");
    let _ = std::fs::create_dir_all(&dir);
    let (a, padded, other) = (dir.join("a.wav"), dir.join("a_padded.wav"), dir.join("other.wav"));
    write_wav(&a, 22050, 40, 0, 1.0);
    write_wav(&padded, 22050, 40, 1, 1.0);
    write_wav(&other, 22050, 40, 0, 1.5);

    let read = |path: &std::path::Path, field: i32| -> (i32, String) {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = [0u16; 256];
        let res = unsafe { ContentGetValueW(wide.as_ptr(), field, 0, buf.as_mut_ptr() as *mut c_void, 512, 0) };
        if res == FT_NUMERIC_32 {
            return (res, i32::from_le_bytes([buf[0] as u8, (buf[0] >> 8) as u8, buf[1] as u8, (buf[1] >> 8) as u8]).to_string());
        }
        (res, String::from_utf16_lossy(&buf).trim_matches('\0').to_string())
    };

    let (res, fp) = read(&a, 11);
    assert_eq!(res, FT_STRINGW);
    assert!(fp.starts_with("40s_"), "{}", fp);
    assert_eq!(read(&a, 13), (FT_NUMERIC_32, "40".to_string()));
    assert_eq!(read(&a, 9).1, "Audio");
    // Leading silence changes neither the PCM hash nor the fingerprint windows' content.
    assert_eq!(read(&a, 12), read(&padded, 12));
    assert_ne!(read(&a, 12), read(&other, 12));
    assert_ne!(fp, read(&other, 11).1);
    // Untagged: no artist/title.
    assert_eq!(read(&a, 14).0, FT_FIELDEMPTY);
}

#[test]
fn test_audio_tag_fields() {
    use std::os::windows::ffi::OsStrExt;
    let dir = std::env::temp_dir().join("mediares_wdx_test_audio");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("tagged.wav");
    let info: &[(&[u8; 4], &str)] = &[(b"IART", "Пикник"), (b"INAM", "Остров"), (b"IPRD", "Иероглиф"), (b"IGNR", "Rock"), (b"ICRD", "1986")];
    write_wav_tagged(&path, 22050, 75, 0, 1.0, info);

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let raw = |field: i32, flags: i32| -> (i32, [u16; 256]) {
        let mut buf = [0u16; 256];
        let res = unsafe { ContentGetValueW(wide.as_ptr(), field, 0, buf.as_mut_ptr() as *mut c_void, 512, flags) };
        (res, buf)
    };
    let text = |field: i32| {
        let (res, buf) = raw(field, 0);
        (res, String::from_utf16_lossy(&buf).trim_matches(' ').to_string())
    };
    // Tag fields are never delayed, unlike the decoding-based ones.
    assert_eq!(raw(15, CONTENT_DELAYIFSLOW).0, FT_STRINGW);
    assert_eq!(raw(11, CONTENT_DELAYIFSLOW).0, FT_DELAYED);

    assert_eq!(text(15), (FT_STRINGW, "Пикник".to_string()));
    assert_eq!(text(16), (FT_STRINGW, "Остров".to_string()));
    assert_eq!(text(17), (FT_STRINGW, "Иероглиф".to_string()));
    assert_eq!(text(22), (FT_STRINGW, "Rock".to_string()));
    assert_eq!(text(14), (FT_STRINGW, "пикник - остров".to_string()));
    let (res, buf) = raw(19, 0);
    assert_eq!((res, buf[0] as u32 | (buf[1] as u32) << 16), (FT_NUMERIC_32, 1986));
    // Length 75 s as ttimeformat (h, m, s).
    let (res, buf) = raw(24, 0);
    assert_eq!((res, &buf[..3]), (FT_TIME, &[0u16, 1, 15][..]));
    let (res, buf) = raw(26, 0);
    assert_eq!((res, buf[0] as u32 | (buf[1] as u32) << 16), (FT_NUMERIC_32, 22050));
    assert_eq!(raw(29, 0).0, FT_BOOLEAN);
    assert_eq!(raw(29, 0).1[0], 0, "no cover");
    assert_eq!(raw(20, 0).0, FT_FIELDEMPTY, "no track number");
    // Not audio: tag fields are empty.
    let img: Vec<u16> = std::path::Path::new("Cargo.toml").as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buf = [0u16; 64];
    assert_eq!(unsafe { ContentGetValueW(img.as_ptr(), 15, 0, buf.as_mut_ptr() as *mut c_void, 128, 0) }, FT_FIELDEMPTY);
}
