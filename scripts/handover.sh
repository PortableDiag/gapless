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

# The outgoing player might be running with its control API down — that is
# exactly the state a broken listener leaves it in, and it is when a handover is
# most needed. MPRIS is the second way in: it is always there, and it can report
# the track and pause it, which is all the outgoing side has to do.
MPRIS_DEST=org.mpris.MediaPlayer2.Gapless
mpris_up() { gdbus call --session --dest "$MPRIS_DEST" --object-path /org/mpris/MediaPlayer2 \
  --method org.freedesktop.DBus.Properties.Get org.mpris.MediaPlayer2.Player PlaybackStatus >/dev/null 2>&1; }
mpris_get() { gdbus call --session --dest "$MPRIS_DEST" --object-path /org/mpris/MediaPlayer2 \
  --method org.freedesktop.DBus.Properties.Get org.mpris.MediaPlayer2.Player "$1" 2>/dev/null; }

OLD_VIA=api
if ! api "$OLD_URL/api/status" >/dev/null 2>&1; then
  if mpris_up; then
    OLD_VIA=mpris
    echo "the outgoing player's API is not answering on $PORT — using MPRIS for it instead"
  fi
fi

if [ "$OLD_VIA" = "api" ] && ! api "$OLD_URL/api/status" >/dev/null 2>&1; then
  echo "nothing is answering on 127.0.0.1:$PORT — starting a player rather than handing over"
  setsid "$NEW_BIN" >"${TMPDIR:-/tmp}/gapless-start-$$.log" 2>&1 < /dev/null &
  sleep 3
  exit 0
fi

if [ "$OLD_VIA" = "api" ]; then
  SNAP=$(api "$OLD_URL/api/status")
  TITLE=$(printf '%s' "$SNAP" | jq_ 'd["track"]["title"]')
  IDX=$(printf '%s' "$SNAP"  | jq_ 'd["track"]["index"]')
  VOL=$(printf '%s' "$SNAP"  | jq_ 'd["volume"]')
  REP=$(printf '%s' "$SNAP"  | jq_ 'd["repeat"]')
  SHUF=$(printf '%s' "$SNAP" | jq_ 'd["shuffle"]')
  WAS_PLAYING=$(printf '%s' "$SNAP" | jq_ 'd["playing"]')
  echo "outgoing: $TITLE (idx $IDX, v$(printf '%s' "$SNAP" | jq_ 'd["version"]'))"
else
  # MPRIS gives the title, the volume and whether it is playing. It does NOT
  # give a queue index, so the replacement is told the FILE instead — which is
  # better anyway: an index is only meaningful against one queue.
  TITLE=$(mpris_get Metadata | grep -oP "xesam:title': <'\K[^']+")
  OLD_URL_FILE=$(mpris_get Metadata | grep -oP "xesam:url': <'\K[^']+")
  VOL=$(mpris_get Volume | grep -oP '<\K[0-9.]+')
  case "$(mpris_get PlaybackStatus)" in *Playing*) WAS_PLAYING=True ;; *) WAS_PLAYING=False ;; esac
  IDX=""; REP=""; SHUF=""
  echo "outgoing: $TITLE (via MPRIS; its API is down)"
fi
[ -n "$VOL" ] || VOL=1.0

# The outgoing process, taken from the channel we are ALREADY talking to.
#
# Never `pgrep`. It matches by name across the whole machine, so it escapes a
# private D-Bus session and a private XDG_CONFIG_HOME without noticing — this
# script, run against test binaries inside what looked like a sandbox, found the
# operator's real player and SIGTERMed it mid-song. The API and the bus are both
# inherently scoped to the instance being handed over: a private bus has no
# owner for the MPRIS name unless the private player owns it.
if [ "$OLD_VIA" = "api" ]; then
  OLD_PID=$(printf '%s' "$SNAP" | jq_ 'd.get("pid","")')
else
  # One step: GetConnectionUnixProcessID takes a well-known name directly, so
  # there is no owner string to parse. (Parsing one is how this first went
  # wrong: `grep -oP "'\K[^']+"` matches TWICE on `(':1.2356',)` — the closing
  # quote opens a second match — and the pid lookup was handed `:1.2356\n,)`.)
  OLD_PID=$(gdbus call --session --dest org.freedesktop.DBus \
              --object-path /org/freedesktop/DBus \
              --method org.freedesktop.DBus.GetConnectionUnixProcessID "$MPRIS_DEST" 2>/dev/null \
            | grep -oP 'uint32 \K[0-9]+')
