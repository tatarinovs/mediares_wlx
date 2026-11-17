//! Prints the audio duplicate-detection fields of the given files (tab-separated), with timing.
//!
//! ```text
//! cargo run --release -p mediares_core --features audio-decode,tags --example audio_fp -- <files...>
//! ```

use std::path::Path;
use std::time::Instant;

use mediares_core::audio_fingerprint::{fingerprint_audio, hash_audio_pcm};
use mediares_core::audio_tags::read_tags;

fn main() {
    for arg in std::env::args().skip(1) {
        let started = Instant::now();
        match (
            fingerprint_audio(Path::new(&arg), &|| false),
            hash_audio_pcm(Path::new(&arg), &|| false),
        ) {
            (Ok(a), Ok(pcm)) => println!(
                "{}\t{}\t{}\t{}\t{}\t{} ms",
                arg,
                a.duration_sec,
                pcm.pcm_hash,
                a.fingerprint.as_deref().unwrap_or("-"),
                read_tags(Path::new(&arg), false)
                    .and_then(|t| t.normalized_artist_title())
                    .unwrap_or_else(|| "-".into()),
                started.elapsed().as_millis()
            ),
            (Err(e), _) | (_, Err(e)) => println!("{}\terror: {:?}", arg, e),
        }
    }
}
