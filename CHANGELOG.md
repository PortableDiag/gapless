# Changelog

All notable changes to Gapless. Newest first.

The project is pre-1.0; entries are grouped by release and carry the commit
that made them.

## v0.3.2 — 2026-09-13

### Fixed

- **The album-art cache grew without bound, for ever.** Every track change wrote
  `~/.cache/gapless/art-{n}` for MPRIS clients to point at, and **nothing ever
  deleted one**. Worse, the sequence restarts at zero on every launch, so each
  run overwrote `art-1`, `art-2`… and permanently orphaned everything above its
  own high-water mark.

  Measured on a real install before the fix: **283 orphaned cover files**, at
  ~96 KB each on this library — tens of megabytes of covers nothing would ever
  read again, growing for as long as the player runs. The newest four are now kept —
  more than one because an MPRIS client fetches art asynchronously and may still
  be reading the previous track's file — and a launch sweeps whatever earlier
  runs left behind. Pruned by the sequence number parsed from the name, not
  lexically: `art-9` sorts after `art-10` as a string, which would delete the
  newest cover and keep nine stale ones.

- **`GET /api/status` reported `position_secs: 0.0` for a cued track.** A track
  restored from the last session has a position — it is where the resume will
  start — but nothing was loaded yet, so the pipeline answered zero. A caller was
  told the track was at the beginning when it was two minutes in, and the number
  changed under them the instant they pressed play.

### Verification

- **`scripts/verify-input.sh`** — the two rating paths nothing else could reach:
  the number-key accelerators and the right-click menu on a list row. 9 checks
  against the real window with real XTEST input.

  It exists because those were the last part of the feature covered only by
  inspection, and there are two reasons that was hard. **GTK4 ignores synthetic
  key events** — `xdotool key --window` uses XSendEvent and does nothing at all,
  silently — so the window has to genuinely hold focus. And **a window manager
  can refuse focus** to a window that just appeared; KDE did, until the window
  was mapped and raised first. The script maps, raises, activates, focuses, then
  *verifies* with `getactivewindow` before sending anything, and **fails rather
  than skipping** if focus is refused: a check that quietly does nothing is worse
  than no check.

  It also proves the menu rates **the row under the pointer** and not the track
  the star strip is pointing at, which is the whole reason the menu exists.

`cargo test` 33/33 · `verify.sh` 6/6 · `verify-resume.sh` 4/4 ·
`verify-mpris-modes.sh` 4/4 · `verify-api.sh` 44/44 · `verify-input.sh` 9/9

### Correction to this entry

The first published version of these notes said the leak measured **682 MB
across 285 files**. The file count was right; the size was not. `~/.cache/gapless`
also holds the AppImage build scratch (`appimage-build`, `appimage-tools` —
about 500 MB), which this project's own release script had put there an hour
earlier, and `du -sh` on the directory counted it as leaked album art.

The leak is real and the fix is unchanged — nothing ever deleted a cover file,
and the sequence restarting at zero each launch orphans everything above the new
run's high-water mark. But it was **tens of megabytes, not hundreds**, and the
number was published before it was checked against what those files actually
were.

## v0.3.1 — 2026-09-13

Four defects, **all four found by operating the player rather than by any test
that existed**. Three of them were in code that shipped hours earlier with a
green suite; the fourth had been there since v0.1.0.

### Fixed

- **A media key, or a lock-screen Play, ignored the resume point.** The play
  *button* resumed where the last session stopped. MPRIS did not — it called
  `play_index(0)`, started the queue from the top and threw the resume point
  away. Anyone restarting the app and pressing Play on their keyboard rather
  than in the window got the wrong track.

  The cause was the resume point living in the GTK front-end's `Ui`, which
  `mpris.rs` cannot see, so `start_or_resume` had no way to reach it and did the
  only thing it could. It now lives on the `Player` as a **cue**, and `play()` /
  `play_pause()` are the whole of "press play" for every caller — the button, a
  media key, MPRIS and the control API all call the same two methods. Anything
  that reimplements that logic is how the resume point gets lost by one route
  and not another.

