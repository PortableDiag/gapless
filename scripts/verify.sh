#!/usr/bin/env bash
# Proves the gapless claim rather than asserting it.
#
# Renders the two test-tone halves through the REAL engine (the actual Player,
# the actual about-to-finish handoff, only the audio sink swapped for a WAV
# writer), then measures the result. Also runs a set of deliberately broken
# captures through the same analyser — a check that cannot fail proves nothing.
#
# Then the three features that are not "is the splice gapless": trimming silence
# recorded into a rip, capping a pause inside a track, and the equal-power
# crossfade. Each of those is rendered with the feature off as well as on, so a
# pass cannot come from a fixture that never had the defect.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${TMPDIR:-/tmp}/gapless-verify"
mkdir -p "$OUT"

# hole.mp3 is one of the feature fixtures, which older testdata/ trees predate —
# checking only for the tone pair would leave them half-built.
if [ ! -f testdata/mp3-part1.mp3 ] || [ ! -f testdata/hole.mp3 ]; then
  ./scripts/make-test-tones.sh
fi

echo "=== rendering the engine to disk ==="
cargo run --release --quiet --example capture -- \
  "$OUT/mp3.wav" testdata/mp3-part1.mp3 testdata/mp3-part2.mp3
cargo run --release --quiet --example capture -- \
  "$OUT/flac.wav" testdata/flac-part1.flac testdata/flac-part2.flac

echo
echo "=== MP3 (encoder padding present — the real test) ==="
python3 scripts/verify-gapless.py "$OUT/mp3.wav"

echo
echo "=== FLAC (lossless control) ==="
python3 scripts/verify-gapless.py "$OUT/flac.wav"

echo
echo "=== negative controls (a check that cannot fail proves nothing) ==="
python3 scripts/verify-gapless.py --negative "$OUT/mp3.wav"

# Each feature is rendered twice — off, then on — and the check requires the OFF
# render to show the defect. A trim that passes because the fixture had no
# silence in it would prove nothing, which is the same argument as above.
echo
echo "=== features (each measured against the same render with it off) ==="

cargo run --release --quiet --example capture -- \
  "$OUT/trim-off.wav" testdata/sil-part1.mp3 testdata/mp3-part2.mp3
GAPLESS_TRIM=1 cargo run --release --quiet --example capture -- \
  "$OUT/trim-on.wav" testdata/sil-part1.mp3 testdata/mp3-part2.mp3
python3 scripts/verify-features.py trim "$OUT/trim-off.wav" "$OUT/trim-on.wav"

cargo run --release --quiet --example capture -- \
  "$OUT/inner-off.wav" testdata/hole.mp3
GAPLESS_INNER=1.0 cargo run --release --quiet --example capture -- \
  "$OUT/inner-on.wav" testdata/hole.mp3
python3 scripts/verify-features.py inner "$OUT/inner-off.wav" "$OUT/inner-on.wav" 1.0

cargo run --release --quiet --example capture -- \
  "$OUT/xf-off.wav" testdata/xf-440.mp3 testdata/xf-880.mp3
GAPLESS_CROSSFADE=3 cargo run --release --quiet --example capture -- \
  "$OUT/xf-on.wav" testdata/xf-440.mp3 testdata/xf-880.mp3
python3 scripts/verify-features.py crossfade "$OUT/xf-off.wav" "$OUT/xf-on.wav" 3
