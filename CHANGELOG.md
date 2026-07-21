# Changelog

All notable changes to Gapless. Newest first.

The project is pre-1.0; entries are grouped by release and carry the commit
that made them.

## v0.1.2 — 2026-07-20

- **Self-contained AppImage release** (`Gapless-x86_64.AppImage`): bundles the
  whole GTK4/libadwaita and GStreamer stacks (all plugins + gst-plugin-scanner),
  so it runs on any distro — including KDE boxes that ship neither libadwaita
  nor a full GStreamer plugin set. Built by `scripts/build-appimage.sh`.
  The AppRun defaults `$APPDIR` before the GStreamer hook runs, so the tree
  also works extracted (how Linux App Manager installs it) — without that,
  playback would find no decoders.

## v0.1.1 — 2026-07-13

### Fixed

- **A resumed track now hands off to the next one.** After resuming a saved
  session, the track played to its end and then stopped dead — no advance, no
  repeat, no shuffle, no crossfade. A branch's length was the length of the whole
  *song*, even when it had been told to start 227 s in and would only play the
  remainder, so the follow-on track was scheduled minutes after the audio
  actually ran out. `Branch` now carries its `skip` and exposes `span()` — what
  it really occupies on the mixer timeline — and the scheduler and the fade both
  use that. Guarded by `scripts/verify-resume.sh`.
- **The session is now saved on SIGTERM and SIGINT.** Logging out, a `kill`, or
  `systemctl --user stop` never closes the window, so the close handler did not
  run and your place in the track died with the process — only the 5-second
  periodic save stood between you and losing it. A player whose whole point is
  remembering where you were should not forget because the *session* ended rather
  than the window.
- **Shuffle and repeat changed over MPRIS are now saved and repaint the UI**
  (`ffdc8d6`, 2026-07-12). Toggling either mode from a lock-screen widget or
  `playerctl` set the flag on the player and nothing else: the in-app buttons
  kept showing the old state, and no save was scheduled, so the change was lost
  unless the window happened to be closed cleanly. Unlike the transport
  controls, repeat and shuffle move no pipeline, so no bus message existed to
  drive the UI off. They now emit a `PlayerEvent::ModesChanged`, and one handler
  repaints the buttons, republishes to MPRIS and schedules the save — the button
  handlers take that same path instead of duplicating it.

### Added

- `scripts/verify-mpris-modes.sh` — proves an MPRIS-only mode change survives a
  `SIGKILL`, on a private D-Bus session and config dir so a running desktop copy
  of Gapless is neither disturbed nor accidentally driven.
- `scripts/verify-resume.sh` — resuming part-way into a track must still hand off
  to the next one, with the crossfade landing where the audio actually ends.
- `docs/SESSION-2026-07-12.md` — session report, including how the resume bug
  managed to imitate a no-op and fool the first round of measurements.

## v0.1.0 — 2026-07-11

- **Start at login** (`590fcfe`) — a switch in the settings popover, rather than
  editing `~/.config/autostart` by hand.
- **Session resume** (`187e2ee`) — remembers the source, track, position, volume,
  shuffle and repeat. The track is stored as a path, not an index, so a rescan or
  an edited playlist cannot resume into the wrong song.
- **Seek, slider-click freeze, and the missing app icon** (`cde881f`) — the seek
  bug was found by rendering a frequency sweep, where pitch encodes position.
- **Initial release** (`6ea2e7f`) — mixer-timeline gapless engine, silence
  trimming, interior-silence cap, equal-power crossfade, MPRIS2, playlists,
  ReplayGain. Gapless playback verified sample-exact rather than asserted; see
  `docs/VERIFICATION.md`.