- **`POST /api/play` answered `"track": null`** about a track it had just
  started, and **`POST /api/rating {"stars":N}` answered 409 "nothing is
  playing"** while it was playing.

  Both read `ui.focus`, which is set by the `TrackStarted` event — and that
  arrives on the channel *after* the call that started playback has returned. A
  caller therefore had to sleep and re-poll to find out what it had just done.
  Both now go through one `focused_track` helper, which leads with
  `player.current()` (set synchronously) and falls back to the cued track.

- **`is_playing()` was false for a moment after a successful play.** A GStreamer
  state change is asynchronous, so the pipeline is still PAUSED with PLAYING
  pending; reading only the current state made `POST /api/play` report
  `"playing": false` about a call that had just succeeded, leaving a caller no
  way to tell "starting" from "refused" except by polling. It now counts the
  **pending** state, which is exactly the missing information.

- **`Player::play()` deadlocked the whole application** — caught by the harness
  before release, and worth recording because it is invisible on inspection.
  Written as `if let Some(x) = self.cued.lock().unwrap().take() { … }`, the
  temporary `MutexGuard` lives to the end of the `if let` **body** in edition
  2021, and the body calls `start_at`, which locks the same mutex to clear the
  cue. The app stayed alive and MPRIS kept answering while every call timed out
  with the main loop wedged. Take the value into a local first. The three other
  `if let … lock()` sites in `player.rs` were checked and are safe: their bodies
  are a single assignment, so the guard drops before any further call.

### Verification

- `scripts/verify-mpris-modes.sh` gains a fourth case: a resume point is seeded,
  the real app is launched, **Play is sent over MPRIS**, and the track that comes
  back must be the cued one and not the top of the queue. It waits for the bus
  name rather than sleeping at it — a fixed sleep raced the MPRIS server coming
  up, and the check then reported an empty title as a failure of the thing it was
  testing.
- `scripts/verify-api.sh` gains three cases, one per API defect above. They exist
  because the existing 41 walked straight past all three: every scripted check
  rated by explicit index and read status after a sleep, so nothing ever asked
  the API a question a person would ask it.
- The cue's own tests build a **real `Player`** with a `fakesink`. They are
  deliberately one test function: `Player` installs a bus watch on the glib main
  context, a main context belongs to the first thread that acquires it, and
  `cargo test` gives every test its own thread — so a second test building a
  `Player` fails intermittently, depending on scheduling.

`cargo test` 30/30 · `verify.sh` 6/6 · `verify-resume.sh` 4/4 ·
`verify-mpris-modes.sh` 4/4 · `verify-api.sh` 44/44

## v0.3.0 — 2026-09-13

### Added

- **A local control API.** HTTP and JSON on `127.0.0.1`, so an agent or another
  program can do everything the window does: load a folder or a playlist, play,
  pause, seek, skip, change every mode, rate a track, change the three playback
  settings, and quit. **Off by default**, switched on under the gear button →
  *Remote control API*, with the port and the key beside the switch.

  **[docs/API.md](docs/API.md) is the reference**, and the running build serves
  its own copy at **`GET /api/docs`** (`?section=` narrows it) — compiled in with
  `include_str!`, so the document can never describe a different version than the
  one answering you. `GET /api` is a one-line index of every route. Pointing
  something at the port and the key is enough to get it started.

  Why not MPRIS, which already exists: it is the right interface for media keys
  and a lock screen and the wrong one for scripting. A caller needs a D-Bus
  connection and bindings; the vocabulary is fixed by the spec, so silence
  trimming, crossfade length, the interior-silence cap, ratings and favorites
  shuffle have nowhere to live in it; and its `Shuffle` is a bool where this
  player has three states. MPRIS is unchanged.

  **Security.** Loopback only, never a wildcard address. Every route needs the
  key — there is no unauthenticated endpoint, not even a health check, because
  an unauthenticated endpoint is a way to discover the player is there and there
  is nothing useful it could tell a caller who has no key. The key is 32 bytes
  from `/dev/urandom`, hex, in `~/.config/gapless/api-key` at mode 0600, and is
  compared in **constant time** — a plain `==` returns at the first differing
  byte, which over enough requests hands the key over one byte at a time.
  `gapless --api-key` prints it without opening a window, because asking the API
  for its own key is not a plan that goes anywhere.

  **One player, not two copies of the state.** Requests are parsed on the
  listener's threads and executed on the GTK main thread, in the same place a
  button click runs — so commands cannot interleave halfway through a queue
  change, and an API call moves the same widgets a click does. Setting the
  volume moves the slider; the slider's own handler is what tells the engine,
  repaints the icon and schedules the save.

  `POST /api/quit` saves the session before exiting, and replies before it goes.

