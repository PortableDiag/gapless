//! Filesystem scan + tag read. Deliberately dumb for now: no database, no
//! watcher. Swap in rusqlite once the playback side is settled.

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::{Accessor, ItemKey};
use lofty::probe::Probe;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub year: Option<u32>,
    pub genre: Option<String>,
    /// Used to keep albums in playing order, which is the whole point of
    /// gapless — a shuffled album is a gapless album nobody wanted.
    pub disc: u32,
    pub track_no: u32,
    pub duration_nanos: u64,
    /// "FLAC · 44.1 kHz · 16-bit · 1006 kbps" — assembled at scan time from the
    /// decoder's own view of the file, not from the tags, which lie.
    pub format: String,
}

/// Embedded cover art. Read lazily on track change rather than at scan time —
/// a few thousand tracks' worth of JPEGs is hundreds of MB we'd never look at.
pub fn cover_art(path: &Path) -> Option<Vec<u8>> {
    let tagged = Probe::open(path).ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let picture = tag.pictures().first()?;
    Some(picture.data().to_vec())
}

/// Reads tags for an explicit list of paths, in the order given. Used by the
/// playlist loader — deliberately does NOT sort, unlike `scan`.
pub fn tracks_from_paths(paths: &[PathBuf]) -> Vec<Track> {
    paths.iter().map(|p| read_track(p)).collect()
}

const EXTENSIONS: &[&str] =
    &["mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "wav", "wv", "ape", "mpc"];

pub fn scan(root: &Path) -> Vec<Track> {
    let mut tracks: Vec<Track> = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| EXTENSIONS.contains(&x.to_ascii_lowercase().as_str()))
                .unwrap_or(false)
        })
        .map(|e| read_track(e.path()))
        .collect();

    tracks.sort_by(|a, b| {
        a.album
            .cmp(&b.album)
            .then(a.disc.cmp(&b.disc))
            .then(a.track_no.cmp(&b.track_no))
            .then(a.path.cmp(&b.path))
    });
    tracks
}

pub fn read_track(path: &Path) -> Track {
    let fallback_title = || {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string()
    };

    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_uppercase();

    let Some(tagged) = Probe::open(path).ok().and_then(|p| p.read().ok()) else {
        return Track {
            path: path.to_path_buf(),
            title: fallback_title(),
            artist: "Unknown Artist".into(),
            album: "Unknown Album".into(),
            year: None,
            genre: None,
            disc: 1,
            track_no: 0,
            duration_nanos: 0,
            format: ext,
        };
    };

    let props = tagged.properties();
    let duration_nanos = props.duration().as_nanos() as u64;

    let mut parts = vec![ext];
    if let Some(hz) = props.sample_rate() {
        parts.push(format!("{:.1} kHz", hz as f64 / 1000.0));
    }
    if let Some(bits) = props.bit_depth() {
        parts.push(format!("{bits}-bit"));
    }
    if let Some(kbps) = props.audio_bitrate() {
        parts.push(format!("{kbps} kbps"));
    }
    match props.channels() {
        Some(1) => parts.push("Mono".into()),
        Some(2) => parts.push("Stereo".into()),
        Some(n) => parts.push(format!("{n} ch")),
        None => {}
    }

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let (title, artist, album, year, genre, disc, track_no) = match tag {
        Some(t) => (
            t.title().map(|s| s.to_string()).unwrap_or_else(fallback_title),
            t.artist().map(|s| s.to_string()).unwrap_or_else(|| "Unknown Artist".into()),
            t.album().map(|s| s.to_string()).unwrap_or_else(|| "Unknown Album".into()),
            t.year(),
            t.genre().map(|s| s.to_string()),
            t.get_string(&ItemKey::DiscNumber).and_then(|s| s.parse().ok()).unwrap_or(1),
            t.track().unwrap_or(0),
        ),
        None => (
            fallback_title(),
            "Unknown Artist".into(),
            "Unknown Album".into(),
            None,
            None,
            1,
            0,
        ),
    };

    Track {
        path: path.to_path_buf(),
        title,
        artist,
        album,
        year,
        genre,
        disc,
        track_no,
        duration_nanos,
        format: parts.join(" · "),
    }
}
