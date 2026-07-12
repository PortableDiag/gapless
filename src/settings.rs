//! Session state persisted to ~/.config/gapless/state.json.
//!
//! Deliberately not GSettings: that needs a compiled schema installed system-
//! wide, which makes `cargo run` from a source tree fail in a confusing way.
//! A JSON file in the XDG config dir has none of that ceremony.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// A folder or a playlist file — `load_source` tells them apart by asking
    /// the filesystem, so one field covers both.
    pub last_source: Option<PathBuf>,
    /// Superseded by `last_source`; still read so existing configs migrate.
    pub last_folder: Option<PathBuf>,
    pub volume: f64,
    /// "off" | "all" | "one"
    pub repeat: String,
    pub shuffle: bool,
    /// Skip silence recorded at track edges. On by default: it is what makes a
    /// non-gapless rip sound gapless, and it is what most people actually want.
    pub trim_silence: bool,
    /// 0 = gapless. Winamp-style overlap, in seconds.
    pub crossfade_secs: f64,
    /// Cap on silence left *inside* a track, in seconds. 0 = leave tracks alone.
    pub inner_silence_secs: f64,
    /// Where we were when the app last closed. Stored as a **path**, not an
    /// index: a folder rescan or an edited playlist renumbers the queue, and
    /// resuming into whatever track happens to sit at index 12 today is worse
    /// than not resuming at all.
    pub last_track: Option<PathBuf>,
    pub last_position_secs: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            last_source: None,
            last_folder: None,
            volume: 1.0,
            repeat: "off".into(),
            shuffle: false,
            trim_silence: true,
            crossfade_secs: 0.0,
            inner_silence_secs: 0.0,
            last_track: None,
            last_position_secs: 0.0,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let dir = glib::user_config_dir().join("gapless");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("state.json"))
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Ok(json) = serde_json::to_string_pretty(self) {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("could not save settings: {e}");
            }
        }
    }

    /// A remembered source that has since been unmounted or deleted must not
    /// resurrect as an empty library with no explanation.
    pub fn valid_last_source(&self) -> Option<&Path> {
        self.last_source
            .as_deref()
            .or(self.last_folder.as_deref())
            .filter(|p| p.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config written by an older build has no `last_track` / `last_position_secs`.
    /// It must load, keep every setting it *does* carry, and default the rest —
    /// not fail to parse and silently reset the user's volume and repeat mode.
    #[test]
    fn old_config_still_loads() {
        let old = r#"{
            "last_source": "/home/null/Music/Austrian Death Machine/adm.m3u8",
            "volume": 0.42,
            "repeat": "all",
            "shuffle": true,
            "trim_silence": true,
            "crossfade_secs": 3.0,
            "inner_silence_secs": 0.0
        }"#;
        let s: Settings = serde_json::from_str(old).expect("old config must still parse");
        assert_eq!(s.volume, 0.42);
        assert_eq!(s.repeat, "all");
        assert!(s.shuffle);
        assert_eq!(s.crossfade_secs, 3.0);
        assert_eq!(s.last_track, None);
        assert_eq!(s.last_position_secs, 0.0);
    }

    #[test]
    fn resume_point_round_trips() {
        let mut s = Settings::default();
        s.last_track = Some(PathBuf::from("/music/03 Get to the Choppa.mp3"));
        s.last_position_secs = 87.5;
        s.shuffle = true;
        s.repeat = "one".into();
        s.volume = 0.7;

        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.last_track, s.last_track);
        assert_eq!(back.last_position_secs, 87.5);
        assert!(back.shuffle);
        assert_eq!(back.repeat, "one");
        assert_eq!(back.volume, 0.7);
    }
}