- **`scripts/verify-api.sh`** — 41 checks over a real socket against the real
  application, on a private bus, a private config directory and a high port, with
  the volume set to zero first so the run is silent. It covers what is most
  likely to be quietly wrong: that an absent key and a wrong key are both
  refused, that an **unauthorised write changes nothing**, that a typo in a mode
  is rejected rather than obeyed, that a rating reaches the sidecar file, that
  modes reach `state.json` after a **SIGKILL**, and that nothing answers on the
  port when the switch is off.

### Fixed

- **Loading a folder could hang the whole application** — introduced with the
  rating context menu in v0.2.0 and found by the new API harness rather than by
  using the app.

  `load_source` cleared the list with `while let Some(row) = list.first_child()`.
  A `GtkListBox`'s children are not all rows: the rating popover is parented to
  it, so once the rows were gone `first_child()` returned the popover for ever,
  `remove` refused it as a non-child, and the loop span. The window froze and
  GTK emitted **5.8 million** `Tried to remove non-child` warnings in a few
  seconds. It now uses `row_at_index(0)`, which only ever returns real rows.

  Worth recording because of how it surfaced: through the UI the symptom needs a
  second Open Folder after a right-click, which is why it survived a manual pass.
  The API harness hit it on its first `POST /api/open`.

## v0.2.0 — 2026-09-13

### Added

- **Star ratings, 1–5.** Set them from the strip under the track title in the
  now-playing panel, from the number keys (`1`–`5` rate, `0` clears), or from a
  right-click on any row in the list — the last of which matters because
  clicking a row in this player *starts* it, so without a context menu you could
  only rate a track by playing it first.

  Stored in **`~/.config/gapless/ratings.json`**, keyed by absolute path.
  Deliberately **not** written into the audio files: rating a song would
  otherwise mean rewriting it, and `POPM` has no agreed 1–5-to-0–255 mapping, so
  a value read back only means something if you already know which player wrote
  it. Deliberately not in `state.json` either — that file is rewritten every few
  seconds while playing and again from the SIGTERM handler, and ratings are the
  one thing on disk the user typed in by hand. Written through a temp file and
  renamed, and written on the click rather than debounced.

  The list shows ratings as a plain label, not five buttons per row: the list is
  not virtualised, so anything per-row is paid for once per track in the library.

- **Favorites shuffle** — a third shuffle state, cycled with the same button:
  off → shuffle → favorites.

  It is an **ordering, not a filter**: every track still plays exactly once per
  pass, but the order is drawn with higher-rated tracks weighted towards the
  front. Weights double per star (1★ = 1 … 5★ = 16), with unrated at 2 — level
  with 2★, because "I have not judged this" is not "I do not like this". The
  draw is Efraimidis–Spirakis weighted sampling without replacement, computed in
  log space so the keys stay distinguishable over a large queue.

  Measured rather than asserted. Over 4,000 passes of a 12-track queue, where a
  uniform shuffle puts every track at a mean slot of 5.50:

  ```
  5-star  1.51      unrated  5.96      1-star  7.65
  ```

  `cargo test -- --nocapture` prints that line on every run, so a weighting that
  points the right way but is too weak to hear does not pass quietly.

  **One bug worth recording.** The first implementation sorted the keys
  ascending instead of descending. Nothing looked wrong — the queue shuffled,
  every track appeared once, no warning anywhere — but the player preferred the
  tracks you rated *worst*. It was caught only because the test measures the
  direction rather than checking that the result is a permutation. The two tests
  that measure direction and strength are there for that reason and should not
  be reduced to a permutation check.

