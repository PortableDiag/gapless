//! Prints a parsed playlist in playing order, one path per line.
//!
//!   cargo run --release --example dump-playlist -- some.m3u8
//!
//! Diff it against the source file to confirm order survived the round trip.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let arg = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: dump-playlist PLAYLIST"))?;
    let pl = gapless::playlist::parse(&PathBuf::from(arg))?;

    eprintln!(
        "name={:?}  tracks={}  missing={}  remote={}",
        pl.name,
        pl.tracks.len(),
        pl.missing.len(),
        pl.remote
    );
    for path in &pl.tracks {
        println!("{}", path.display());
    }
    for path in &pl.missing {
        eprintln!("MISSING: {}", path.display());
    }
    Ok(())
}
