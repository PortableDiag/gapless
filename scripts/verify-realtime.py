#!/usr/bin/env python3
"""Analyse a RECORDING of the sink monitor — i.e. what actually reached the
speakers, through the real autoaudiosink, in real time.

This differs from verify-gapless.py in two ways that matter:

  * The recording has arbitrary leading/trailing silence, so the tone region has
    to be located rather than assumed.
  * The sink may resample (44.1 -> 48 kHz), so the sample rate comes from the
    file and the tone is found rather than assumed to be sample-aligned.

The question it answers: between the two halves of one continuous tone, did the
speakers receive an unbroken signal, or a gap — and if a gap, how long?
"""
import sys
import wave

import numpy as np

FREQ = 440.0


def load(path):
    with wave.open(path, "rb") as w:
        sr, n, ch = w.getframerate(), w.getnframes(), w.getnchannels()
        raw = w.readframes(n)
    x = np.frombuffer(raw, dtype=np.int16).astype(np.float64) / 32768.0
    if ch > 1:
        x = x.reshape(-1, ch).mean(axis=1)
    return x, sr


def main():
    path = sys.argv[1]
    x, sr = load(path)
    print(f"{path}\n  {len(x)} frames @ {sr} Hz = {len(x)/sr:.3f} s recorded")

    # Envelope over ~10 ms blocks, used to find the tone and to find holes in it.
    blk = max(1, int(sr * 0.010))
    frames = x[: len(x) // blk * blk].reshape(-1, blk)
    env = np.abs(frames).max(axis=1)
    peak = np.percentile(env, 99)
    if peak < 0.005:
        sys.exit("  recorded almost nothing — is the monitor capturing the right sink?")

    loud = env > 0.25 * peak
    idx = np.flatnonzero(loud)
    if len(idx) == 0:
        sys.exit("  no tone found in the recording")
    start, end = idx[0], idx[-1]
    tone_s = (end - start + 1) * blk / sr
    print(f"  tone runs {start*blk/sr:.3f}s .. {(end+1)*blk/sr:.3f}s  ({tone_s:.3f} s)")
    print()

    # --- holes inside the tone -----------------------------------------
    interior = ~loud[start:end + 1]
    holes = []
    run = 0
    for i, quiet in enumerate(interior):
        if quiet:
            run += 1
        elif run:
            holes.append((start + i - run, run))
            run = 0
    if run:
        holes.append((start + len(interior) - run, run))

    total_gap_ms = sum(n for _, n in holes) * blk / sr * 1000.0

    if holes:
        print(f"  [FAIL] {len(holes)} gap(s), {total_gap_ms:.1f} ms total:")
        for at, n in holes:
            print(f"           {n*blk/sr*1000:7.1f} ms at {at*blk/sr:.3f}s")
    else:
        print("  [PASS] no silence inside the tone")

    # --- phase continuity ----------------------------------------------
    seg = x[start * blk:(end + 1) * blk]
    t = np.arange(len(seg)) / sr
    bb = seg * np.exp(-2j * np.pi * FREQ * t)
    w = int(round(sr / FREQ))
    sm = np.convolve(bb, np.ones(w) / w, mode="valid")
    ph = np.unwrap(np.angle(sm))
    jump = np.convolve(np.abs(np.diff(ph)), np.ones(w), mode="valid")
    worst = float(jump.max())
    ok = worst < 0.15  # looser than the file test: a real sink adds jitter
    print(f"  [{'PASS' if ok else 'FAIL'}] max phase step {worst:.4f} rad"
          f"{'' if ok else f' at {(int(jump.argmax())+w)/sr + start*blk/sr:.3f}s'}")

    print()
    expected = 20.0
    drift = tone_s - expected
    print(f"  tone length {tone_s:.3f}s vs {expected:.1f}s expected ({drift*1000:+.0f} ms)")
    if holes or not ok:
        print("\n  REAL-TIME GAPLESS: FAIL")
        sys.exit(1)
    print("\n  REAL-TIME GAPLESS: PASS — the speakers got one unbroken tone.")


if __name__ == "__main__":
    main()
