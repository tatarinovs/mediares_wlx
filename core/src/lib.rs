//! Mediares core shared library: decoding, hashing, metadata and the WDX FFI glue.
//! Knows nothing about Win32 windows — UI lives in the `combo` crate.

#[cfg(feature = "audio-decode")]
pub mod audio_decode;
#[cfg(feature = "tags")]
pub mod audio_tags;
pub mod cache;
pub mod exif;
pub mod ffi;
pub mod hashing;
pub mod image_decode;
mod jpeg;
pub mod mf_init;
pub mod probe;
#[cfg(feature = "psd-preview")]
pub mod psd_preview;
#[cfg(feature = "raw-preview")]
pub mod raw_preview;
pub mod tc_api;
pub mod video_frame;
pub mod wdx_api;

pub use image;
