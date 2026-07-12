//! MPRIS2 D-Bus integration: media keys, the GNOME/KDE lock-screen widget, and
//! anything else that speaks org.mpris.MediaPlayer2.
//!
//! mpris-server's `Player` is built on `Rc<LocalServer>` and takes plain `Fn`
//! callbacks, so it lives on the GTK main thread alongside everything else. No
//! cross-thread command channel needed.

use crate::library::Track;
use crate::player::{Player, Repeat};
use mpris_server::{LoopStatus, Metadata, PlaybackStatus, Time, TrackId};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

pub type Mpris = Rc<mpris_server::Player>;

pub async fn start(player: Arc<Player>) -> Option<Mpris> {
    let mpris = mpris_server::Player::builder("Gapless")
        .identity("Gapless")
        .desktop_entry("gapless")
        .can_play(true)
        .can_pause(true)
        .can_go_next(true)
        .can_go_previous(true)
        .can_seek(true)
        .can_control(true)
        .can_quit(false)
        .can_raise(false)
        .supported_uri_schemes(["file"])
        .supported_mime_types(["audio/mpeg", "audio/flac", "audio/ogg", "audio/mp4"])
        .build()
        .await
        .map_err(|e| eprintln!("MPRIS unavailable (media keys won't work): {e}"))
        .ok()?;

    let mpris = Rc::new(mpris);

    // The UI does not need updating from any of these: every one of them moves
    // the pipeline, and the pipeline's bus messages drive the UI already. A
    // lock-screen pause and a click on the pause button take the same path.
    mpris.connect_play({
        let player = player.clone();
        move |_| {
            let _ = start_or_resume(&player);
        }
    });
    mpris.connect_pause({
        let player = player.clone();
        move |_| {
            let _ = player.set_playing(false);
        }
    });
    mpris.connect_play_pause({
        let player = player.clone();
        move |_| {
            if player.is_loaded() {
                let _ = player.toggle_pause();
            } else {
                let _ = start_or_resume(&player);
            }
        }
    });
    mpris.connect_stop({
        let player = player.clone();
        move |_| {
            let _ = player.stop();
        }
    });
    mpris.connect_next({
        let player = player.clone();
        move |_| {
            let _ = player.next();
        }
    });
    mpris.connect_previous({
        let player = player.clone();
        move |_| {
            let _ = player.previous();
        }
    });

    // MPRIS Seek is a *relative* offset and may be negative.
    mpris.connect_seek({
        let player = player.clone();
        move |_, offset| {
            let now = player.position() as i128;
            let target = (now + offset.as_nanos()).max(0) as u64;
            player.seek(target);
        }
    });

    // SetPosition is absolute.
    mpris.connect_set_position({
        let player = player.clone();
        move |_, _track_id, position| {
            player.seek(position.as_nanos().max(0) as u64);
        }
    });

    // Repeat and shuffle move no pipeline, so unlike the transport controls above
    // they have nothing to drive the UI off. `Player::set_repeat` / `set_shuffle`
    // emit ModesChanged for exactly that reason; main.rs repaints and saves.
    mpris.connect_set_loop_status({
        let player = player.clone();
        move |_, status| {
            player.set_repeat(match status {
                LoopStatus::None => Repeat::Off,
                LoopStatus::Playlist => Repeat::All,
                LoopStatus::Track => Repeat::One,
            });
        }
    });

    mpris.connect_set_shuffle({
        let player = player.clone();
        move |_, shuffle| player.set_shuffle(shuffle)
    });

    mpris.connect_set_volume({
        let player = player.clone();
        move |_, volume| player.set_volume(volume)
    });

    // Serves the D-Bus connection for as long as the app lives.
    glib::spawn_future_local({
        let mpris = mpris.clone();
        async move { mpris.run().await }
    });

    Some(mpris)
}

/// A media key pressed on a freshly launched player must start the queue, not
/// silently resume a pipeline that has nothing in it.
fn start_or_resume(player: &Arc<Player>) -> anyhow::Result<()> {
    if player.is_loaded() {
        player.set_playing(true)
    } else {
        player.play_index(0)
    }
}

pub fn publish_track(mpris: &Mpris, index: usize, track: &Track, art: Option<&PathBuf>) {
    let mut builder = Metadata::builder()
        .title(&track.title)
        .artist([&track.artist])
        .album(&track.album)
        .length(Time::from_nanos(track.duration_nanos as i64));

    if let Ok(id) = TrackId::try_from(format!("/com/procomputation/Gapless/Track/{index}").as_str())
    {
        builder = builder.trackid(id);
    }
    if track.track_no > 0 {
        builder = builder.track_number(track.track_no as i32);
    }
    if let Some(art) = art {
        builder = builder.art_url(crate::player::uri_for(art));
    }

    let metadata = builder.build();
    let mpris = mpris.clone();
    glib::spawn_future_local(async move {
        let _ = mpris.set_metadata(metadata).await;
    });
}

/// Without this the lock-screen widget shows a track with no progress at all —
/// MPRIS serves `Position` from what we last told it, and we were telling it
/// nothing.
pub fn publish_position(mpris: &Mpris, nanos: u64) {
    mpris.set_position(Time::from_nanos(nanos as i64));
}

/// Tells clients the position jumped, rather than letting them extrapolate from
/// a stale one.
pub fn publish_seeked(mpris: &Mpris, nanos: u64) {
    let mpris = mpris.clone();
    glib::spawn_future_local(async move {
        let _ = mpris.seeked(Time::from_nanos(nanos as i64)).await;
    });
}

pub fn publish_status(mpris: &Mpris, playing: bool) {
    let status = if playing {
        PlaybackStatus::Playing
    } else {
        PlaybackStatus::Paused
    };
    let mpris = mpris.clone();
    glib::spawn_future_local(async move {
        let _ = mpris.set_playback_status(status).await;
    });
}

pub fn publish_modes(mpris: &Mpris, repeat: Repeat, shuffle: bool) {
    let status = match repeat {
        Repeat::Off => LoopStatus::None,
        Repeat::All => LoopStatus::Playlist,
        Repeat::One => LoopStatus::Track,
    };
    let mpris = mpris.clone();
    glib::spawn_future_local(async move {
        let _ = mpris.set_loop_status(status).await;
        let _ = mpris.set_shuffle(shuffle).await;
    });
}
