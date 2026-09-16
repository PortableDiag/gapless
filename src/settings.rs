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
    /// Superseded by `shuffle_mode`, which has three states rather than two.
    /// Still written on every save — an older build, or `verify-mpris-modes.sh`,
    /// reads this key and nothing else — and still read when `shuffle_mode` is
    /// absent, so a config written before favorites shuffle existed migrates
    /// instead of silently turning shuffle off.
    pub shuffle: bool,
    /// "off" | "on" | "favorites". `None` means this config predates the field.
    pub shuffle_mode: Option<String>,
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
    /// The local HTTP control API. Off by default: a control socket nobody asked
    /// for should not appear because somebody installed a music player.
    pub api_enabled: bool,
    /// Port for that API, on 127.0.0.1. See `api::DEFAULT_PORT`.
    pub api_port: u16,
    /// Force Tempo: bring every track to one pace. Off by default — it changes
    /// what the music sounds like, and a player should not start doing that on
    /// its own.
    pub force_tempo: bool,
    /// The tempo to bring tracks to, in BPM.
    pub target_bpm: u32,
    /// The largest stretch allowed, as a percentage. See `tempo::speed_for`.
    pub max_stretch_percent: u32,
    /// Never play a track slower than it was recorded. On by default: the point
    /// of the feature is a workout with no slow patches in it.
    pub only_faster: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            last_source: None,
            last_folder: None,
            volume: 1.0,
            repeat: "off".into(),
            shuffle: false,
            shuffle_mode: None,
            trim_silence: true,
            crossfade_secs: 0.0,
            inner_silence_secs: 0.0,
            last_track: None,
            last_position_secs: 0.0,
            api_enabled: false,
            api_port: crate::api::DEFAULT_PORT,
            force_tempo: false,
            target_bpm: crate::tempo::DEFAULT_TARGET_BPM,
            max_stretch_percent: crate::tempo::DEFAULT_MAX_CHANGE_PERCENT,
            only_faster: crate::tempo::DEFAULT_ONLY_FASTER,
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
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(settings) => settings,
            Err(e) => {
                // SAY SO. One field of the wrong type fails the whole parse, and
                // falling back to defaults silently resets *everything* — volume,
                // the resume point, the API port. That is indistinguishable from
                // the file having been deleted, and it cost a confusing debugging
                // session here when a hand-written `"shuffle": "off"` (it is a
                // bool) quietly moved the control API back to its default port.
                eprintln!(
                    "gapless: {} is not valid ({e}); starting from defaults and \
                     leaving the file alone so it can be inspected",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Ok(json) = serde_json::to_string_pretty(self) {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("could not save settings: {e}");
            }
        }
    }

    /// The two shuffle keys reconciled: the three-state one wins when present,
    /// the old bool stands in when it is not. Returned as text so `settings` does
    /// not have to depend on `player`.
    pub fn shuffle_mode_str(&self) -> &str {
        match self.shuffle_mode.as_deref() {
            Some(m) => m,
            None if self.shuffle => "on",
            None => "off",
        }
    }

    /// Sets both keys from the three-state value, so they can never disagree.
    pub fn set_shuffle_mode(&mut self, mode: &str) {
        self.shuffle = mode != "off";
        self.shuffle_mode = Some(mode.to_string());
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

    /// The migration that matters: a config from before favorites shuffle has
    /// only the bool. `shuffle: true` must come back as plain shuffle, not as
    /// "off" — the failure mode of adding a new field and reading only that one.
    #[test]
    fn old_bool_shuffle_migrates_to_a_mode() {
        let old = r#"{ "shuffle": true }"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.shuffle_mode_str(), "on");

        let off = r#"{ "shuffle": false }"#;
        let s: Settings = serde_json::from_str(off).unwrap();
        assert_eq!(s.shuffle_mode_str(), "off");
    }

    /// And the other direction: the bool must keep being written, because
    /// `verify-mpris-modes.sh` and any older install read that key alone.
    #[test]
    fn setting_a_mode_keeps_the_old_bool_in_step() {
        let mut s = Settings::default();
        s.set_shuffle_mode("favorites");
        assert!(s.shuffle, "the legacy bool must say shuffle is on");
        assert_eq!(s.shuffle_mode_str(), "favorites");

        s.set_shuffle_mode("off");
        assert!(!s.shuffle);
        assert_eq!(s.shuffle_mode_str(), "off");
    }

    /// A field of the wrong type must not be mistaken for a valid config. The
    /// caller gets defaults, but the *user* gets told — see `load`.
    #[test]
    fn a_wrong_type_fails_the_parse_rather_than_being_ignored() {
        // `shuffle` is a bool; a string is a type error, not a coercion.
        let bad = r#"{ "volume": 0.5, "shuffle": "off" }"#;
        assert!(
            serde_json::from_str::<Settings>(bad).is_err(),
            "a wrong type must be an error, so `load` can report it instead of \
             silently resetting every other setting too"
        );

        // And the same file with the right type keeps everything.
        let good = r#"{ "volume": 0.5, "shuffle": false, "api_port": 18441 }"#;
        let s: Settings = serde_json::from_str(good).unwrap();
        assert_eq!(s.volume, 0.5);
        assert_eq!(s.api_port, 18441);
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
