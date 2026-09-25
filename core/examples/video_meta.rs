//! Prints the WDX video metadata for the given files.
fn main() {
    for arg in std::env::args().skip(1) {
        println!("{}: {:?}", arg, mediares_core::video_frame::probe_video_meta(std::path::Path::new(&arg)));
    }
}
