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

    // Field 11: out of bounds
    let f11 = unsafe { ContentGetSupportedField(11, field_name.as_mut_ptr(), units.as_mut_ptr(), 128) };
    assert_eq!(f11, FT_NOMOREFIELDS);
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
    assert!(!detect.contains(r#"EXT="MP3""#), "audio is not analyzed by the WDX");
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
