//! What the tempo estimator makes of a file, and how sure it is.
//!
//!   cargo run --release --example bpm-info -- track.mp3 [more.flac ...]
//!
//! Prints the tag's tempo, the measured one, the reading before the octave
//! tie-break, and the confidence. The confidence column is how `MIN_CONFIDENCE`
//! in `src/bpm.rs` was chosen: run this over a pile of real music and a pile of
//! things with no beat in them, and put the threshold in the gap.

use anyhow::Result;
use gapless::bpm;
use std::path::PathBuf;

fn main() -> Result<()> {
    let files: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if files.is_empty() {
        eprintln!("usage: bpm-info TRACK...");
        std::process::exit(2);
    }

    gst::init()?;

    println!("{:<46} {:>7} {:>9} {:>9} {:>6}", "file", "tag", "measured", "pre-fold", "conf");
    println!("{}", "-".repeat(82));

    for path in &files {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let short: String = name.chars().take(45).collect();

        let tag = bpm::from_tag(path)
            .map(bpm::format_bpm)
            .unwrap_or_else(|| "-".into());

        match bpm::analyze(path) {
            Ok(r) => {
                let measured = r.bpm.map(bpm::format_bpm).unwrap_or_else(|| "none".into());
                let raw = r.raw_bpm.map(bpm::format_bpm).unwrap_or_else(|| "-".into());
                println!(
                    "{short:<46} {tag:>7} {measured:>9} {raw:>9} {:>6.3}",
                    r.confidence
                );
            }
            Err(e) => {
                println!("{short:<46} {tag:>7} {:>9} {:>9} {:>6}", "ERR", "-", "-");
                eprintln!("  {name}: {e}");
            }
        }
    }

    Ok(())
}
