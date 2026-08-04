#!/usr/bin/env python3
"""Decide whether a captured render of the test tones is actually gapless.

The input is expected to be the two halves of one continuous 440 Hz sine played
back to back. Three things must hold, each failing in a different way:

  1. LENGTH. Two 10.000 s halves must render as exactly 20.000 s = 882000 frames.
     MP3 carries ~31 ms of encoder delay+padding per file; a decoder that fails
     to strip it renders measurably long. Tolerance is deliberately tight (1 ms),
     because it is also the only check that can see a splice which drops a whole
     number of wave cycles — that is phase-invisible by construction.

  2. NO SILENCE. A pipeline torn down between tracks inserts digital silence.

  3. PHASE CONTINUITY. The subtle one, and the reason this script does not simply
     look for a spike in the first difference. A naive max|dx| test only sees
     *amplitude* steps. If a splice lands near a zero crossing, a 180-degree phase
     flip reverses the slope while the sample values stay near zero: blatantly
     audible, completely invisible to max|dx|. (Verified: dropping 50 samples —
     half a cycle — sails through that test.)

     So instead we demodulate. Multiplying by exp(-i*2*pi*f*t) shifts the tone to
     DC, where its instantaneous phase should be a constant. Averaging over one
     period kills the image at 2f. Any splice then shows up as a step in that
     phase, whatever part of the cycle it landed on.

Run with --negative to check the checker: seven deliberately broken captures are
synthesised from a good render, and every one of them must be caught by at least
one of the three checks. No single check catches all seven, which is the point —
they cover for each other. A drop of 100 samples is phase-invisible by
construction (one period is 100.2 samples at 440 Hz / 44.1 kHz) and is caught on
length; a drop of 25 samples is 0.57 ms, inside the length tolerance, and is
caught on phase.

    verify-gapless.py CAPTURE.wav              measure one render
    verify-gapless.py --negative CAPTURE.wav   prove the analyser can fail
"""
import sys
import wave

import numpy as np

FREQ = 440.0
EXPECTED_SECONDS = 20.0
LENGTH_TOL_MS = 1.0
# A step this large in the demodulated phase is not something a continuous tone
# can do. ~3 degrees; real captures sit two orders of magnitude below it.
PHASE_STEP_RAD = 0.05


def load(path):
    with wave.open(path, "rb") as w:
        sr, n, ch = w.getframerate(), w.getnframes(), w.getnchannels()
        raw = w.readframes(n)
    x = np.frombuffer(raw, dtype=np.int16).astype(np.float64) / 32768.0
    if ch > 1:
        x = x.reshape(-1, ch).mean(axis=1)
    return x, sr


def phase_steps(x, sr):
    """Instantaneous phase of the tone, and the largest jump in it."""
    t = np.arange(len(x)) / sr
    baseband = x * np.exp(-2j * np.pi * FREQ * t)

    # Boxcar of exactly one period: removes the 2f image, keeps the DC phase.
    w = int(round(sr / FREQ))
    kernel = np.ones(w) / w
    smoothed = np.convolve(baseband, kernel, mode="valid")

    phase = np.unwrap(np.angle(smoothed))
    d = np.abs(np.diff(phase))

    # The smoothing window straddles a splice for w samples, so a step arrives
    # smeared across it. Summing over that width recovers the true jump.
    jump = np.convolve(d, np.ones(w), mode="valid")
    return jump, w


def check_length(x, sr):
    drift_ms = (len(x) / sr - EXPECTED_SECONDS) * 1000.0
    ok = abs(drift_ms) < LENGTH_TOL_MS
    line = f"{len(x)} frames vs {int(EXPECTED_SECONDS * sr)} expected ({drift_ms:+.2f} ms)"
    why = (f"render is {drift_ms:+.2f} ms off — samples are being added or dropped "
           f"(encoder padding not stripped, or a lossy splice)")
    return ok, line, why


