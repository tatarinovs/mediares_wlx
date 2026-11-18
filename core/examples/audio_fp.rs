//! Prints the audio duplicate-detection fields of the given files (tab-separated), with timing.
//!
//! ```text
//! cargo run --release -p mediares_core --features audio-decode,tags --example audio_fp -- <files...>
//! ```

use std::path::Path;
use std::time::Instant;

use mediares_core::audio_fingerprint::{analyze_audio, fast_fingerprint_audio};
use mediares_core::audio_tags::read_tags;

fn main() {
    for arg in std::env::args().skip(1) {
        let path = Path::new(&arg);
        let t0 = Instant::now();
        let full = analyze_audio(path, &|| false);
        let full_ms = t0.elapsed().as_millis();

        let t1 = Instant::now();
        let fast = fast_fingerprint_audio(path, &|| false);
        let fast_ms = t1.elapsed().as_millis();

        match (full, fast) {
            (Ok(a), Ok(f)) => println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{} ms / fast {} ms",
                arg,
                a.duration_sec,
                a.pcm_hash,
                a.fingerprint.as_deref().unwrap_or("-"),
                f.fingerprint.as_deref().unwrap_or("-"),
                read_tags(path, false)
                    .and_then(|t| t.normalized_artist_title())
                    .unwrap_or_else(|| "-".into()),
                full_ms,
                fast_ms,
            ),
            (Err(e), _) | (_, Err(e)) => println!("{}\terror: {:?}", arg, e),
        }
    }
}
