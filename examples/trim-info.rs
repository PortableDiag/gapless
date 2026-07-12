//! Prints what the silence analyser thinks about a file.
//!
//!   cargo run --release --example trim-info -- track.mp3 ...

use anyhow::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    gst::init()?;
    for arg in std::env::args().skip(1) {
        let path = PathBuf::from(&arg);
        match gapless::silence::analyze(&path) {
            Ok(t) => {
                let ms = |n: u64| n as f64 / 1e6;
                println!(
                    "{:>9.1} ms head   {:>9.1} ms tail   audible {:>9.1} ms of {:>9.1} ms   {}",
                    ms(t.start),
                    ms(t.total.saturating_sub(t.end)),
                    ms(t.len()),
                    ms(t.total),
                    path.file_name().unwrap_or_default().to_string_lossy(),
                );
            }
            Err(e) => println!("FAILED {}: {e}", path.display()),
        }
    }
    Ok(())
}
