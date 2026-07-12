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
- Repeat off/all/one, shuffle, ReplayGain, MPRIS2 (media keys + lock screen) —
  and a mode toggled from the lock screen or `playerctl` repaints the buttons and
  is saved, exactly as a click on them would be.
- M3U/M3U8/PLS playlists, **in playlist order**.
- Album art, per-track detail (year, genre, codec, sample rate, bit depth).
- **Resumes your session** — folder or playlist, volume, shuffle, repeat, and the
  track and position you stopped at. Cued up, not auto-played.
- **Start at login** — a switch in the settings popover, no fiddling with
  `~/.config/autostart` by hand.

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
negative controls       7/7 broken splices caught
silence trim            1002 ms gap  ->  9 ms
interior silence cap    3000 ms pause -> 1015 ms at a 1.0 s cap
crossfade 3 s           20.0 s -> 17.0 s, equal-power crossover
real library (ADM)      1064 ms gap at the join  ->  0 ms
```

See **[docs/VERIFICATION.md](docs/VERIFICATION.md)** for how and why, including
why the obvious way to test this is wrong.

Two more checks, each written after a real bug got past the ones above:

```sh
./scripts/verify-resume.sh       # a track resumed part-way in must still hand off
./scripts/verify-mpris-modes.sh  # a mode set over D-Bus must survive a SIGKILL
```

## Documentation

| | |
|---|---|
| **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** | How the engine works, and the three approaches that failed first |
| **[docs/VERIFICATION.md](docs/VERIFICATION.md)** | How the gapless claim is proved rather than asserted |
| **[docs/DEVELOPING.md](docs/DEVELOPING.md)** | Layout, the tools in `examples/`, gotchas |
| **[docs/SESSION-2026-07-12.md](docs/SESSION-2026-07-12.md)** | Latest session: the MPRIS mode-persistence fix, and the open crossfade bug |
| **[CHANGELOG.md](CHANGELOG.md)** | What changed, when |

## Status

Playback is solid. Not yet done: no database (the library is rescanned on each
open), no search, no queue editing, no folder.jpg cover fallback.

Note that **Next is a hard cut, deliberately** — it tears the mixer timeline down
and starts the new track at once. Crossfade applies to the track that follows
naturally, not to a skip.
