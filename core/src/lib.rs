//! Mediares core shared library: decoding, hashing, metadata and the WDX FFI glue.
//! Knows nothing about Win32 windows — UI lives in the `combo` crate.

#[cfg(feature = "audio-decode")]
pub mod audio_decode;
#[cfg(feature = "audio-decode")]
pub mod audio_fingerprint;
#[cfg(feature = "tags")]
pub mod audio_tags;
pub mod cache;
pub mod exif;
pub mod ffi;
pub mod hashing;
pub mod heif;
pub mod image_decode;
pub mod jpeg;
pub mod mf_audio;
pub mod mf_init;
pub mod orientation;
pub mod probe;
#[cfg(feature = "psd-preview")]
pub mod psd_preview;
#[cfg(feature = "raw-preview")]
pub mod raw_preview;
pub mod svg;
mod svg_css;
pub mod tc_api;
pub mod video_frame;
pub mod video_tags;
pub mod wdx_api;
pub mod wic_decode;

pub use image;

#[cfg(test)]
pub(crate) mod test_util {
    use std::path::PathBuf;

    /// A 16-bit PCM WAV file in the temp folder; `sample(frame)` is the value of every channel.
    pub fn pcm16_wav(
        name: &str,
        rate: u32,
        channels: u16,
        frames: u32,
        sample: impl Fn(u32) -> i16,
    ) -> PathBuf {
        let data_len = frames * channels as u32 * 2;
        let mut b = Vec::with_capacity(44 + data_len as usize);
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        b.extend_from_slice(&(channels * 2).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let v = sample(i).to_le_bytes();
            for _ in 0..channels {
                b.extend_from_slice(&v);
            }
        }
        let path =
            std::env::temp_dir().join(format!("mediares_{}_{}.wav", name, std::process::id()));
        std::fs::write(&path, b).unwrap();
        path
    }

    /// An uncompressed AVI (24-bit RGB, every frame a key frame) in the temp folder;
    /// `luma(frame, x, y)` is the gray level of each pixel. `width` must be a multiple of 4.
    pub fn gray_avi(
        name: &str,
        (width, height): (u32, u32),
        fps: u32,
        frames: u32,
        luma: impl Fn(u32, u32, u32) -> u8,
    ) -> PathBuf {
        let chunk = |id: &[u8; 4], body: &[u8]| {
            let mut c = id.to_vec();
            c.extend_from_slice(&(body.len() as u32).to_le_bytes());
            c.extend_from_slice(body);
            c
        };
        let list = |kind: &[u8; 4], body: &[u8]| chunk(b"LIST", &[&kind[..], body].concat());
        let u32s = |v: &[u32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let frame_size = width * height * 3;

        // avih: µs per frame, max bytes/s, padding, flags (HASINDEX), frames, initial frames,
        // streams, suggested buffer, width, height, 4 reserved.
        let avih = u32s(&[
            1_000_000 / fps,
            frame_size * fps,
            0,
            0x10,
            frames,
            0,
            1,
            frame_size,
            width,
            height,
            0,
            0,
            0,
            0,
        ]);
        // strh: type, handler, flags, priority+language, initial frames, scale, rate, start,
        // length, suggested buffer, quality, sample size, frame rectangle.
        let mut strh = b"vidsDIB ".to_vec();
        strh.extend(u32s(&[0, 0, 0, 1, fps, 0, frames, frame_size, u32::MAX, 0]));
        strh.extend(
            [0u16, 0, width as u16, height as u16]
                .iter()
                .flat_map(|v| v.to_le_bytes()),
        );
        // BITMAPINFOHEADER, bottom-up 24-bit BI_RGB.
        let mut strf = u32s(&[40, width, height]);
        strf.extend_from_slice(&1u16.to_le_bytes());
        strf.extend_from_slice(&24u16.to_le_bytes());
        strf.extend(u32s(&[0, frame_size, 0, 0, 0, 0]));
        let hdrl = list(
            b"hdrl",
            &[
                chunk(b"avih", &avih),
                list(
                    b"strl",
                    &[chunk(b"strh", &strh), chunk(b"strf", &strf)].concat(),
                ),
            ]
            .concat(),
        );

        let mut movi = Vec::new();
        let mut idx1 = Vec::new();
        for f in 0..frames {
            let mut pixels = Vec::with_capacity(frame_size as usize);
            for y in (0..height).rev() {
                for x in 0..width {
                    pixels.extend_from_slice(&[luma(f, x, y); 3]);
                }
            }
            // Offsets are relative to the `movi` FOURCC.
            idx1.extend_from_slice(b"00db");
            idx1.extend(u32s(&[0x10, 4 + movi.len() as u32, frame_size]));
            movi.extend(chunk(b"00db", &pixels));
        }
        let body = [
            &b"AVI "[..],
            &hdrl,
            &list(b"movi", &movi),
            &chunk(b"idx1", &idx1),
        ]
        .concat();
        let path =
            std::env::temp_dir().join(format!("mediares_{}_{}.avi", name, std::process::id()));
        std::fs::write(&path, chunk(b"RIFF", &body)).unwrap();
        path
    }
}
