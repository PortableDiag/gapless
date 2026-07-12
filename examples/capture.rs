//! Renders the real playback engine to a WAV file instead of the speakers.
//!
//! This is not a reimplementation of the player for testing purposes — it is
//! the actual `Player`, with the actual `about-to-finish` gapless handoff, with
//! only the audio sink replaced. Whatever lands in the WAV is exactly what
//! would have hit your ears.
//!
//!   cargo run --release --example capture -- out.wav track1.mp3 track2.mp3
//!
//! Then analyse it: scripts/verify-gapless.py out.wav

use anyhow::{anyhow, Result};
use gapless::player::{Player, PlayerEvent, QueuedTrack};
use gst::prelude::*;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let out = args.next().ok_or_else(|| anyhow!("usage: capture OUT.wav TRACK..."))?;
    let tracks: Vec<PathBuf> = args.map(PathBuf::from).collect();
    if tracks.is_empty() {
        return Err(anyhow!("no input tracks"));
    }

    gst::init()?;

    // GAPLESS_REALTIME=1 makes the sink honour the clock, so the render takes as
    // long as the music does. Required for any test that reacts mid-stream — with
    // sync=false the whole track is rendered before a callback can even fire.
    let realtime = std::env::var("GAPLESS_REALTIME").is_ok();
    let sync = if realtime { "true" } else { "false" };
    if realtime {
        eprintln!("  (real-time render)");
    }

    // Pin the format so the two decoded streams can't renegotiate caps midway,
    // which would make wavenc emit a second header and corrupt the analysis.
    let sink = gst::parse::bin_from_description(
        &format!(
            "audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate=44100,channels=2,layout=interleaved \
             ! wavenc ! filesink name=out sync={sync}"
        ),
        true,
    )?;
    sink.by_name("out")
        .ok_or_else(|| anyhow!("no filesink"))?
        .set_property("location", &out);

    let player = Player::with_sink(Some(sink.upcast()))?;
    player.set_tracks(
        tracks
            .iter()
            .map(|p| QueuedTrack { path: p.clone(), duration_nanos: 0 })
            .collect(),
    );
    player.set_trim_silence(std::env::var("GAPLESS_TRIM").is_ok());
    // Fill the trim cache before we start. Real playback analyses lazily and has
    // minutes of music to do it in; this harness renders faster than real time,
    // so the next branch must already be schedulable the instant we hit play.
    player.preanalyze_blocking();
    if let Some(secs) = std::env::var("GAPLESS_INNER").ok().and_then(|v| v.parse::<f64>().ok()) {
        player.set_inner_limit((secs * 1e9) as u64);
        eprintln!("  (inner silence capped at {secs:.2}s)");
    }
    if let Some(secs) = std::env::var("GAPLESS_CROSSFADE").ok().and_then(|v| v.parse::<f64>().ok()) {
        player.set_crossfade((secs * 1e9) as u64);
        eprintln!("  (crossfade {secs:.1}s)");
    }


    let main_loop = glib::MainLoop::new(None, false);
    let events = player.events.clone();

    glib::spawn_future_local({
        let main_loop = main_loop.clone();
        async move {
            while let Ok(event) = events.recv().await {
                match event {
                    PlayerEvent::TrackStarted(i) => {
                        eprintln!("  -> stream start: track {i}");
                    }
                    PlayerEvent::QueueFinished => {
                        eprintln!("  -> end of queue");
                        main_loop.quit();
                    }
                    PlayerEvent::Error(e) => {
                        eprintln!("  !! {e}");
                        main_loop.quit();
                    }
                    _ => {}
                }
            }
        }
    });

    player.play_index(0)?;
    main_loop.run();

    // Let the pipeline flush the WAV header's final size fields.
    player.stop()?;
    eprintln!("wrote {out}");
    Ok(())
}
