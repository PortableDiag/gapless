#!/usr/bin/env bash
# Proves the gapless claim rather than asserting it.
#
# Renders the two test-tone halves through the REAL engine (the actual Player,
# the actual about-to-finish handoff, only the audio sink swapped for a WAV
# writer), then measures the result. Also runs a set of deliberately broken
# captures through the same analyser — a check that cannot fail proves nothing.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${TMPDIR:-/tmp}/gapless-verify"
mkdir -p "$OUT"

[ -f testdata/mp3-part1.mp3 ] || ./scripts/make-test-tones.sh

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
