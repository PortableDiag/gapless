# Architecture

## The engine is a mixer timeline, not a playlist

`src/player.rs` builds one long-lived pipeline:

```
audiomixer ─ audioconvert ─ rgvolume ─ rglimiter ─ volume ─ autoaudiosink
    ▲
    ├── sink_0 ◀── [ uridecodebin ─ audioconvert ─ audioresample ─ capsfilter ─ queue ]   track N
    ├── sink_1 ◀── [ uridecodebin ─ audioconvert ─ audioresample ─ capsfilter ─ queue ]   track N+1
    └── …
```

Every track is its own **branch** feeding its own mixer pad. Two pad properties
do all the work:

- **`offset`** — where this track begins on the mixer's timeline.
- **`volume`** — automatable from a `GstControlSource`.

That single mechanism gives us everything:

| Setting | What it does to the pads |
|---|---|
| **Gapless** | `offset(N+1) = end of N`. The tracks abut exactly. |
| **Crossfade** | `offset(N+1) = end of N − overlap`, plus an equal-power volume envelope on each pad. |
| **Skip silence** | The branch's probe drops the silent head/tail, so the track's *length* shrinks — and the next `offset` moves in. |

Crossfade of 0 therefore collapses to exact sample-for-sample concatenation.
There is no separate "gapless path" and "crossfade path" to keep in sync.

Everything downstream of the mixer is pinned to **44.1 kHz stereo F32**. A caps
change mid-stream resets the audio sink, which is its own source of gaps.

## Silence

`src/silence.rs` decodes each track to 8 kHz mono on a worker thread (cheap, and
plenty to find an amplitude edge) and reports:

```rust
struct Trim {
    start: u64,           // first sample of actual audio
    end:   u64,           // one past the last
    total: u64,           // full decoded length
    holes: Vec<(u64,u64)>,// silent runs *inside* the music
}
```

The threshold is **relative to each track's own peak**, not absolute. This matters
more than it sounds: a fixed threshold reads the noise floor a lossy codec decodes
into a "silent" run-out as *music*. Before this was fixed, a 991 ms silence was
detected as 186 ms.

### Edge silence vs interior silence are different problems

**Edges are easy.** The pad probe drops buffers outside `[start, end]`. The branch
then hits EOS early, and — because we also computed a shorter length — the *next*
track's `offset` moves in to meet it. No seek, no glitch.

**Interiors are not.** Dropping the buffers in the middle of a track achieves
*nothing*: the mixer simply emits silence over the stretch where no buffers
arrived. To actually close the hole, everything after it must be **pulled
earlier**, which means rewriting timestamps in flight:

```rust
// buffers overlapping a cut: drop
if cuts.iter().any(|(a, b)| pts < *b && pts + dur > *a) {
    return PadProbeReturn::Drop;
}
// buffers past a cut: pull them earlier by everything removed before them
let removed = cuts.iter().filter(|(_, b)| *b <= pts).map(|(a,b)| b - a).sum();
buffer.make_mut().set_pts(ClockTime::from_nseconds(pts - removed));
```

The subtlety that cost the most time: you must drop buffers **overlapping** the
cut, not merely those **contained** in it. A buffer straddling the edge is
otherwise neither dropped (not fully inside) nor shifted (the cut doesn't end
before it) — so it passes through at its *old* timestamp, and that one backwards
jump makes `audiomixer` decide the stream is broken, resync, and **discard the
entire rewritten timeline**. The symptom is bizarre and silent: the hole is still
there *and* the track gets truncated.

Interior silence is capped rather than removed, and runs under 400 ms are never
touched. A four-bar rest is music; five minutes of dead air before a hidden track
is not.

## "Now playing" is derived from position, never from buffers

A branch's buffers reach the mixer **as soon as the branch is created**, which can
be *minutes* before that track is audible — the mixer holds them until the pad's
offset comes due. So a first-buffer callback is a useless "track started" signal;
using one made the UI display the *next* song for the whole of the current one.