fi
if [ -z "$OLD_PID" ]; then
  echo "could not identify the outgoing player from its own API or its bus name;"
  echo "refusing to guess — nothing was touched."
  exit 1
fi

# ---- 1. bring the replacement up, alongside -----------------------------
START=$(date +%s.%N)
# Keep its stderr. A managed restart that discards it leaves you with nothing to
# read when the replacement comes up silent — which has happened, and cost two
# minutes of diagnosis from zero.
LOG="${TMPDIR:-/tmp}/gapless-handover-$$.log"
setsid "$NEW_BIN" --new-instance --api-port "$SCRATCH_PORT" >"$LOG" 2>&1 < /dev/null &
for _ in $(seq 1 200); do api "$NEW_URL/api/status" >/dev/null 2>&1 && break; sleep 0.1; done
if ! api "$NEW_URL/api/status" >/dev/null 2>&1; then
  echo "the replacement never answered on $SCRATCH_PORT — leaving the current player alone"
  echo "--- its output ---"; tail -20 "$LOG"
  exit 1
fi
echo "replacement up in $(echo "$(date +%s.%N) - $START" | bc | cut -c1-4)s, old one still playing"

# ---- 2. match it, and pre-roll the pipeline -----------------------------
[ -n "$REP" ]  && api -X POST -d "{\"mode\":\"$REP\"}"  "$NEW_URL/api/repeat"  >/dev/null
[ -n "$SHUF" ] && api -X POST -d "{\"mode\":\"$SHUF\"}" "$NEW_URL/api/shuffle" >/dev/null
api -X POST -d "{\"volume\":0.0}" "$NEW_URL/api/volume" >/dev/null

# Where the outgoing player is. MPRIS reports microseconds.
old_position() {
  if [ "$OLD_VIA" = "api" ]; then
    api "$OLD_URL/api/status" | jq_ 'd["position_secs"]'
  else
    mpris_get Position | grep -oP 'int64 \K[0-9]+' | awk '{printf "%.3f", $1/1000000}'
  fi
}
# Which track. By index when the API told us, by path when MPRIS did.
play_new() { # $1 = position
  if [ -n "$IDX" ]; then
    api -X POST -d "{\"index\":$IDX,\"position_secs\":$1}" "$NEW_URL/api/play" >/dev/null
  else
    PATH_JSON=$(FILE="$OLD_URL_FILE" python3 -c '
import json, os, urllib.parse
u = os.environ["FILE"]
print(json.dumps(urllib.parse.unquote(u[7:]) if u.startswith("file://") else u))')
    IDX=$(api "$NEW_URL/api/queue" | PJ="$PATH_JSON" python3 -c '
import json, os, sys
want = json.loads(os.environ["PJ"])
for t in json.load(sys.stdin)["tracks"]:
    if t["path"] == want:
        print(t["index"]); break')
    [ -n "$IDX" ] && api -X POST -d "{\"index\":$IDX,\"position_secs\":$1}" "$NEW_URL/api/play" >/dev/null
  fi
}
POS=$(old_position)
play_new "$POS"
api -X POST "$NEW_URL/api/pause"  >/dev/null
api -X POST -d "{\"volume\":$VOL}" "$NEW_URL/api/volume" >/dev/null

# ---- 3. the swap: overlap, never a hole ---------------------------------
SWAP=$(date +%s.%N)
POS=$(old_position)
if [ "$WAS_PLAYING" = "True" ]; then
  play_new "$POS"
fi
if [ "$OLD_VIA" = "api" ]; then
  api -X POST "$OLD_URL/api/pause" >/dev/null
else
  gdbus call --session --dest "$MPRIS_DEST" --object-path /org/mpris/MediaPlayer2 \
    --method org.mpris.MediaPlayer2.Player.Pause >/dev/null 2>&1
fi
echo "swapped in $(echo "$(date +%s.%N) - $SWAP" | bc | cut -c1-5)s"

# ---- 4. retire the old copy, take its port ------------------------------
if [ "$OLD_VIA" = "api" ]; then
  api -X POST "$OLD_URL/api/quit" >/dev/null 2>&1
else
  # No API to ask politely. SIGTERM runs the app's own save-and-quit handler,
  # so the session is still written; only SIGKILL would lose it.
  [ -n "$OLD_PID" ] && kill -TERM "$OLD_PID" 2>/dev/null
fi
GONE=no
for _ in $(seq 1 60); do
  if [ "$OLD_VIA" = "api" ]; then
    api "$OLD_URL/api/status" >/dev/null 2>&1 || { GONE=yes; break; }
  else
    kill -0 "${OLD_PID:-0}" 2>/dev/null || { GONE=yes; break; }
  fi
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
