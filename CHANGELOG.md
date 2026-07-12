# Changelog

All notable changes to Gapless. Newest first.

The project is pre-1.0 and unreleased; entries are grouped by date and carry the
commit that made them.

## Unreleased

### Fixed

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
- `docs/SESSION-2026-07-12.md` — session report, including the open crossfade bug
  and the experiments that came back negative.

### Known issues

- **Crossfade does not audibly apply in the running app.** The value is saved
  correctly and the engine honours it when rendered through `examples/capture`
  (with trimming on or off), so the fault is app-side — most likely the
  startup/resume path. See `docs/SESSION-2026-07-12.md` §7.

## 2026-07-11

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
