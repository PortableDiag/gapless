//! M3U / M3U8 / PLS parsing.
//!
//! The one rule that matters: **preserve file order**. The library scanner sorts
//! by album/disc/track, which is right for a folder and catastrophic for a
//! playlist — a hand-sequenced set is the single case where gapless playback
//! matters most, and re-sorting it destroys the sequence.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Playlist {
    pub name: String,
    /// Entries that exist on disk, in playlist order.
    pub tracks: Vec<PathBuf>,
    /// Entries that don't. Kept so we can say so rather than silently shrinking
    /// the playlist — and kept *out* of the queue, because a dead path in the
    /// gapless handoff stalls the pipeline mid-album.
    pub missing: Vec<PathBuf>,
    /// http(s) entries. playbin could stream them, but lofty can't tag-read
    /// them, so they're out of scope until the library grows a remote path.
    pub remote: usize,
}

pub fn parse(path: &Path) -> Result<Playlist> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("could not read {}: {e}", path.display()))?;

    let base = path.parent().unwrap_or(Path::new("."));
    let is_pls = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pls"))
        .unwrap_or(false);

    let mut playlist = Playlist {
        name: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Playlist")
            .to_string(),
        ..Default::default()
    };

    for raw in text.lines() {
        // Strip a UTF-8 BOM off the first line if the file has one.
        let line = raw.trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }

        let entry = if is_pls {
            // PLS is an ini: only File1=..., File2=... are paths.
            match line.split_once('=') {
                Some((key, value)) if key.trim().to_ascii_lowercase().starts_with("file") => value,
                _ => continue,
            }
        } else {
            // M3U: '#' lines are comments and #EXTINF metadata. We take titles
            // from the files' own tags, so there's nothing here we need.
            if line.starts_with('#') {
                continue;
            }
            line
        };

        let entry = entry.trim();
        if entry.starts_with("http://") || entry.starts_with("https://") {
            playlist.remote += 1;
            continue;
        }

        match resolve(entry, base) {
            Some(p) => playlist.tracks.push(p),
            None => playlist.missing.push(PathBuf::from(entry)),
        }
    }

    Ok(playlist)
}

/// Playlist paths may be absolute or relative to the playlist's own directory.
/// Windows-authored playlists use backslashes, which are legal characters in a
/// Linux filename — so only try that interpretation if the literal path missed.
fn resolve(entry: &str, base: &Path) -> Option<PathBuf> {
    let candidates = [
        PathBuf::from(entry),
        base.join(entry),
        PathBuf::from(entry.replace('\\', "/")),
        base.join(entry.replace('\\', "/")),
    ];
    candidates.into_iter().find(|p| p.is_file())
}
