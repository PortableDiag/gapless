//! Sharing a track: the audio file itself, and the metadata that describes it.
//!
//! Two ways out, because "share" means different things depending on where it is
//! going:
//!
//!   * **The clipboard**, so the file can be pasted straight into a chat window,
//!     an email, or a file manager. See `clipboard_payloads` — the trick is that
//!     one clipboard carries several representations at once, and the receiving
//!     application picks the one it understands.
//!   * **A copy on disk**, next to a readable `.txt` of the metadata, for a USB
//!     stick or a shared folder. See `copy_to`.
//!
//! The logic lives here rather than in `main.rs` so it can be tested without a
//! display, and so the control API and the window run exactly the same code —
//! `/api/share` and the Share button are the same two functions.

use crate::library::Track;
use crate::ratings;
use std::path::{Path, PathBuf};

/// A human-readable description of a track. This is what lands in a chat window
/// when the file is pasted somewhere that takes text, and what is written beside
/// the audio file by `copy_to`.
///
/// Deliberately plain text with aligned labels rather than JSON: a person is
/// meant to read it. The machine-readable form is what `GET /api/queue` already
/// returns.
pub fn details(track: &Track) -> String {
    let mut out = String::new();
    let mut line = |label: &str, value: &str| {
        if !value.is_empty() {
            out.push_str(&format!("{label:<9}{value}\n"));
        }
    };

    line("Title", &track.title);
    line("Artist", &track.artist);
    line("Album", &track.album);

    if track.track_no > 0 {
        let n = if track.disc > 1 {
            format!("{} (disc {})", track.track_no, track.disc)
        } else {
            track.track_no.to_string()
        };
        line("Track", &n);
    }
    if let Some(year) = track.year {
        line("Year", &year.to_string());
    }
    if let Some(genre) = &track.genre {
        line("Genre", genre);
    }
    if track.duration_nanos > 0 {
        line("Length", &clock(track.duration_nanos));
    }
    line("Format", &track.format);
    if track.rating > 0 {
        line("Rating", &ratings::stars_text(track.rating));
    }
    line("File", &track.path.display().to_string());

    out
}

fn clock(nanos: u64) -> String {
    let secs = nanos / 1_000_000_000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The MIME types and payloads to put on the clipboard, in the order a receiving
/// application should prefer them.
///
/// **One clipboard, several representations.** Paste into Telegram, Discord or a
/// file manager and it takes `text/uri-list` and you get the actual audio file;
/// paste into a text field and it takes `text/plain` and you get the metadata.
/// Offering only one of those makes the button useless in half the places
/// somebody would press it.
///
/// `x-special/gnome-copied-files` is the third because GTK and Nautilus-derived
/// file managers look for it specifically, and its `copy\n` prefix is what
/// distinguishes a copy from a cut — without it a paste can *move* the user's
/// music out of their library.
pub fn clipboard_payloads(track: &Track) -> Vec<(&'static str, String)> {
    let uri = crate::player::uri_for(&track.path);
    vec![
        // CRLF, and a trailing one: RFC 2483 says uri-list lines end that way,
        // and some receivers keep the stray character as part of the filename.
        ("text/uri-list", format!("{uri}\r\n")),
        ("x-special/gnome-copied-files", format!("copy\n{uri}")),
        ("text/plain;charset=utf-8", details(track)),
    ]
}

/// Copies the audio file into `dest_dir` and writes the metadata beside it.
/// Returns `(audio, metadata)` as written.
///
/// **Never overwrites.** A share that silently replaces a file already in the
/// destination is a share that eats somebody's work, so a clashing name gets
/// ` (2)`, ` (3)` and so on — and the audio and its metadata take the *same*
/// suffix, so the pair cannot be split up.
pub fn copy_to(track: &Track, dest_dir: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    if !dest_dir.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} is not a directory", dest_dir.display()),
        ));
    }

    let stem = track
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("track");
    let ext = track.path.extension().and_then(|s| s.to_str());

    let (audio, meta) = free_pair(dest_dir, stem, ext);
    std::fs::copy(&track.path, &audio)?;
    // Written after the audio: if the copy fails there is no orphan text file
    // describing a track that is not there.
    std::fs::write(&meta, details(track))?;
    Ok((audio, meta))
}

