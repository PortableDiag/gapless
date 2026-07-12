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


def main():
    if len(sys.argv) < 2:
        sys.exit("usage: verify-gapless.py CAPTURE.wav")
    path = sys.argv[1]
    x, sr = load(path)
    dur = len(x) / sr
    amp = np.percentile(np.abs(x), 99.9)

    print(f"{path}")
    print(f"  {len(x)} frames @ {sr} Hz = {dur:.4f} s   peak≈{amp:.3f}")
    print()
    failures = []

    # --- 1. length ----------------------------------------------------
    drift_ms = (dur - EXPECTED_SECONDS) * 1000.0
    ok = abs(drift_ms) < LENGTH_TOL_MS
    print(f"  [{'PASS' if ok else 'FAIL'}] length      "
          f"{len(x)} frames vs {int(EXPECTED_SECONDS * sr)} expected ({drift_ms:+.2f} ms)")
    if not ok:
        failures.append(f"render is {drift_ms:+.2f} ms off — samples are being added or dropped "
                        f"(encoder padding not stripped, or a lossy splice)")

    # --- 2. interior silence ------------------------------------------
    win = 64
    frames = x[: len(x) // win * win].reshape(-1, win)
    quiet = np.flatnonzero(np.abs(frames).max(axis=1) < 0.02 * amp)
    interior = quiet[(quiet > 2) & (quiet < len(frames) - 3)]
    silence_ms = len(interior) * win / sr * 1000.0
    ok = silence_ms < 1.0
    print(f"  [{'PASS' if ok else 'FAIL'}] silence     "
          f"{silence_ms:.2f} ms of interior digital silence")
    if not ok:
        failures.append(f"{silence_ms:.1f} ms of silence at {interior[0] * win / sr:.3f}s "
                        f"— the pipeline is being torn down between tracks")

    # --- 3. phase continuity ------------------------------------------
    jump, w = phase_steps(x, sr)
    worst = float(jump.max())
    ok = worst < PHASE_STEP_RAD
    print(f"  [{'PASS' if ok else 'FAIL'}] continuity  "
          f"max phase step {worst:.4f} rad (limit {PHASE_STEP_RAD})")
    if not ok:
        at = (int(jump.argmax()) + w) / sr
        failures.append(f"phase discontinuity of {worst:.3f} rad at {at:.4f}s "
                        f"— audible as a click")

    print()
    if failures:
        print("  GAPLESS: FAIL")
        for f in failures:
            print(f"    - {f}")
        sys.exit(1)
    print("  GAPLESS: PASS — the two files rendered as one unbroken tone.")


if __name__ == "__main__":
    main()
