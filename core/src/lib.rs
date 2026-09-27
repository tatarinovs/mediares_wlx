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
pub mod image_decode;
#[cfg(any(feature = "raw-preview", feature = "psd-preview"))]
pub mod jpeg;
pub mod mf_audio;
pub mod mf_init;
pub mod probe;
#[cfg(feature = "psd-preview")]
pub mod psd_preview;
#[cfg(feature = "raw-preview")]
pub mod raw_preview;
pub mod tc_api;
pub mod video_frame;
pub mod video_tags;
pub mod wdx_api;

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
}
