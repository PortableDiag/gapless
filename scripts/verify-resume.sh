#!/usr/bin/env bash
# Proves that a track resumed part-way in still hands off to the next one.
#
# The bug this guards against: a branch's `len` was the length of the whole
# *song*, even when the branch had been told to start 227 s in and would only
# ever play the remainder. `schedule_following` places the next track at
# `start + len - crossfade`, so the follower was scheduled minutes after the
# audio actually ran out. What you heard was the track end, then dead air, and
# no crossfade — for the whole queue, because the same thing happened to every
# resumed branch.
#
# Resuming a saved session and seeking are the SAME code path (`start_at`), so a
# headless seek reproduces the session-resume bug exactly.
#
#   ./scripts/verify-resume.sh
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${TMPDIR:-/tmp}/gapless-resume"
mkdir -p "$OUT"
[ -f testdata/xf-440.mp3 ] || ./scripts/make-test-tones.sh

# Two 10 s tones. Resume 5 s into the first: 5 s of it are left, so
#   no crossfade -> 5 + 10            = 15.0 s
#   crossfade 4  -> the fade is clamped to half the *remaining* 5 s, i.e. 2.5 s,
#                   so the tracks overlap by 2.5 s -> 2.5 + 10 = 12.5 s
# Before the fix these rendered 20.0 s and 16.0 s: the follower was scheduled
# against the full 10 s track, leaving a 5 s hole where the music had stopped.
check() { # label wav expected_secs max_silence_ms
  python3 - "$@" <<'PY'
import sys, wave, audioop
label, path, want, max_sil = sys.argv[1], sys.argv[2], float(sys.argv[3]), float(sys.argv[4])
w = wave.open(path); n, r = w.getnframes(), w.getframerate()
data = w.readframes(n)
secs = n / r
step = int(r * 0.02)
worst = run = 0
for i in range(0, n - step, step):
    if audioop.rms(data[i*4:(i+step)*4], 2) < 50:
        run += 1; worst = max(worst, run)
    else:
        run = 0
sil = worst * 20
ok = abs(secs - want) < 0.05 and sil <= max_sil
print(f"  [{'PASS' if ok else 'FAIL'}] {label:28} {secs:6.3f} s (want {want:.3f})"
      f"   longest hole {sil:4d} ms")
sys.exit(0 if ok else 1)
PY
}

echo "=== resumed 5 s into a 10 s track, then handing off ==="
GAPLESS_SEEK=5 cargo run --release --quiet --example capture -- \
  "$OUT/plain.wav" testdata/xf-440.mp3 testdata/xf-880.mp3 2>/dev/null
check "resume, no crossfade" "$OUT/plain.wav" 15.0 60

GAPLESS_SEEK=5 GAPLESS_CROSSFADE=4 cargo run --release --quiet --example capture -- \
  "$OUT/xf.wav" testdata/xf-440.mp3 testdata/xf-880.mp3 2>/dev/null
check "resume, 4 s crossfade" "$OUT/xf.wav" 12.5 60

echo "=== control: no resume, so the numbers must not move ==="
cargo run --release --quiet --example capture -- \
  "$OUT/c1.wav" testdata/xf-440.mp3 testdata/xf-880.mp3 2>/dev/null
check "no resume, no crossfade" "$OUT/c1.wav" 20.0 60

GAPLESS_CROSSFADE=4 cargo run --release --quiet --example capture -- \
  "$OUT/c2.wav" testdata/xf-440.mp3 testdata/xf-880.mp3 2>/dev/null
check "no resume, 4 s crossfade" "$OUT/c2.wav" 16.0 60

echo "all resume checks passed"