- The shuffle button now has three looks, since it has three states: quiet when
  off, the standard accent for plain shuffle, and the theme's warning colour for
  favorites. It does **not** swap in a star icon — the rating strip a few inches
  to its left is made of stars, and a star on the transport row reads as
  "favorite this track", not "shuffle by favorites".

### Changed

- `state.json` gains **`shuffle_mode`** (`"off"` | `"on"` | `"favorites"`). The
  old `shuffle` bool is still written on every save, so an older build and
  `verify-mpris-modes.sh` both still read it, and still read when `shuffle_mode`
  is absent, so a config written before this release migrates rather than
  silently turning shuffle off.

- **MPRIS cannot silently downgrade favorites shuffle.** Its `Shuffle` property
  is a bool, and both on-states publish as `true`; a client that echoes the
  property back — lock-screen widgets do — would otherwise turn the user's
  favorites shuffle into a plain one with no visible symptom beyond the music
  quietly no longer preferring their favorites. `true` now keeps whatever
  on-mode is already set and only means plain shuffle from a standing start.
  `false` still turns shuffle off.

- `scripts/verify-mpris-modes.sh` now runs **three** cases rather than one: the
  original crash-persistence check (still seeded in the old config format, so it
  also exercises the migration), plus the two directions of the MPRIS bool
  above. The third case exists so the second cannot pass by ignoring the
  property altogether.

### Documentation

- **Two of the three verification scripts need a display, and nothing said so.**
  `verify-resume.sh` and `verify-mpris-modes.sh` launch the real application, so
  `DISPLAY` has to be set; from a desktop terminal it always is, which is why
  this went unrecorded through five releases. Run over ssh or from anything that
  doesn't inherit the session environment, they fail without mentioning a
  display — and `verify-mpris-modes.sh` pipes its own output, so a GTK startup
  failure ends up stuck in a buffered pipe with nothing on screen at all.

  README and `docs/DEVELOPING.md` now say to prefix them with `DISPLAY=:0`, note
  that `verify.sh` alone is genuinely headless, and record the two things that
  look like failures and aren't: the `fusermount3` / xdg-desktop-portal warning
  wall that `dbus-run-session` emits, and the buffered-pipe trap.

  (This entry was written against v0.1.5 as a documentation-only change with no
  version bump; it ships here instead.)

## v0.1.5 — 2026-08-03

### Added

- **The project is now licensed: MIT.** It had no `LICENSE` file and no `license`
  field, which by default means *all rights reserved* — an odd footing for a
  public repository that publishes AppImages, and the reason v0.1.4's About
  dialog stayed silent on the subject rather than inventing an answer.

  Stated in three places, which must agree: `LICENSE` (MIT, © 2026 PortableDiag),
  the `license = "MIT"` field in `Cargo.toml`, and the About dialog's **Legal**
  page, via `gtk::License::MitX11` — GTK's name for the same MIT/X11 licence.
  A copyright line was added alongside it.

## v0.1.4 — 2026-08-03

### Added

- **The app can now tell you what version it is.** It could not before, in any
  way: nothing in `src/` referenced `CARGO_PKG_VERSION`, so `strings` on a build
  found no version anywhere, and there was no way to ask a running or installed
  copy what it was. Version tracking lived entirely in metadata outside the
  binary — `Cargo.toml`, the git tag, and whatever an installer recorded.

  - **`gapless --version`** (also `-V`) prints `gapless <version>`. Answered
    before GTK or the audio engine start, so it works headless, over ssh, and
    while another copy is already running — none of which can open a dialog.
    Handled directly rather than through `GApplication`, which would need a
    running instance to reply.
  - **An About dialog**, from the bottom of the settings popover. States what
    the program is and which version this is, with links to the repository and
    the issue tracker.

  Both take the number from `env!("CARGO_PKG_VERSION")`, so it can only be wrong
  by being wrong in `Cargo.toml`.

  No license is declared in the dialog: the repo carries no LICENSE file and
  `Cargo.toml` has no `license` field, and stating one would invent a legal fact.

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
