#!/usr/bin/env python3
"""Measure the three playback features that are not "is the splice gapless".

verify-gapless.py answers one question — did two files render as one unbroken
tone. These are the other claims the README makes, each of which fixes a
different problem and can regress on its own:

  TRIM.      Silence recorded into the tail of a rip. No pipeline can close that
             hole, because it is real audio data; the engine has to know where it
             is and refuse to play it.

  INNER.     A pause *inside* one track. Different problem, different fix: it is
             capped rather than removed, because a four-bar rest is music and
             five minutes of dead air before a hidden track is not. Removing the
             pause also has to pull the rest of the track earlier, so the check
             looks at the tail as well as the hole.

  CROSSFADE. Two tracks overlapped with an equal-power envelope. Duration alone
             does not prove it: a LINEAR fade shortens the render by exactly the
             same amount and sounds wrong in the middle. So the real check is
             that loudness stays constant across the overlap.

Every mode takes a BASELINE render (the feature off) as well as the treated one,
and requires the baseline to show the defect. A trim check that passes because
the fixture had no silence in it would prove nothing — the same reason
verify-gapless.py has negative controls.

    verify-features.py trim       BASELINE.wav TRIMMED.wav
    verify-features.py inner      BASELINE.wav CAPPED.wav  CAP_SECONDS
    verify-features.py crossfade  BASELINE.wav FADED.wav   FADE_SECONDS
"""
import importlib.util
import pathlib
import sys

import numpy as np

# One definition of "how a capture is read" for the whole harness.
_spec = importlib.util.spec_from_file_location(
    "verify_gapless", pathlib.Path(__file__).with_name("verify-gapless.py"))
_vg = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_vg)
load = _vg.load

# Relative to the capture's own peak, never absolute — the same reasoning as
# src/silence.rs. A fixed threshold reads the noise floor a lossy codec decodes
# into a silent run-out as music.
SILENCE_FLOOR = 0.02
WIN_MS = 10
EDGE_GUARD_S = 0.05

# The crossfade fixtures are two different tones, so the two tracks can be told
# apart *while they overlap*: project the render onto each frequency and you
# recover the gain the engine applied to each branch. Equal power means
# a^2 + b^2 stays at 1 across the overlap. A LINEAR fade sags to 0.5 in the
# middle, so a 5% band separates them with room to spare.
#
# This is measured on the two components rather than on total loudness, because
# total loudness also picks up the fade-in at the very start of a render and the
# fade-out at the very end — those are the first and last branches fading from
# and to silence, which is a different behaviour from the crossover under test.
TONE_A, TONE_B = 440.0, 880.0
EQUAL_POWER_TOL = 0.05
PRESENT = 0.02


