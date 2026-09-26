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
mod jpeg;
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