The honest signal is the playback position against the timeline. Since we chose
every track's start time, this is exact:

```rust
branches.iter()
    .filter(|b| b.start_rt <= pos)
    .max_by_key(|b| b.start_rt)
```

`current` (what is playing) and `announced` (what the UI has been told) are
tracked separately — otherwise the very first track looks like "no change" to the
poller and is never announced at all.

## Scheduling

The next branch **must exist before the current one hits EOS**, or the mixer runs
out of pads and ends the stream. Branches are therefore built eagerly, as soon as
the silence analysis for the following track lands. A `followed` flag keeps
`schedule_following()` idempotent — it is reachable from several places and would
otherwise append the same track twice.

## Seeking

Seeking rebuilds the timeline with the current track at its head. An explicit jump
is allowed to be disruptive, and it keeps the pad offsets trivially correct — which
they would not be if we seeked a timeline that already had a crossfade scheduled
into it.

The seek itself is **the same probe mechanism as everything else**: the branch is
given the target as its `skip`, the probe drops everything before it, and the pad
offset shifts what remains back to running time zero. There is no seek event
anywhere in the engine.

That is worth stating plainly because the first implementation did *both* — probe
skip **and** a pipeline seek — and the two compounded. With the pad offset already
shifted by −10 s, the extra seek to 10 s landed at 20 s, off the end of the track,
so the branch EOSed having played nothing. A seek produced **silence**.

Cost: seeking decodes and discards everything before the target. Measured at
~0.46 s to chew through a whole 233 s MP3, so worst-case seek latency is about half
a second and it is bounded by the track length. Cheap enough not to warrant a
decoder seek, which would reintroduce the pad-offset interaction that caused the
bug above.

`Sched::skip` remembers how far into the song the branch was told to start, because
the mixer timeline always begins at zero and the song does not. Without it the
position display reads zero right after a seek.

## Session state

`~/.config/gapless/state.json`, written **whenever a setting changes** rather than
only on close. Saving on `close-request` alone looks fine and isn't: the app can be
killed, or quit with Ctrl-C in the terminal it was launched from, and a setting you
have to set twice is a setting that isn't remembered. Writes are debounced 600 ms,
because a volume drag emits a value per frame.

The playing position is checkpointed every 5 s, and on every pause — pausing is the
strongest available signal that this is where the user wants to come back to.

The resume point is stored as a **path, not an index**. A folder rescan or an edited
playlist renumbers the queue, and resuming into whatever track happens to sit at
index 12 today is worse than not resuming at all. On startup the path is looked up
in the freshly-loaded queue; if it isn't there (deleted, unmounted, different
source), the session simply doesn't resume.

Restoring **cues the track up without starting it** — `resume` holds `(index,
offset)` and the first press of play consumes it via `play_index_at`, which is the
same `start_at` path as a seek. A music player that begins blaring on login is a
music player you uninstall.

## Ratings, and the shuffle that uses them

Ratings are 1–5 stars, held in **`~/.config/gapless/ratings.json`** and keyed by
absolute path. Two deliberate choices:

**Not written into the files.** `POPM` (ID3) and `RATING` (Vorbis) exist, and
other players use them, but rating a song would then mean rewriting the user's
audio file — and `POPM` has no agreed 1–5-to-0–255 mapping anyway, so a number
read back only means something if you already know which player wrote it.

**Not in `state.json`.** That file is rewritten every few seconds while playing
and again from the SIGTERM handler. Ratings are the only thing on disk the user
typed in by hand, so they get their own file, written through a temporary file
and renamed, and written immediately rather than debounced — a rating changes
once per click, not once per frame.

`Shuffle` therefore has three states, not two: `Off`, `On`, `Favorites`.

