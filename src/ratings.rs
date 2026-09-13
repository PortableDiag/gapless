//! Per-track star ratings, 1–5, persisted to ~/.config/gapless/ratings.json.
//!
//! **Not a tag write.** Ratings could live in the files themselves — ID3 `POPM`,
//! a Vorbis `RATING` comment — and some players do that. This one does not, for
//! two reasons:
//!
//!   1. Rating a song would mean rewriting the user's audio file. A player whose
//!      whole argument is "your rips already have silence baked into them" should
//!      not be the thing that also rewrites them.
//!   2. `POPM` has no agreed scale. Windows Media, Banshee, MediaMonkey and
//!      foobar2000 each map 1–5 stars onto 0–255 differently, so a number read
//!      back is only meaningful if you already know who wrote it.
//!
//! So: a sidecar file, keyed by absolute path, in the same config dir as
//! `state.json` — and deliberately a **separate file**. `state.json` is session
//! state, rewritten every few seconds while playing and again from the SIGTERM
//! handler. Ratings are the one thing here the user typed in by hand; they do
//! not belong in the file with the highest write rate and the most ways to die
//! mid-write.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 0 means unrated, and is never stored — see `set`.
pub type Stars = u8;

pub const MAX: Stars = 5;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Ratings {
    /// Absolute path -> 1..=5. Paths, not indices: a rescan renumbers the queue,
    /// and the same reasoning that keeps the resume point path-keyed applies with
    /// more force to data the user entered.
    #[serde(default)]
    stars: HashMap<PathBuf, Stars>,
}

fn ratings_path() -> Option<PathBuf> {
    let dir = glib::user_config_dir().join("gapless");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("ratings.json"))
}

impl Ratings {
    pub fn load() -> Self {
        let Some(path) = ratings_path() else {
            return Self::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Written through a temporary file and renamed. This is the one store whose
    /// contents cannot be reconstructed from anything else on disk, so a crash
    /// during the write must not be able to truncate it.
    pub fn save(&self) {
        let Some(path) = ratings_path() else { return };
        let Ok(json) = serde_json::to_string_pretty(self) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, json) {
            eprintln!("could not save ratings: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            eprintln!("could not save ratings: {e}");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub fn get(&self, path: &Path) -> Stars {
        self.stars.get(path).copied().unwrap_or(0).min(MAX)
    }

    /// Anything outside 1..=5 clears the rating and removes the entry entirely,
    /// so "unrated" is the absence of a key rather than a zero someone has to
    /// remember to special-case.
    pub fn set(&mut self, path: &Path, stars: Stars) {
        if stars == 0 || stars > MAX {
            self.stars.remove(path);
        } else {
            self.stars.insert(path.to_path_buf(), stars);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.stars.is_empty()
    }

    pub fn len(&self) -> usize {
        self.stars.len()
    }
}

/// "★★★☆☆" for 3, empty string for unrated — an unrated track in a list of
/// hundreds should be blank, not a row of grey holes competing for attention.
pub fn stars_text(stars: Stars) -> String {
    if stars == 0 || stars > MAX {
        return String::new();
    }
    let filled = stars as usize;
    "★".repeat(filled) + &"☆".repeat(MAX as usize - filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let mut r = Ratings::default();
        r.set(Path::new("/music/03 Get to the Choppa.mp3"), 5);
        r.set(Path::new("/music/04 Broken Arrow.mp3"), 2);

        let back: Ratings = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.get(Path::new("/music/03 Get to the Choppa.mp3")), 5);
        assert_eq!(back.get(Path::new("/music/04 Broken Arrow.mp3")), 2);
        assert_eq!(back.get(Path::new("/music/nobody rated this.mp3")), 0);
    }

    /// Clearing must remove the key, not leave a zero behind — otherwise the file
    /// grows a permanent entry for every track the user ever changed their mind
    /// about.
    #[test]
    fn zero_clears_rather_than_stores() {
        let mut r = Ratings::default();
        let p = Path::new("/music/mistake.flac");
        r.set(p, 4);
        assert_eq!(r.len(), 1);
        r.set(p, 0);
        assert_eq!(r.get(p), 0);
        assert!(r.is_empty(), "a cleared rating must not linger as a stored zero");
    }

    #[test]
    fn out_of_range_is_refused_not_clamped_into_a_lie() {
        let mut r = Ratings::default();
        let p = Path::new("/music/six stars.flac");
        r.set(p, 9);
        assert_eq!(r.get(p), 0, "9 stars is not a rating; it must not become 5");
    }

    /// An empty or corrupt file must read as "nothing rated yet", not panic and
    /// not throw away the app's startup.
    #[test]
    fn garbage_reads_as_empty() {
        let r: Ratings = serde_json::from_str(r#"{"stars":{}}"#).unwrap();
        assert!(r.is_empty());
        assert!(serde_json::from_str::<Ratings>("not json at all").is_err());
    }

    #[test]
    fn star_text_is_blank_when_unrated() {
        assert_eq!(stars_text(0), "");
        assert_eq!(stars_text(3), "★★★☆☆");
        assert_eq!(stars_text(5), "★★★★★");
        assert_eq!(stars_text(7), "");
    }
}