def check_silence(x, sr):
    amp = np.percentile(np.abs(x), 99.9)
    win = 64
    frames = x[: len(x) // win * win].reshape(-1, win)
    quiet = np.flatnonzero(np.abs(frames).max(axis=1) < 0.02 * amp)
    interior = quiet[(quiet > 2) & (quiet < len(frames) - 3)]
    silence_ms = len(interior) * win / sr * 1000.0
    ok = silence_ms < 1.0
    line = f"{silence_ms:.2f} ms of interior digital silence"
    at = interior[0] * win / sr if len(interior) else 0.0
    why = (f"{silence_ms:.1f} ms of silence at {at:.3f}s "
           f"— the pipeline is being torn down between tracks")
    return ok, line, why


def check_continuity(x, sr):
    jump, w = phase_steps(x, sr)
    worst = float(jump.max())
    ok = worst < PHASE_STEP_RAD
    line = f"max phase step {worst:.4f} rad (limit {PHASE_STEP_RAD})"
    at = (int(jump.argmax()) + w) / sr
    why = f"phase discontinuity of {worst:.3f} rad at {at:.4f}s — audible as a click"
    return ok, line, why


def run_checks(x, sr):
    """The three checks, as (name, ok, line, why). Nothing is printed."""
    return [
        ("length", *check_length(x, sr)),
        ("silence", *check_silence(x, sr)),
        ("continuity", *check_continuity(x, sr)),
    ]


def analyse(path):
    x, sr = load(path)
    amp = np.percentile(np.abs(x), 99.9)
    print(f"{path}")
    print(f"  {len(x)} frames @ {sr} Hz = {len(x) / sr:.4f} s   peak≈{amp:.3f}")
    print()

    failures = []
    for name, ok, line, why in run_checks(x, sr):
        print(f"  [{'PASS' if ok else 'FAIL'}] {name:<12}{line}")
        if not ok:
            failures.append(why)

    print()
    if failures:
        print("  GAPLESS: FAIL")
        for f in failures:
            print(f"    - {f}")
        sys.exit(1)
    print("  GAPLESS: PASS — the two files rendered as one unbroken tone.")


def broken_variants(x, sr):
    """Deliberately damaged copies of a good capture, spliced at the midpoint.

    The midpoint is where the two test-tone halves already meet, so a defect
    injected here is exactly the defect a broken pipeline would produce.
    """
    mid = len(x) // 2
    gap = np.zeros(int(round(0.020 * sr)))
    cases = [("20 ms silence splice", np.concatenate([x[:mid], gap, x[mid:]]))]
    for n in (25, 50, 75, 100, 150, 200):
        cases.append((f"drop {n} samples", np.concatenate([x[:mid], x[mid + n:]])))
    return cases


def negative_controls(path):
    """A check that cannot fail proves nothing. Every break must be caught."""
    x, sr = load(path)
    cases = broken_variants(x, sr)

    print(f"synthesised from {path}")
    print()
    print(f"  {'broken capture':<22}{'length':<10}{'silence':<10}{'continuity':<12}verdict")
    slipped = []
    for name, y in cases:
        caught = [not ok for _, ok, _, _ in run_checks(y, sr)]
        cells = "".join(f"{'CAUGHT' if c else '·':<10}" for c in caught[:2])
        cells += f"{'CAUGHT' if caught[2] else '·':<12}"
        print(f"  {name:<22}{cells}{'caught' if any(caught) else 'SLIPPED THROUGH'}")
        if not any(caught):
            slipped.append(name)

    print()
    total = len(cases)
    if slipped:
        print(f"  NEGATIVE CONTROLS: FAIL — {len(slipped)}/{total} slipped through")
        for name in slipped:
            print(f"    - {name} was not caught by any of the three checks")
        sys.exit(1)
    print(f"  NEGATIVE CONTROLS: PASS — {total}/{total} broken splices caught.")
    print("  No single check catches all of them; they cover for each other.")


def main():
    args = sys.argv[1:]
    if args and args[0] == "--negative":
        if len(args) < 2:
            sys.exit("usage: verify-gapless.py --negative CAPTURE.wav")
        negative_controls(args[1])
    elif len(args) == 1:
        analyse(args[0])
    else:
        sys.exit("usage: verify-gapless.py [--negative] CAPTURE.wav")


if __name__ == "__main__":
    main()
