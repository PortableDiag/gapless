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

# ---------------------------------------------------------------------------
# Fixtures for the feature checks. These are NOT interchangeable with the pair
# above: that pair proves the splice is sample-exact, these prove that trimming,
# capping and crossfading do what they say.
#
# testdata/ is gitignored, so everything the verify scripts touch has to be
# reproducible from here. It wasn't: verify-resume.sh already guarded itself with
#   [ -f testdata/xf-440.mp3 ] || ./scripts/make-test-tones.sh
# and this script did not build xf-440.mp3, so on a clean checkout the guard ran,
# achieved nothing, and the render failed on a missing file.

# Two DIFFERENT tones, so a crossfade between them is visible as well as
# measurable — same amplitude, so equal-power means constant RMS across the
# overlap. (Also what verify-resume.sh renders.)
for f in 440 880; do
  ffmpeg -loglevel error -y \
    -f lavfi -i "sine=frequency=${f}:sample_rate=44100:duration=10" \
    -c:a pcm_s16le "xf-${f}.wav"
  lame --quiet -b 192 --tt "Tone ${f} Hz" --ta "Test Signal" \
    "xf-${f}.wav" "xf-${f}.mp3"
done

# 10 s of tone with 1.000 s of silence recorded into the tail — the ordinary
# non-gapless rip. Playing it into another track leaves a ~1 s hole that no
# pipeline can close, because the silence is real audio data.
ffmpeg -loglevel error -y \
  -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=10" \
  -af "apad=pad_dur=1" -t 11 -c:a pcm_s16le sil-part1.wav
lame --quiet -b 192 --tt "Tone with 1 s of trailing silence" --ta "Test Signal" \
  sil-part1.wav sil-part1.mp3

# 5 s tone, 3.000 s of silence, 5 s tone: a pause INSIDE one track, which is a
# different problem from silence at the edges and is capped rather than removed.
# The trailing 5 s is what proves the cap shifted the tail instead of eating it.
ffmpeg -loglevel error -y \
  -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
  -f lavfi -i "anullsrc=r=44100:cl=mono:d=3" \
  -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
  -filter_complex "[0:a][1:a][2:a]concat=n=3:v=0:a=1" \
  -c:a pcm_s16le hole.wav
lame --quiet -b 192 --tt "Tone with a 3 s hole in the middle" --ta "Test Signal" \
  hole.wav hole.mp3

# 20 s linear sweep, 200 Hz -> 2200 Hz: instantaneous frequency is 200 + 100*t,
# so pitch ENCODES position. Measure the tone at the start of a render and you
# know exactly where a seek landed. Phase is the integral: 2*pi*(200t + 50t^2).
ffmpeg -loglevel error -y \
  -f lavfi -i "aevalsrc='sin(2*PI*(200*t+50*t*t))':s=44100:d=20" \
  -c:a pcm_s16le sweep.wav
lame --quiet -b 192 --tt "Linear sweep 200-2200 Hz" --ta "Test Signal" \
  sweep.wav sweep.mp3

rm -f xf-440.wav xf-880.wav sil-part1.wav hole.wav sweep.wav

echo "Wrote $(pwd):"
ls -1 mp3-*.mp3 flac-*.flac xf-*.mp3 sil-part1.mp3 hole.mp3 sweep.mp3
echo
echo "Play part 1 and listen at the 10-second mark."
echo "  Gapless  -> one unbroken 20-second tone."
echo "  Not      -> an obvious CLICK where the files join."
