#!/usr/bin/env bash
# Builds an unambiguous gapless test.
#
# A single continuous 440 Hz sine is cut into two 10-second halves. At 44100 Hz,
# ten seconds of 440 Hz is exactly 4400 whole cycles, so the cut lands precisely
# on a cycle boundary — the second file resumes at the exact phase the first one
# ended. Played back-to-back with true gapless, the two files are indistinguishable
# from one unbroken 20-second tone.
#
# Any gap, however short, is a hard discontinuity in a steady sine, and your ear
# hears that as a distinct CLICK. You are not straining to notice a subtle pause;
# you are listening for an obvious tick that is either there or isn't.
#
# The MP3 pair is the real test: LAME adds encoder delay and padding to every
# file, so these will click in a player that ignores the LAME/Xing gapless tag
# even if its pipeline is otherwise seamless.
#
# The FLAC pair is the control: lossless, no encoder padding. If FLAC is seamless
# but MP3 clicks, the pipeline is fine and the padding handling is broken.
set -euo pipefail

OUT="$(dirname "$0")/../testdata"
mkdir -p "$OUT"
cd "$OUT"

for half in 1 2; do
  ffmpeg -loglevel error -y \
    -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=10" \
    -c:a pcm_s16le "part${half}.wav"
done

for half in 1 2; do
  lame --quiet -b 192 \
    --tt "Continuous Tone (part ${half} of 2)" \
    --ta "Test Signal" \
    --tl "Gapless Test MP3" \
    --tn "${half}" \
    "part${half}.wav" "mp3-part${half}.mp3"

  ffmpeg -loglevel error -y -i "part${half}.wav" \
    -metadata title="Continuous Tone (part ${half} of 2)" \
    -metadata artist="Test Signal" \
    -metadata album="Gapless Test FLAC" \
    -metadata track="${half}" \
    -c:a flac "flac-part${half}.flac"
done

rm -f part1.wav part2.wav

echo "Wrote $(pwd):"
ls -1 mp3-*.mp3 flac-*.flac
echo
echo "Play part 1 and listen at the 10-second mark."
echo "  Gapless  -> one unbroken 20-second tone."
echo "  Not      -> an obvious CLICK where the files join."
