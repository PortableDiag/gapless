# Changelog

All notable changes to Gapless. Newest first.

The project is pre-1.0; entries are grouped by release and carry the commit
that made them.

## v0.1.3 — 2026-08-03

**No application code changed in this release.** Everything here is the
verification harness and the documentation, so the v0.1.2 AppImage is still
current and there is no binary to update. The theme is that several things the
project *claimed* to measure were not being measured, and one of them could not
be measured by anyone but the author.

### Added

- **`verify.sh` now measures the three features too** — silence trim, interior
  silence cap and crossfade — via a new `scripts/verify-features.py`. Together
  with the negative controls below, every line the README prints as a result is
  now produced by the run rather than quoted from a measurement taken once.

  Each feature is rendered **twice**, off and on, and the check fails if the
  *off* render does not show the defect. A trim that passes because the fixture
  had no silence in it proves nothing.

  The crossfade check measures the **shape** of the fade rather than its length,
  because duration cannot tell equal-power from linear — both turn a 20 s render
  into exactly 17 s. The fixtures are an octave apart, so projecting the render
  onto each tone recovers the gain applied to each branch, and equal power means
  `a² + b²` holds at 1 across the overlap. Measured 0.13%; a synthetic linear
  fade of identical duration was put through the same check and sagged 49.99%, so
  the check is known to separate them.

- **`make-test-tones.sh` now builds every fixture the harness uses**, not just
  the tone pair: `xf-440`, `xf-880`, `sil-part1`, `hole` and `sweep`. `testdata/`
  is gitignored, so those five existed only on the machine they were first made
  on. This was already load-bearing and already broken — `verify-resume.sh`
  guarded itself with `[ -f testdata/xf-440.mp3 ] || ./scripts/make-test-tones.sh`,
  and the script it called did not build `xf-440.mp3`, so on a clean checkout the
  guard fired, achieved nothing, and the render failed on a missing file.
  `verify.sh` now also checks for a feature fixture, so a `testdata/` predating
  this change is completed rather than left half-built.

### Removed

- **The `real library (ADM)` line is gone from `docs/VERIFICATION.md`.** It was
  measured against a personal music library that is not in the repo and cannot be
  re-measured by anyone reading the document. A number nobody can reproduce is an
  assertion with a decimal point on it.

### Fixed

- **`verify.sh` now runs the negative controls it always claimed to.** Both the
  README and the script's own header said it ran deliberately-broken captures
  through the analyser — *"a test that cannot fail proves nothing"* — and it did
  not. It ran two checks. The seven broken splices were recorded in
  `docs/VERIFICATION.md` as measurements that had been taken, but no code in the
  repo could re-take them: `verify-gapless.py` accepted a single WAV path and had
  no negative mode. For a project whose entire verification argument is that a
  check must be able to fail, the check that proves the analyser can fail was the
  one not wired up.

  `verify-gapless.py --negative CAPTURE.wav` now synthesises the seven breaks — a
  20 ms silence splice and sample drops of 25/50/75/100/150/200 — into the
  midpoint of a good render, and requires each to be caught by at least one of
  the three checks. It prints the full matrix of which check caught what, because
  the **dots** are the argument: a 25-sample drop is 0.57 ms and hides inside the
  length tolerance, so only the phase test sees it; drops of 100 and 200 remove a
  whole number of cycles (one period is 100.2 samples) and are phase-invisible by
  construction, so only the length test sees them. Delete either check and a real
  defect walks through.

- **README linked a session report that had been deleted.** `b345c7f` moved
  session reports out to an external log directory and removed
  `docs/SESSION-2026-07-12.md`, but the Documentation table kept pointing at it —
  a dead link on the front page, whose description also still advertised "the
  open crossfade bug" that v0.1.1 had closed. The row is gone; `CHANGELOG.md`
  carries that history and ships with the repo.

- **README no longer overstates what `verify.sh` measures.** Its result block
  listed six lines; the script produced two. The gap was closed from both ends —
  the script grew the negative controls and the three feature checks above, and
  the one line that could never be reproduced (`real library (ADM)`) was removed
  rather than left standing. Every line in the block is now output of the run.

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