/// The first `(audio, metadata)` pair of names where **neither** exists yet.
fn free_pair(dir: &Path, stem: &str, ext: Option<&str>) -> (PathBuf, PathBuf) {
    let named = |suffix: &str| {
        let audio = match ext {
            Some(e) => dir.join(format!("{stem}{suffix}.{e}")),
            None => dir.join(format!("{stem}{suffix}")),
        };
        let meta = dir.join(format!("{stem}{suffix}.txt"));
        (audio, meta)
    };

    let (audio, meta) = named("");
    if !audio.exists() && !meta.exists() {
        return (audio, meta);
    }
    for n in 2..10_000 {
        let (audio, meta) = named(&format!(" ({n})"));
        if !audio.exists() && !meta.exists() {
            return (audio, meta);
        }
    }
    // Ten thousand copies of one song is not a case worth handling gracefully,
    // but it must not loop for ever either.
    named(" (overflow)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Track {
        Track {
            path: PathBuf::from("/music/03 Get to the Choppa.mp3"),
            title: "Get to the Choppa".into(),
            artist: "Austrian Death Machine".into(),
            album: "Total Brutal".into(),
            year: Some(2008),
            genre: Some("Thrash Metal".into()),
            disc: 1,
            track_no: 3,
            duration_nanos: 143_000_000_000,
            format: "MP3 · 44.1 kHz · 320 kbps · Stereo".into(),
            rating: 4,
        }
    }

    #[test]
    fn details_reads_as_something_a_person_would_send() {
        let d = details(&track());
        assert!(d.contains("Title    Get to the Choppa"));
        assert!(d.contains("Artist   Austrian Death Machine"));
        assert!(d.contains("Length   2:23"));
        assert!(d.contains("Rating   ★★★★☆"));
        assert!(d.contains("/music/03 Get to the Choppa.mp3"));
    }

    /// Empty fields are omitted, not printed as blank labels — a share of an
    /// untagged file should not be a column of empty headings.
    #[test]
    fn unknown_fields_are_left_out_entirely() {
        let mut t = track();
        t.year = None;
        t.genre = None;
        t.rating = 0;
        t.track_no = 0;
        t.duration_nanos = 0;
        let d = details(&t);
        for absent in ["Year", "Genre", "Rating", "Track", "Length"] {
            assert!(!d.contains(absent), "{absent} should not appear");
        }
        assert!(d.contains("Title"));
    }

    #[test]
    fn a_disc_number_only_shows_when_there_is_more_than_one() {
        let mut t = track();
        assert!(details(&t).contains("Track    3\n"));
        t.disc = 2;
        assert!(details(&t).contains("Track    3 (disc 2)"));
    }

    /// The clipboard has to carry the file *and* the text, or the button only
    /// works in half the places somebody would press it.
    #[test]
    fn the_clipboard_offers_the_file_and_the_text() {
        let p = clipboard_payloads(&track());
        let types: Vec<&str> = p.iter().map(|(t, _)| *t).collect();
        assert_eq!(
            types,
            vec![
                "text/uri-list",
                "x-special/gnome-copied-files",
                "text/plain;charset=utf-8"
            ]
        );

        let uri_list = &p[0].1;
        assert!(uri_list.starts_with("file:///music/03%20Get"), "{uri_list}");
        assert!(uri_list.ends_with("\r\n"), "uri-list lines end CRLF");

        // Without the `copy\n` prefix a paste can MOVE the user's music.
        assert!(p[1].1.starts_with("copy\nfile:///"), "{}", p[1].1);

        assert!(p[2].1.contains("Austrian Death Machine"));
    }

    #[test]
    fn copy_to_writes_the_audio_and_the_metadata_together() {
        let src_dir = std::env::temp_dir().join(format!("gapless-share-src-{}", std::process::id()));
        let dst_dir = std::env::temp_dir().join(format!("gapless-share-dst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();

        let src = src_dir.join("03 Get to the Choppa.mp3");
        std::fs::write(&src, b"not really an mp3").unwrap();
        let mut t = track();
        t.path = src.clone();

        let (audio, meta) = copy_to(&t, &dst_dir).unwrap();
        assert_eq!(std::fs::read(&audio).unwrap(), b"not really an mp3");
        assert!(std::fs::read_to_string(&meta).unwrap().contains("Get to the Choppa"));
        assert_eq!(audio.file_name().unwrap(), "03 Get to the Choppa.mp3");
        assert_eq!(meta.file_name().unwrap(), "03 Get to the Choppa.txt");

        // Sharing the same track twice must not overwrite the first copy, and the
        // pair must keep the same suffix so they stay together.
        let (audio2, meta2) = copy_to(&t, &dst_dir).unwrap();
        assert_eq!(audio2.file_name().unwrap(), "03 Get to the Choppa (2).mp3");
        assert_eq!(meta2.file_name().unwrap(), "03 Get to the Choppa (2).txt");
        assert!(audio.exists(), "the first copy must still be there");

        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);
    }

    /// A destination that is not a directory is an error, not a panic and not a
    /// file written somewhere surprising.
    #[test]
    fn a_bad_destination_is_refused() {
        let t = track();
        assert!(copy_to(&t, Path::new("/definitely/not/here")).is_err());
    }
}
