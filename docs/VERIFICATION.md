# Verification

"Gapless" is exactly the kind of claim that is easy to assert and easy to believe
about your own code. So it is measured instead.

```sh
./scripts/verify.sh
```

## What is actually under test

`examples/capture.rs` runs the **real engine** — the actual `Player`, the actual
mixer timeline, the actual probes — with only the audio sink swapped for a WAV
writer. It is not a reimplementation for testing purposes. What lands in the file
is exactly what would have hit the speakers.

```sh
cargo run --release --example capture -- out.wav track1.mp3 track2.mp3
```

| Env var | Effect |
|---|---|
| `GAPLESS_TRIM=1` | skip silence at track edges |
| `GAPLESS_INNER=1.0` | cap silence inside a track at 1.0 s |
| `GAPLESS_CROSSFADE=3` | 3 s crossfade |
| `GAPLESS_SEEK=10` | seek to 10 s and render from there |
| `GAPLESS_REALTIME=1` | render at 1× instead of as fast as possible |

`GAPLESS_REALTIME` matters more than it looks. With `sync=false` an 11-second
track renders in ~100 ms, so anything that has to *react* mid-stream loses the
race. Any test of timing must set it.

## The test signal

`scripts/make-test-tones.sh` builds one continuous 440 Hz sine and cuts it into
two 10-second halves.

At 44.1 kHz, ten seconds of 440 Hz is **exactly 4400 whole cycles**. The cut
therefore lands precisely on a cycle boundary, and part 2 resumes at the exact
phase part 1 ended. Spliced correctly, the two files are *mathematically* one
unbroken 20-second tone. Any defect is a discontinuity in a pure sine, which is
about the most detectable thing in signal processing.

It also builds the same pair as FLAC. That is the **control**: FLAC is lossless
with no encoder padding, so it should be seamless even in a mediocre player. The
MP3 pair is the real test, because LAME adds ~31 ms of encoder delay and padding
to *every* file:

```
flac-part1.flac   10.000 s
mp3-part1.mp3     10.031 s   <- the 31 ms is encoder padding
```

Two MP3s must still render as exactly 20.000 s. If they don't, the padding isn't
being stripped, and that alone will make every MP3 album click.

## Testing the seek without touching the mouse

`testdata/sweep.mp3` is a 20-second linear sweep, 200 Hz → 2200 Hz. Frequency
therefore *encodes position*: `f ≈ 242 + 100·t`. Measure the pitch at the start of
a render and you know exactly where the seek landed.

```
seek to 10 s:  rendered 10.00 s
  output t=0.5s -> 1292 Hz  = track position 10.5 s   (expected 10.5)
  output t=3.0s -> 1542 Hz  = track position 13.0 s   (expected 13.0)
```

This is how the "a seek plays silence" bug was found. Driving the real GUI with
synthetic clicks is a bad idea on a machine with other work on it — and it tells
you less.

## The three checks

`scripts/verify-gapless.py`:

| Check | Catches |
|---|---|
| **Length** — exactly 882000 frames | encoder padding not stripped, or a lossy splice |
| **Silence** — none in the interior | pipeline teardown between tracks |
| **Phase continuity** | an "almost gapless" splice that clicks |

### Why the obvious phase test is wrong

The natural way to find a splice is to look for a spike in the first difference:
a sine can only change so much between adjacent samples, so a step should stick
out.

**It doesn't work.** `max|dx|` only sees *amplitude* steps. If a splice lands near
a zero crossing, a 180° phase flip reverses the *slope* while the sample values
stay near zero — blatantly audible, and completely invisible to that test.

This is not hypothetical. Dropping exactly 50 samples (half a cycle) sails
straight through it:

```
drop   cycles  phase err    max|d|    verdict
  25    0.249      0.249   0.12869    DETECTED
  50    0.499      0.499   0.00836    invisible   <-- a blatant click
 150    1.497      0.497   0.01010    invisible
```

So the signal is **demodulated to baseband** instead. Multiplying by
`exp(-i·2π·440·t)` shifts the tone to DC, where its instantaneous phase should be
a constant. Averaging over one period kills the image at 2f. Any splice then shows
up as a step in that phase, wherever in the cycle it landed.

## The analyser is itself tested

A check that cannot fail proves nothing. So the analyser is run against
deliberately broken captures — a 20 ms silence splice, plus sample drops of
25/50/75/100/150/200 — and **must catch all seven**.

`verify.sh` runs this, or drive it directly against any good capture:

