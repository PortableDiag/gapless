# Developing

## Layout

```
src/
  main.rs      GTK4 + libadwaita UI
  player.rs    the engine — audiomixer timeline, scheduling, probes
  silence.rs   finds where the music really starts and stops
  library.rs   folder scan + tag reading (lofty)
  playlist.rs  M3U / M3U8 / PLS
  mpris.rs     MPRIS2 — media keys, lock screen
  settings.rs  ~/.config/gapless/state.json
examples/      diagnostic tools (below)
scripts/       setup, test-signal generation, verification
```

`src/lib.rs` exists so the engine can be driven headlessly by the test harness as
well as by the UI. That is the whole reason for the lib/bin split — `capture.rs`
drives the *real* `Player`, not a copy of it.

## Tools

```sh
# render the engine to a WAV instead of the speakers
cargo run --release --example capture -- out.wav a.mp3 b.mp3

# what does the silence analyser think of these files?
cargo run --release --example trim-info -- *.mp3

# parse a playlist and print it in playing order
cargo run --release --example dump-playlist -- some.m3u8
```

`GAPLESS_DEBUG=1` on the main binary prints engine internals.

## Gotchas

**Builds go to `~/.cache/cargo-target/gapless`, not `target/`.** The project lives
on an exfat volume, which has no POSIX file locking and no atomic renames — both
of which cargo depends on. See `.cargo/config.toml`. If you move the project to
ext4, delete that file.

**The dependency versions are load-bearing.** `gtk4`, `libadwaita`, `gstreamer`
and `glib` must all sit on the same glib generation (0.20) or you get two
incompatible `glib::Object` types and nothing compiles. The `v4_12` / `v1_5`
feature flags gate the bindings to a minimum C library version; without them
`FileDialog` and `ToolbarView` are not compiled in at all.

**`glib::filename_to_uri` requires an absolute path.** The naive fallback —
`format!("file://{path}")` — is worse than useless on a relative one: in
`file://testdata/song.mp3` the leading segment parses as a *hostname*, so
GStreamer goes looking for `/song.mp3` at the filesystem root. `player::uri_for`
absolutises first.

**A buffer modified in a pad probe must be `make_mut()`'d**, and buffers
*overlapping* a region you are cutting must be dropped, not merely those
*contained* in it. See `docs/ARCHITECTURE.md` — a single straddling buffer at a
stale timestamp makes `audiomixer` discard the whole rewritten timeline.

**Don't trust a first-buffer callback as "now playing."** The mixer receives a
branch's data long before that branch is audible. Derive the current track from
the playback position.

**Two of the three checks need a display.** `verify-resume.sh` and
`verify-mpris-modes.sh` launch the real application — from a desktop terminal you
never notice, but over ssh, from cron, or from a tool that doesn't inherit the
session environment, `DISPLAY` is unset and they fail in a way that doesn't
mention a display:

```sh
DISPLAY=:0 ./scripts/verify-mpris-modes.sh
```

`verify.sh` is genuinely headless — it renders through the `capture` example with
the audio sink swapped out — and needs nothing.

Two things that look like failures and aren't. `verify-mpris-modes.sh` re-execs
itself under `dbus-run-session`, which starts its own xdg-desktop-portal: the
`fusermount3: Permission denied` and portal warnings on stderr are normal noise,
and the verdict is in the last few lines. And because it pipes its own output, a
GTK startup failure gets stuck in a buffered pipe — redirect to a file rather
than piping to `tail` while you are debugging one.

## Before you commit

```sh
cargo build --release && ./scripts/verify.sh
```

All six results must stay green: gapless `PASS`, the FLAC control `PASS`, the
negative controls 7/7, and the silence-trim, interior-cap and crossfade checks.
These have caught real regressions in this codebase — including one where the
whole queue silently stopped after the first track.

Each feature check renders the feature **off** as well as on, and fails if the
off render does not show the defect it is meant to fix. That baseline is part of
the test, not scaffolding: a trim that passes because the fixture had no silence
in it proves nothing.

`testdata/` is gitignored, so anything a check depends on has to be built by
`scripts/make-test-tones.sh` — otherwise it works only on the machine it was
first made on. `verify.sh` runs that script for you when the tree is missing or
incomplete.
