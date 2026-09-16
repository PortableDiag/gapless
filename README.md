# Gapless

A Linux music player that actually plays albums without holes in them.

Built because Elisa — and most other players — leave an audible gap between
tracks. It turns out that complaint has **two** causes, and fixing only the
famous one leaves you still hearing a gap.

![GTK4 / libadwaita](https://img.shields.io/badge/GTK4-libadwaita-blue)
![GStreamer](https://img.shields.io/badge/audio-GStreamer-green)
![Rust](https://img.shields.io/badge/rust-2021-orange)

---

## The two problems

**1. The pipeline.** Most players stop the audio pipeline at end-of-track, build a
new one for the next file, and let the sink drain in between. That is 100–300 ms
of silence, on every track, forever.

**2. The files.** Even with a *perfect* pipeline you can still hear a hole,
because a great many rips have silence **recorded into them**. Measured on a real
45-track library:

| | |
|---|---|
| Median trailing silence, per track | **1158 ms** |
| Worst case | **7.4 s** |
| Files carrying a LAME/Xing gapless header | **0 of 45** |

That silence is real audio data. No amount of cleverness in the pipeline removes
it — you have to know where it is and refuse to play it.

Nearly every "gapless" player solves (1) and quietly loses to (2). This one does
both, and can crossfade instead if you'd rather.

## Features

- **Gapless playback** — verified *sample-exact*, not asserted. Includes the
  repeat-all wrap and repeat-one, which is where most players still gap.
- **Skip silence between tracks** — the fix for non-gapless rips. On by default.
- **Cap silence inside a track** — for long pauses and hidden tracks buried in
  dead air. A cap, not a switch: a four-bar rest is music.
- **Crossfade, 0–10 s** — Winamp-style, equal-power.
- **Force tempo** — set a BPM and every track plays at it, **pitch-preserving**,
  so a 128 BPM track at 1.09x is still in the key it was recorded in. A tempo
  comes from the file's `TBPM` tag or is measured from the audio, and is
  remembered. A track with **no** steady tempo is found to have none and left
  alone; a track already fast enough is left alone too.
- **Star ratings, 1–5** — on the now-playing panel, on the number keys, or from
  a right-click on any row. Kept in a sidecar file, **not** written into your
  audio files.
- **Shuffle that prefers your favorites** — a third shuffle state that still
  plays every track once, but draws the order with the higher-rated tracks
  weighted towards the front. Measured, not asserted: over 4,000 passes of a
  12-track queue, 5-star tracks average slot 1.5 where a plain shuffle averages
  5.5.
- **Share a track** — the button under the song title, or a right-click on any
  row. Puts the **audio file itself** on the clipboard so it pastes into a chat
  window or a file manager, and the metadata as text at the same time, from the
  same copy — or saves a copy to a folder with a readable `.txt` of the details
  beside it. Also `POST /api/share`.
- **A local control API** — HTTP and JSON on 127.0.0.1, key-authenticated, off
  until you switch it on. It does everything the window does, so an agent or a
  script can drive the player. `GET /api/docs` serves the full reference from
  the running build. See **[docs/API.md](docs/API.md)**.
- Repeat off/all/one, shuffle, ReplayGain, MPRIS2 (media keys + lock screen) —
  and a mode toggled from the lock screen or `playerctl` repaints the buttons and
  is saved, exactly as a click on them would be.
- M3U/M3U8/PLS playlists, **in playlist order**.
- Album art, per-track detail (year, genre, codec, sample rate, bit depth).
- **Resumes your session** — folder or playlist, volume, shuffle, repeat, and the
  track and position you stopped at. Cued up, not auto-played.
- **Start at login** — a switch in the settings popover, no fiddling with
  `~/.config/autostart` by hand.
- **About dialog**, and a command line that answers without opening a window:
  `--version`, `--api-key`, and `--new-instance` / `--api-port` for running a
  second copy alongside the first.

## Install

Ubuntu 24.04 (or any Debian-ish distro with GStreamer 1.20+):

```sh
./scripts/setup-deps.sh     # needs sudo
cargo run --release
```

Then **Open Folder…** or **Open Playlist…**, and press play.

The playback settings live behind the **gear button** in the header bar.

### Force tempo

**Set a BPM and everything plays at it.** Switch it on under **Force tempo**
behind the gear button, pick a target from 60 to 200, and each track is sped up
as far as it needs to reach it.

It exists for one complaint: a workout playlist where **one slow track is a slow
patch in the workout**. Everything about the defaults follows from that.

| Control | Default | What it does |
|---|---|---|
| **Target tempo** | 140 BPM | The tempo to bring every track to. |
| **Most a track may be stretched** | 30% | The ceiling. Under about 30% a stretched track just sounds like a track at that tempo; well over it, it sounds stretched. A track that cannot reach the target inside the ceiling goes as far as the ceiling allows rather than further. |
| **Never slow a track down** | on | A track already at or above the target is left **completely alone** — verified bit-exact, not just the same length. Off, a fast track is slowed to meet the target too. |

The stretch is GStreamer's `pitch` element (SoundTouch), so **the pitch does not
move**: a 128 BPM track at 1.09x is still in the key it was recorded in. The
verification renders a 440 Hz fixture at 1.25x and requires it to come back at
440 Hz — a naive resample lands at 550 Hz and is caught.

**Where a tempo comes from.** The file's **`TBPM` tag** if it has a believable
one; otherwise Gapless **measures** it — half a minute decoded from 30 s in (the
opening of a track is the least representative part of it), reduced to an onset
envelope at 10 ms resolution and autocorrelated. Either way the answer is
**remembered per track** in `~/.config/gapless/bpm.json`, so it is worked out
once and never again, and the **next** track in the queue is analysed while the
current one plays — so the only track ever heard at the wrong speed is the one
that was already playing when you switched the feature on.

**A track with no tempo is never touched.** An audiobook, a drone or a field
recording is analysed, found to have no steady beat, and **written down as having
none** — it plays unchanged and is not analysed again. "Nothing known yet" and
"nothing there" are deliberately two different states, all the way out to the
API: a player that invented a tempo for a podcast and then played it 30% fast
would be indefensible.

**The measured number is editable**, because it is a measurement rather than a
preference and you can hear when it is wrong. The panel shows the playing track's
BPM in a field you can type over; a number you supply **wins and is never quietly
re-measured**, and clearing it throws the measurement away and has another go.

#### Octaves, which is where this gets interesting

A tempo is only defined up to a factor of two — a track counted at 70 BPM and a
track counted at 140 can be the same felt pace. That ambiguity is handled in
**two** places, and it has to be.

- **Choosing the speed.** 70 BPM in double time *is* 140, so such a track is left
  alone rather than played at 2.0x. But the fold is only accepted when the halved
  or doubled reading lands **near** the target (within 15%, measured in log space
  so it means the same thing in both directions), so a merely slow **95 BPM**
  track is not excused as a secretly-fast one on the grounds that 190 is "closer"
  to 140 in ratio — it gets stretched, which is the point.
- **Measuring it.** The estimator resolves its own octave with a preference
  weight around the perceived pulse, plus a tie-break that takes the **faster**
  reading when the audio supports both equally: a rhythm that repeats every beat
  also repeats every two, so the slower lag always correlates at least as well
  and picking the maximum would systematically halve every tempo. The converse
  does not hold — if the beat really were the slower one, the half-lag would be
  lining beats up with the gaps and correlating badly.

The lag search runs at **quarter-frame** resolution rather than whole frames, and
that is not a precision nicety — it is the difference between a right answer and
an octave. A 160 BPM beat lands every 37.5 envelope frames, so *neither* lag 37
nor lag 38 lines the rhythm up with itself, while lag 75 — two beats — lines it
up perfectly. A whole-frame search reports 80 BPM for that track, confidently.
(The envelope is also blurred before it is interpolated, without which the
quarter-frame grid is a fiction and the same octave error comes back by a
different route.)

**What it composes with.** Force Tempo is the first thing here that makes **media
time and wall-clock time different quantities**. The mixer timeline is measured
in seconds of listening; a track is measured in seconds of track. At 1.3x the
last six seconds of a track are 4.6 seconds of listening, so a crossfade timed
against the wrong one starts late and is cut off by the transition, and a track
resumed part-way in would place its follower wrong by the skip times the speed.
Every conversion goes through one pair of functions in `src/tempo.rs`, and both
compositions are checked by `verify-resume.sh`.

## Verify the claims yourself

Nothing here is taken on faith:

```sh
./scripts/verify.sh
```

This renders the **real engine** to a WAV file — the actual player, the actual
mixer timeline, with only the audio sink swapped out — and measures the result.
It also runs deliberately-broken captures through the same analyser, because a
test that cannot fail proves nothing.

```
gapless (crossfade 0)   882000 frames, +0.00 ms, 0 ms silence   PASS
FLAC control            882000 frames, +0.00 ms, 0 ms silence   PASS
negative controls       7/7 broken splices caught
silence trim            1000 ms gap  ->  10 ms
interior silence cap    3000 ms pause -> 1010 ms at a 1.0 s cap
crossfade 3 s           20.0 s -> 17.0 s, equal-power to 0.13%
force tempo 1.25x       20.000 s -> 16.000 s, pitch 440 Hz -> 440 Hz
force tempo, fast track 20.000 s -> 20.000 s, untouched
```

Every line there is produced by the run — nothing in that block is quoted from a
measurement someone took once.

Two of them are worth explaining. **The negative controls** deliberately break a
good capture seven ways and require each break to be caught: no single check
catches all seven, since a 25-sample drop is 0.57 ms and hides inside the length
tolerance while a 100-sample drop is phase-*invisible* by construction (one
period is 100.2 samples). The run prints which check caught what, so that stays
visible. **The crossfade line** measures the *shape* of the fade, not its
length — the render is projected onto each tone to recover the gain applied to
each track, and `a² + b²` must stay at 1. A linear fade produces an identical
17.0 s render and sags to 0.5.

Each feature is also rendered with itself switched **off**, and the check fails
if that render doesn't show the defect — a trim that passes because the fixture
had no silence in it would prove nothing.

Fixtures are generated by `scripts/make-test-tones.sh`, which `verify.sh` runs
for you if `testdata/` is missing or incomplete.

See **[docs/VERIFICATION.md](docs/VERIFICATION.md)** for how and why, including
why the obvious way to test this is wrong.

Four more scripts, each written after a real bug got past the ones above:

```sh
./scripts/verify-resume.sh                  # a track resumed part-way in must still hand off,
                                            # at 1.0x and stretched
DISPLAY=:0 ./scripts/verify-mpris-modes.sh  # a mode set over D-Bus must survive a SIGKILL, must
                                            # not downgrade favorites shuffle, and Play must resume
DISPLAY=:0 ./scripts/verify-api.sh          # 78 checks over a real socket against the real app
DISPLAY=:0 GAPLESS_INPUT_OK=1 \
  ./scripts/verify-input.sh      # the rating keys and the right-click menu, real input
```

The weighted shuffle is measured by the unit tests, which print the distribution
rather than only asserting its direction:

```sh
cargo test -- --nocapture
```

**These scripts share your desktop, so they behave themselves.** `verify-api.sh`
and `verify-mpris-modes.sh` open a window, so they wait for the machine to be
idle first (`GAPLESS_WINDOWS_OK=1` to go anyway). `verify-input.sh` drives the
real pointer and keyboard — GTK4 ignores synthetic key events, so there is no
other way to test them — and therefore **refuses to run at all** unless you set
`GAPLESS_INPUT_OK=1`; even then it waits for a quiet keyboard and abandons its
run the moment you touch a key. `verify.sh` and `verify-resume.sh` touch nothing
and can run whenever.

**Three of the five need a display**, because they launch the real application:
`verify-mpris-modes.sh`, `verify-api.sh` and `verify-input.sh`. Prefix those with
`DISPLAY=:0` over ssh or from anything that doesn't inherit a desktop session.

`verify.sh` **and `verify-resume.sh`** are genuinely headless — both render
through the `capture` example with the audio sink swapped out, and need nothing.
(`verify-resume.sh` was documented as needing a display for five releases. It
does not; it was measured with `DISPLAY` unset and passes 4/4.)

## Driving it from something else

```sh
KEY=$(gapless --api-key)
curl -s -H "Authorization: Bearer $KEY" http://127.0.0.1:8421/api/status
```

Switch it on under the gear button → **Remote control API**. It binds to loopback
only and every route needs the key. The whole reference is
**[docs/API.md](docs/API.md)**, and the running build serves its own copy at
`GET /api/docs` — so pointing an agent at the port is enough.

## Replacing it while it is playing

```sh
./scripts/handover.sh
```

Starts a second copy alongside, hands the track over mid-playback and retires the
old one — **34 ms** of overlap instead of a silent gap. Uses `--new-instance` and
`--api-port`; see [docs/API.md](docs/API.md).

## Documentation

| | |
|---|---|
| **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** | How the engine works, and the three approaches that failed first |
| **[docs/VERIFICATION.md](docs/VERIFICATION.md)** | How the gapless claim is proved rather than asserted |
| **[docs/API.md](docs/API.md)** | The control API — enabling it, the key, every endpoint |
| **[docs/DEVELOPING.md](docs/DEVELOPING.md)** | Layout, the tools in `examples/`, gotchas |
| **[CHANGELOG.md](CHANGELOG.md)** | What changed, when |

## Status

Playback is solid. Not yet done: no database (the library is rescanned on each
open, and ratings are a flat JSON sidecar rather than a table), no search, no
queue editing, no folder.jpg cover fallback, and no import of ratings already
sitting in your files' `POPM` frames.

Force Tempo has one sibling in Lull that is **not** here: *push through quiet
parts*, which speeds up further inside a dead spot. Gapless answers that case
differently already, with the interior-silence cap.

Note that **Next is a hard cut, deliberately** — it tears the mixer timeline down
and starts the new track at once. Crossfade applies to the track that follows
naturally, not to a skip.

## License

MIT — see [LICENSE](LICENSE).
