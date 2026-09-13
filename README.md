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
- **Star ratings, 1–5** — on the now-playing panel, on the number keys, or from
  a right-click on any row. Kept in a sidecar file, **not** written into your
  audio files.
- **Shuffle that prefers your favorites** — a third shuffle state that still
  plays every track once, but draws the order with the higher-rated tracks
  weighted towards the front. Measured, not asserted: over 4,000 passes of a
  12-track queue, 5-star tracks average slot 1.5 where a plain shuffle averages
  5.5.
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
- **About dialog**, and `gapless --version` for asking an installed copy what it
  is without opening a window.

## Install

Ubuntu 24.04 (or any Debian-ish distro with GStreamer 1.20+):

```sh
./scripts/setup-deps.sh     # needs sudo
cargo run --release
```

Then **Open Folder…** or **Open Playlist…**, and press play.

The three playback settings live behind the **gear button** in the header bar.

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

Two more checks, each written after a real bug got past the ones above:

```sh
./scripts/verify-resume.sh       # a track resumed part-way in must still hand off
./scripts/verify-mpris-modes.sh  # a mode set over D-Bus must survive a SIGKILL,
                                 # and must not downgrade favorites shuffle
./scripts/verify-api.sh          # 41 checks over a real socket against the real app
```

The weighted shuffle is measured by the unit tests, which print the distribution
rather than only asserting its direction:

```sh
cargo test -- --nocapture
```

Those two launch the real application, so they need a display — prefix them with
`DISPLAY=:0` if you are running over ssh or from anything that isn't a desktop
terminal. `verify.sh` renders through the engine with the audio sink swapped out
and needs nothing.

## Driving it from something else

```sh
KEY=$(gapless --api-key)
curl -s -H "Authorization: Bearer $KEY" http://127.0.0.1:8421/api/status
```

Switch it on under the gear button → **Remote control API**. It binds to loopback
only and every route needs the key. The whole reference is
**[docs/API.md](docs/API.md)**, and the running build serves its own copy at
`GET /api/docs` — so pointing an agent at the port is enough.

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

Note that **Next is a hard cut, deliberately** — it tears the mixer timeline down
and starts the new track at once. Crossfade applies to the track that follows
naturally, not to a skip.

## License

MIT — see [LICENSE](LICENSE).