Favorites shuffle is **an ordering, not a filter** — every track still plays
exactly once per pass. Each track gets a weight from its rating (1★ = 1, 2★ = 2,
3★ = 4, 4★ = 8, 5★ = 16, unrated = 2) and the order is drawn by
**Efraimidis–Spirakis** weighted sampling without replacement: give item *i* the
key `u^(1/wᵢ)` for `u` uniform on (0,1), then sort by key descending. That is
exactly equivalent to repeatedly drawing from what remains with probability
proportional to weight, but in one O(n log n) pass.

Three things about it are easy to get wrong and are therefore pinned by tests:

  * **The keys are computed in log space** — `ln(u)/wᵢ` — because `u^(1/16)` over
    a few thousand tracks crowds a lot of keys into the same few floats just below
    1.0 and the sort stops telling them apart.
  * **The sort is descending.** Ascending is a completely silent inversion: the
    queue still shuffles, it just prefers the tracks you rated *worst*. This was a
    real bug during development, caught only because the test measures the
    direction rather than checking the result is a permutation.
  * **Unrated sits at 2, level with 2★, not at the bottom.** "I have not judged
    this" is not the same statement as "I do not like this".

The ramp doubles per star on purpose. With linear weights a real library — which
is overwhelmingly unrated — buries its handful of 5★ tracks under sheer volume.
Measured over 4,000 passes of a 12-track queue (uniform would be 5.50):

```
5-star  mean slot 1.51      unrated  mean slot 5.96      1-star  mean slot 7.65
```

`cargo test -- --nocapture` prints that line, so a weighting that is technically
in the right direction but too weak to hear stays visible.

MPRIS is the awkward edge: its `Shuffle` property is a **bool**, so both on-states
publish as `true`. A client echoing that back — lock-screen widgets do — would
otherwise demote favorites shuffle to a plain one, silently. So `true` keeps
whatever on-mode is already set, and only turns on plain shuffle from `Off`.
`scripts/verify-mpris-modes.sh` checks all three directions.

## Start at login

An XDG autostart entry at `~/.config/autostart/com.procomputation.Gapless.desktop`,
written by `src/autostart.rs`.

**The file is the state.** There is deliberately no `autostart` flag in
`state.json`: the desktop's own startup-applications panel writes that same file,
so a second copy of the truth would drift the moment the user touched it there,
and the switch would then confidently show the wrong thing. `is_enabled()` reads
the file, and honours `Hidden=true` / `X-GNOME-Autostart-enabled=false`, which is
how a desktop usually *disables* an entry rather than deleting it.

`Exec` points at `~/.local/bin/gapless` — the copy `scripts/install.sh` puts
there — in preference to the running binary. The running binary is very often
`~/.cache/cargo-target/gapless/release/gapless`, and baking a *cache* path into a
login hook yields an autostart that silently stops working the first time someone
runs `cargo clean`. That is worse than one that never worked at all.

Launching at login restores the queue and cues the track up **paused**, exactly as
a manual launch does.

---

# Three approaches that failed first

Recorded because all three look reasonable, and two of them would have shipped as
bugs.

### 1. `playbin` + `about-to-finish`

The textbook gapless recipe, and it worked — verified sample-exact. Rejected for
two hard limits: it plays whatever is in the file (so it cannot skip recorded
silence — that needs a per-track segment, which the gapless handoff cannot
express), and it cannot crossfade.

### 2. `removesilence`

Looks purpose-built for the job. It is **mono only** (`channels=1` in its pad
template) — dropping it into a music pipeline either fails to negotiate or
downmixes the audio to mono. It silently did nothing here, which is the only
reason it didn't produce a nasty surprise.

### 3. A mid-stream segment-stop seek

Tell the pipeline "end this track early, where the music actually stops."
Non-flushing, so playback isn't interrupted. `playbin` accepts it (as
`start=SET(pos)`; `start=NONE` is rejected), and it *does* close the gap.

It also **duplicates ~1 s of already-buffered audio**, because the seek re-pushes
from the seek point while the sink's existing buffers are not discarded. Verified:
a phase discontinuity of 1.66 rad at the seek point. It trades a gap for a worse
artifact.
