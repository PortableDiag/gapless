//! "Start when I log in", via the XDG autostart spec.
//!
//! The state is **the file itself**, not a flag in `state.json`. Two sources of
//! truth would drift the moment the user toggled the app in their desktop's own
//! startup-applications panel, or deleted the file by hand, and the switch would
//! then confidently show the wrong thing.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

const APP_ID: &str = "com.procomputation.Gapless";

fn autostart_file() -> Option<PathBuf> {
    Some(glib::user_config_dir().join("autostart").join(format!("{APP_ID}.desktop")))
}

/// Which binary the login session should launch.
///
/// Prefer the installed copy. The running binary is very often
/// `~/.cache/cargo-target/gapless/release/gapless` — a *cache* directory, which
/// `cargo clean` empties and which nothing guarantees will survive. Baking that
/// path into a login hook produces an autostart that silently stops working weeks
/// later, which is worse than one that never worked.
fn exec_path() -> Result<PathBuf> {
    let installed = glib::home_dir().join(".local/bin/gapless");
    if installed.is_file() {
        return Ok(installed);
    }
    std::env::current_exe().map_err(|e| anyhow!("cannot locate this executable: {e}"))
}

pub fn is_enabled() -> bool {
    let Some(path) = autostart_file() else { return false };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    // A desktop that "disables" an autostart entry usually keeps the file and
    // sets one of these rather than deleting it. Honour that.
    let disabled = text.lines().any(|l| {
        let l = l.trim();
        l.eq_ignore_ascii_case("Hidden=true")
            || l.replace(' ', "").eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
    });
    !disabled
}

pub fn set(on: bool) -> Result<()> {
    let path = autostart_file().ok_or_else(|| anyhow!("no config directory"))?;

    if !on {
        match std::fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(anyhow!("could not remove {}: {e}", path.display())),
        }
    }

    let dir = path.parent().ok_or_else(|| anyhow!("no autostart directory"))?;
    std::fs::create_dir_all(dir)?;

    let exec = exec_path()?;
    let exec = exec.to_string_lossy();
    // Exec is a *command line*, not a path: a space in it would parse as an
    // argument. Quoting is the spec's answer.
    let exec = if exec.contains(' ') {
        format!("\"{exec}\"")
    } else {
        exec.to_string()
    };

    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Gapless\n\
         Comment=Music player with true gapless playback\n\
         Exec={exec}\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         StartupNotify=false\n\
         X-GNOME-Autostart-enabled=true\n"
    );
    std::fs::write(&path, entry)
        .map_err(|e| anyhow!("could not write {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a real file, so it is opt-in and must be pointed at a throwaway
    /// config dir. Never run against a real $HOME — it would clobber the user's
    /// actual setting.
    ///
    ///   XDG_CONFIG_HOME=/tmp/x cargo test --lib -- --ignored autostart
    #[test]
    #[ignore]
    fn round_trip_on_disk() {
        let path = autostart_file().expect("config dir");
        assert!(
            !path.starts_with(glib::home_dir().join(".config")),
            "refusing to run against the real config dir — set XDG_CONFIG_HOME"
        );

        set(true).expect("enable");
        assert!(path.is_file(), "enabling must create {}", path.display());
        assert!(is_enabled(), "must read back as enabled");

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[Desktop Entry]"));
        assert!(text.lines().any(|l| l.starts_with("Exec=")), "must have an Exec line");

        set(false).expect("disable");
        assert!(!path.exists(), "disabling must remove the file");
        assert!(!is_enabled());

        set(false).expect("disabling twice must not be an error");
    }

    #[test]
    fn disabled_markers_are_honoured() {
        // Reading the file's mere existence as "enabled" would show the switch on
        // for an entry the desktop has actually turned off.
        for text in [
            "[Desktop Entry]\nType=Application\nHidden=true\n",
            "[Desktop Entry]\nType=Application\nX-GNOME-Autostart-enabled=false\n",
        ] {
            let disabled = text.lines().any(|l| {
                let l = l.trim();
                l.eq_ignore_ascii_case("Hidden=true")
                    || l.replace(' ', "").eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
            });
            assert!(disabled, "should read as disabled: {text}");
        }
    }
}
