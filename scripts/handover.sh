#!/usr/bin/env bash
# Replace a running Gapless with another copy WITHOUT a silent gap.
#
# The problem this solves: stopping the player and starting it again leaves the
# room silent for as long as the new copy takes to come up — and if the copy is
# being updated at the same time, for as long as the installer takes too. On this
# machine that was ~25 seconds of nothing, which is a long time if you are
# listening to music.
#
# Normally a second copy is impossible: `GApplication` is single-instance, so
# launching Gapless while it is running hands the request to the copy already
# there and exits. `--new-instance` opts out of that, and `--api-port` gives the
# replacement somewhere to listen while the outgoing copy still owns the
# configured port.
#
# The sequence, and why it is this way:
#
#   1. Start the replacement on a scratch port. It loads the library and cues the
#      track **while the old copy is still playing**. This is the slow part and
#      it costs nothing.
#   2. Match volume and modes, and pre-roll it *paused* at the right position, so
#      the audio pipeline is already built when the swap happens.
#   3. Read the outgoing position at the last possible moment, then play the
#      replacement and pause the old one back to back. The two overlap for a few
#      hundred milliseconds rather than leaving a hole — that is the right way
#      round: a listener notices silence, not a brief doubling.
#   4. Quit the old copy, and move the replacement onto the configured port.
#   5. **Check there is a real audio stream**, not just that the API says
#      "playing". A sink that failed to open the device leaves the pipeline
#      PLAYING and the position advancing while nothing reaches the speakers.
#      That exact failure happened here and cost two minutes of silence, because
#      the API's own word was taken as proof.
#
#   ./scripts/handover.sh                      # same binary, e.g. after an update
#   ./scripts/handover.sh /path/to/other/gapless
set -uo pipefail

APP_DEFAULT="$HOME/.local/share/linux-app-manager/apps/com.procomputation.Gapless/AppRun.wrapped"
NEW_BIN="${1:-$APP_DEFAULT}"
SCRATCH_PORT="${SCRATCH_PORT:-18499}"

[ -x "$NEW_BIN" ] || { echo "no executable at $NEW_BIN"; exit 1; }
[ -n "${DISPLAY:-}" ] || { echo "no DISPLAY — the player needs one"; exit 1; }

