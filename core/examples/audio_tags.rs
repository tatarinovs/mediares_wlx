//! Prints the tag-based WDX audio fields of the given files.
//!
//! ```text
//! cargo run -p mediares_core --features tags --example audio_tags -- <files...>
//! ```

fn main() {
    for arg in std::env::args().skip(1) {
        match mediares_core::audio_tags::read_tags(std::path::Path::new(&arg), false) {
            Some(t) => println!(
                "{}\t{}\tlossless={:?}\tcomposer={:?}\ttrack={:?}/{:?}\tdisc={:?}/{:?}\t{:?} Hz {:?} bit {:?} ch {:?} kbps",
                arg,
                t.codec.unwrap_or("-"),
                t.lossless,
                t.composer,
                t.track,
                t.track_total,
                t.disc,
                t.disc_total,
                t.sample_rate,
                t.bit_depth,
                t.channels,
                t.bitrate_kbps
            ),
            None => println!("{}\tunreadable", arg),
        }
    }
}