def silence_runs(x, sr):
    """Every run of near-silence, as (start_seconds, length_ms)."""
    amp = np.percentile(np.abs(x), 99.9)
    win = int(sr * WIN_MS / 1000)
    frames = x[: len(x) // win * win].reshape(-1, win)
    loud = np.abs(frames).max(axis=1) > SILENCE_FLOOR * amp

    runs, start = [], None
    for i, is_loud in enumerate(loud):
        if not is_loud and start is None:
            start = i
        elif is_loud and start is not None:
            runs.append((start * WIN_MS / 1000.0, (i - start) * WIN_MS))
            start = None
    if start is not None:
        runs.append((start * WIN_MS / 1000.0, (len(loud) - start) * WIN_MS))
    return runs


def interior_gap_ms(x, sr):
    """The longest silent run that is neither the head nor the tail."""
    duration = len(x) / sr
    return max(
        (ms for at, ms in silence_runs(x, sr)
         if at > EDGE_GUARD_S and at + ms / 1000.0 < duration - EDGE_GUARD_S),
        default=0.0,
    )


def branch_gains(x, sr, win_ms=50):
    """Per-window gain applied to each tone, normalised so full scale is 1.

    Projecting onto exp(-i*2*pi*f*t) and taking the magnitude is the amplitude of
    that tone in the window, and the two fixtures are an octave apart so neither
    leaks into the other's bin.
    """
    win = int(sr * win_ms / 1000)
    frames = x[: len(x) // win * win].reshape(-1, win)
    t = np.arange(win) / sr
    both = [np.abs((frames * np.exp(-2j * np.pi * f * t)).mean(axis=1))
            for f in (TONE_A, TONE_B)]
    peak = max(g.max() for g in both)
    return both[0] / peak, both[1] / peak, win_ms


def overlap_window(a, b, win_ms):
    """(start_s, seconds, mask) for the stretch where both tracks are audible."""
    mask = (a > PRESENT) & (b > PRESENT)
    idx = np.flatnonzero(mask)
    if not len(idx):
        return None, 0.0, mask
    return idx[0] * win_ms / 1000.0, (idx[-1] - idx[0] + 1) * win_ms / 1000.0, mask


def report(label, ok, detail):
    print(f"  [{'PASS' if ok else 'FAIL'}] {label:28}{detail}")
    return ok


def check_trim(baseline, treated):
    b, sr = load(baseline)
    t, _ = load(treated)
    before, after = interior_gap_ms(b, sr), interior_gap_ms(t, sr)

    ok = report("silence trim", before > 900 and after < 50,
                f"{before:.0f} ms gap -> {after:.0f} ms")
    if before <= 900:
        print(f"    - the BASELINE render has no gap to remove ({before:.0f} ms) "
              f"— the fixture is wrong, so a pass would have meant nothing")
    elif after >= 50:
        print(f"    - {after:.0f} ms of recorded silence survived the trim")
    return ok


def check_inner(baseline, treated, cap_s):
    b, sr = load(baseline)
    t, _ = load(treated)
    before, after = interior_gap_ms(b, sr), interior_gap_ms(t, sr)
    cap_ms = cap_s * 1000.0
    # The tail must move earlier by what was removed, not be eaten with it.
    expected = len(b) / sr - (before - after) / 1000.0
    tail_ok = abs(len(t) / sr - expected) < 0.10

    capped_ok = cap_ms * 0.85 < after < cap_ms * 1.15
    ok = report("interior silence cap", before > 2900 and capped_ok and tail_ok,
                f"{before:.0f} ms pause -> {after:.0f} ms at a {cap_s:.1f} s cap")
    if before <= 2900:
        print(f"    - the BASELINE render has no interior pause ({before:.0f} ms) "
              f"— the fixture is wrong")
    elif not capped_ok:
        print(f"    - {after:.0f} ms is not the {cap_ms:.0f} ms cap "
              f"(silence inside a track is capped, never removed)")
    elif not tail_ok:
        print(f"    - render is {len(t)/sr:.3f} s, expected {expected:.3f} s "
              f"— the tail was truncated instead of pulled earlier")
    return ok


def check_crossfade(baseline, treated, fade_s):
    b, sr = load(baseline)
    t, _ = load(treated)
    b_dur, t_dur = len(b) / sr, len(t) / sr
    want = b_dur - fade_s
    dur_ok = abs(t_dur - want) < 0.05

    # Control: with the feature off the tracks must butt together, so there is no
    # stretch where both are audible. Without this, "the tones overlap" could be
    # true of a render that was never crossfaded.
    _, base_overlap, _ = overlap_window(*branch_gains(b, sr))
    control_ok = base_overlap < 0.2

    start, seconds, mask = overlap_window(*(g := branch_gains(t, sr)))
    a_gain, b_gain, _ = g
    width_ok = abs(seconds - fade_s) < 0.5

    if mask.any():
        power = a_gain[mask] ** 2 + b_gain[mask] ** 2
        drift = float(np.abs(power - 1.0).max())
    else:
        drift = 1.0
    power_ok = drift < EQUAL_POWER_TOL

    ok = report(f"crossfade {fade_s:.0f} s",
                dur_ok and control_ok and width_ok and power_ok,
                f"{b_dur:.1f} s -> {t_dur:.1f} s, "
                f"equal-power to {drift * 100:.2f}% over a {seconds:.1f} s overlap")
    if not dur_ok:
        print(f"    - expected {want:.3f} s, got {t_dur:.3f} s "
              f"— the tracks are not overlapping by {fade_s:.0f} s")
    if not control_ok:
        print(f"    - the BASELINE render already overlaps by {base_overlap:.2f} s "
              f"— it is not a hard cut, so a pass would have meant nothing")
    if not width_ok:
        print(f"    - both tracks are audible for {seconds:.2f} s, expected "
              f"~{fade_s:.1f} s")
    if not power_ok:
        print(f"    - a^2 + b^2 departs from 1 by {drift * 100:.1f}% across the "
              f"overlap (limit {EQUAL_POWER_TOL * 100:.0f}%) — that is not an "
              f"equal-power fade; a linear one sags to 0.5, i.e. 50%")
    return ok


def main():
    args = sys.argv[1:]
    usage = ("usage: verify-features.py trim      BASELINE.wav TRIMMED.wav\n"
             "       verify-features.py inner     BASELINE.wav CAPPED.wav CAP_SECONDS\n"
             "       verify-features.py crossfade BASELINE.wav FADED.wav  FADE_SECONDS")
    if not args:
        sys.exit(usage)

    mode = args[0]
    if mode == "trim" and len(args) == 3:
        ok = check_trim(args[1], args[2])
    elif mode == "inner" and len(args) == 4:
        ok = check_inner(args[1], args[2], float(args[3]))
    elif mode == "crossfade" and len(args) == 4:
        ok = check_crossfade(args[1], args[2], float(args[3]))
    else:
        sys.exit(usage)

    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