STATE="${XDG_CONFIG_HOME:-$HOME/.config}/gapless/state.json"
PORT=$(python3 -c "
import json
try: print(json.load(open('$STATE')).get('api_port') or 8421)
except Exception: print(8421)")
KEY=$("$NEW_BIN" --api-key) || { echo "could not read the API key"; exit 1; }

# The URL is built explicitly and passed as an argument. Writing this as
# `curl "http://127.0.0.1:$PORT$@"` glues the first argument onto the port —
# the URL becomes `127.0.0.1:18441-X` — and every call fails silently while the
# script reports success. That happened, and the handover appeared to pass while
# doing nothing at all.
OLD_URL="http://127.0.0.1:$PORT"
NEW_URL="http://127.0.0.1:$SCRATCH_PORT"
api() { curl -sf -m 10 -H "Authorization: Bearer $KEY" "$@"; }
jq_() { python3 -c "import json,sys;d=json.load(sys.stdin);print(eval(sys.argv[1]))" "$1"; }

if ! api "$OLD_URL/api/status" >/dev/null 2>&1; then
  echo "nothing is answering on 127.0.0.1:$PORT — starting a player rather than handing over"
  setsid "$NEW_BIN" >/dev/null 2>&1 < /dev/null &
  sleep 3
  exit 0
fi

SNAP=$(api "$OLD_URL/api/status")
echo "outgoing: $(printf '%s' "$SNAP" | jq_ 'd["track"]["title"]') "\
"(idx $(printf '%s' "$SNAP" | jq_ 'd["track"]["index"]'), v$(printf '%s' "$SNAP" | jq_ 'd["version"]'))"

IDX=$(printf '%s' "$SNAP"  | jq_ 'd["track"]["index"]')
VOL=$(printf '%s' "$SNAP"  | jq_ 'd["volume"]')
REP=$(printf '%s' "$SNAP"  | jq_ 'd["repeat"]')
SHUF=$(printf '%s' "$SNAP" | jq_ 'd["shuffle"]')
WAS_PLAYING=$(printf '%s' "$SNAP" | jq_ 'd["playing"]')

# ---- 1. bring the replacement up, alongside -----------------------------
START=$(date +%s.%N)
setsid "$NEW_BIN" --new-instance --api-port "$SCRATCH_PORT" >/dev/null 2>&1 < /dev/null &
for _ in $(seq 1 200); do api "$NEW_URL/api/status" >/dev/null 2>&1 && break; sleep 0.1; done
if ! api "$NEW_URL/api/status" >/dev/null 2>&1; then
  echo "the replacement never answered on $SCRATCH_PORT — leaving the current player alone"
  exit 1
fi
echo "replacement up in $(echo "$(date +%s.%N) - $START" | bc | cut -c1-4)s, old one still playing"

# ---- 2. match it, and pre-roll the pipeline -----------------------------
api -X POST -d "{\"mode\":\"$REP\"}"  "$NEW_URL/api/repeat"  >/dev/null
api -X POST -d "{\"mode\":\"$SHUF\"}" "$NEW_URL/api/shuffle" >/dev/null
api -X POST -d "{\"volume\":0.0}"     "$NEW_URL/api/volume"  >/dev/null
POS=$(api "$OLD_URL/api/status" | jq_ 'd["position_secs"]')
api -X POST -d "{\"index\":$IDX,\"position_secs\":$POS}" "$NEW_URL/api/play" >/dev/null
api -X POST "$NEW_URL/api/pause"  >/dev/null
api -X POST -d "{\"volume\":$VOL}" "$NEW_URL/api/volume" >/dev/null

# ---- 3. the swap: overlap, never a hole ---------------------------------
SWAP=$(date +%s.%N)
POS=$(api "$OLD_URL/api/status" | jq_ 'd["position_secs"]')
if [ "$WAS_PLAYING" = "True" ]; then
  api -X POST -d "{\"index\":$IDX,\"position_secs\":$POS}" "$NEW_URL/api/play" >/dev/null
fi
api -X POST "$OLD_URL/api/pause" >/dev/null
echo "swapped in $(echo "$(date +%s.%N) - $SWAP" | bc | cut -c1-5)s"

# ---- 4. retire the old copy, take its port ------------------------------
api -X POST "$OLD_URL/api/quit" >/dev/null 2>&1
GONE=no
for _ in $(seq 1 60); do
  api "$OLD_URL/api/status" >/dev/null 2>&1 || { GONE=yes; break; }
  sleep 0.25
done
if [ "$GONE" != "yes" ]; then
  echo "[FAIL] the outgoing player did not quit; two copies are running"
  exit 1
fi
api -X POST -d "{\"port\":$PORT}" "$NEW_URL/api/listen" >/dev/null

# ---- 5. prove it is audible, not merely "playing" -----------------------
sleep 1
FINAL=$(curl -sf -m 10 -H "Authorization: Bearer $KEY" "http://127.0.0.1:$PORT/api/status")
if [ -z "$FINAL" ]; then
  echo "[FAIL] nothing is answering on $PORT after the handover"
  exit 1
fi
echo "incoming: $(printf '%s' "$FINAL" | jq_ 'd["track"]["title"]') "\
"(v$(printf '%s' "$FINAL" | jq_ 'd["version"]'), at $(printf '%s' "$FINAL" | jq_ 'round(d["position_secs"],1)')s, "\
"sink $(printf '%s' "$FINAL" | jq_ 'd.get("audio_sink","?")'))"

STREAMS=$(wpctl status 2>/dev/null | grep -ci 'gapless\|AppRun.wrapped' || true)
PLAYING=$(printf '%s' "$FINAL" | jq_ 'd["playing"]')
if [ "$WAS_PLAYING" = "True" ] && [ "$PLAYING" != "True" ]; then
  echo "[FAIL] it is not playing"
  exit 1
fi
if [ "$WAS_PLAYING" = "True" ] && [ "${STREAMS:-0}" -eq 0 ]; then
  echo "[FAIL] the API says playing but NO audio stream is attached to the sink."
  echo "       That is the silent-player failure; restart it rather than trusting this."
  exit 1
fi
echo "[PASS] playing, $STREAMS stream(s) on the sink, API on $PORT"