```sh
python3 scripts/verify-gapless.py --negative /tmp/gapless-verify/mp3.wav
```

The breaks are spliced into the midpoint of a *good* render — which is where the
two test-tone halves already meet, so an injected defect is exactly the defect a
broken pipeline would produce. The run prints which check caught what, because
that matrix is the whole argument:

```
  broken capture        length    silence   continuity  verdict
  20 ms silence splice  CAUGHT    CAUGHT    CAUGHT      caught
  drop 25 samples       ·         ·         CAUGHT      caught
  drop 50 samples       CAUGHT    ·         CAUGHT      caught
  drop 75 samples       CAUGHT    ·         CAUGHT      caught
  drop 100 samples      CAUGHT    ·         ·           caught
  drop 150 samples      CAUGHT    ·         CAUGHT      caught
  drop 200 samples      CAUGHT    ·         ·           caught
```

**Read the dots, not the CAUGHTs.** No single check catches all seven. A drop of
25 samples is 0.57 ms, comfortably inside the 1 ms length tolerance, and only the
phase test sees it. Drops of 100 and 200 samples are phase-*invisible* by
construction — at 440 Hz / 44.1 kHz one period is 100.2 samples, so those splices
remove a whole number of cycles — and only the length check sees them. Delete
either check and a real defect walks straight through. The three cover for each
other on purpose.

## Current results

```
gapless (crossfade 0, trim off)
  882000 frames @ 44100 Hz = 20.0000 s
  [PASS] length      882000 frames vs 882000 expected (+0.00 ms)
  [PASS] silence     0.00 ms of interior digital silence
  [PASS] continuity  max phase step 0.0184 rad (limit 0.05)
  FLAC control: PASS

negative controls        7/7 broken splices caught
silence trim             1000 ms gap -> 10 ms
interior silence cap     3000 ms pause -> 1010 ms (1.0 s cap), tail intact
crossfade 3 s            20.0 s -> 17.0 s, equal-power to 0.13%
```

Every line is produced by `./scripts/verify.sh`. An earlier version of this block
also carried `real library (ADM) 1064 ms gap at the join -> 0 ms`; that has been
removed rather than left in, because it was measured against a personal music
library that is not in the repo and cannot be re-measured by anyone reading this.
A number nobody can reproduce is an assertion with a decimal point on it, which
is the thing this document exists to avoid.

## The three features

These are not "is the splice gapless", and each regresses on its own, so
`scripts/verify-features.py` measures them separately. Every one is rendered
**twice** — the feature off, then on — and the check fails if the *off* render
does not show the defect it is supposed to fix. A trim that passes because the
fixture had no silence in it proves nothing, for the same reason the analyser has
negative controls.

| Check | Fixture | What the "off" render must show |
|---|---|---|
| `trim` | `sil-part1.mp3` + `mp3-part2.mp3` | a ~1000 ms hole at the join |
| `inner` | `hole.mp3` | a 3000 ms pause mid-track |
| `crossfade` | `xf-440.mp3` + `xf-880.mp3` | no stretch where both tones sound |

**The crossfade check measures the shape of the fade, not its length**, which
matters because duration alone cannot tell an equal-power fade from a linear one:
both shorten a 20 s render to exactly 17 s. The two fixtures are an octave apart,
so projecting the render onto each frequency recovers the gain the engine applied
to each branch — and equal power means `a² + b²` stays at 1 across the overlap.
Measured: **0.13%** departure. A synthetic linear fade of identical duration was
run through the same check and sags by **49.99%**, so the check is known to
separate them rather than assumed to.

Note that with a crossfade set, a render also fades **in** over its first seconds
and **out** over its last — the first and last branches have nothing to cross
with, so their envelopes run against silence. That is why the check reads the two
tones separately instead of watching total loudness, which those ramps dominate.

Fixtures come from `scripts/make-test-tones.sh`. `testdata/` is gitignored, so
everything the checks touch has to be reproducible from that script — it wasn't
before, and `verify-resume.sh` had been guarding itself with a call to a script
that did not build the file it was looking for.

## Testing against real speakers

`scripts/verify-realtime.py` analyses a recording of the sink monitor — what
actually reached the hardware, in real time, through `autoaudiosink`:

```sh
pw-record --target <sink> --rate 48000 --channels 2 --format s16 monitor.wav
python3 scripts/verify-realtime.py monitor.wav
```

Useful for ruling the *player* out when something sounds wrong: if the file
render is clean and the monitor recording is not, the problem is in PipeWire or
the sink, not the engine.
