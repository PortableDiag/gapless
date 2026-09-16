#!/usr/bin/env bash
# Exercises the local control API against the real application.
#
# Every check here is a round trip: the request goes over a TCP socket to the
# running player, and the result is read back either from a later API call or
# from what the player wrote to disk. Nothing is mocked, and nothing asserts a
# behaviour the run did not actually produce.
#
# It also covers the two things most likely to be quietly wrong:
#
#   * **Authentication.** A control socket that answers without the key is worse
#     than no control socket, so the first two checks are that it refuses.
#   * **One state, not two.** An API call and a click must move the same player,
#     so a mode set over HTTP has to be visible in `state.json` afterwards — the
#     same file the settings popover writes.
#
# Runs on a private D-Bus session, a private XDG_CONFIG_HOME and a high port, so
# a copy of Gapless already running on your desktop is neither disturbed nor
# talked to by accident. Volume is set to zero before anything plays, so this
# does not make noise.
#
# Needs a display — it launches the real GTK application:
#
#   DISPLAY=:0 ./scripts/verify-api.sh
set -uo pipefail
cd "$(dirname "$0")/.."

if [ -z "${DBUS_SESSION_BUS_ADDRESS_PRIVATE:-}" ]; then
  exec dbus-run-session -- env DBUS_SESSION_BUS_ADDRESS_PRIVATE=1 "$0" "$@"
fi

PORT=18421
PASS=0
FAIL=0

# This check launches the real application, so a window appears on the operator's
# screen. That is an interruption even though it takes no input, so it waits for
# the machine to be free first. `xprintidle` was installed for this.
#   GAPLESS_WINDOWS_OK=1  run immediately anyway
if [ "${GAPLESS_WINDOWS_OK:-}" != "1" ]; then
  if ! ./scripts/wait-for-idle.sh "${GAPLESS_IDLE_SECS:-10}" "${GAPLESS_IDLE_WAIT:-900}"; then
    echo "[SKIP] the machine is in use and this opens a window — not interrupting."
    echo "       It will run cleanly when the desk is free, or set GAPLESS_WINDOWS_OK=1."
    exit 0
  fi
fi

cargo build 2>/dev/null || { echo "build failed"; exit 1; }
BIN=$(cargo metadata --format-version 1 --no-deps \
      | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/debug/gapless

[ -f testdata/mp3-part1.mp3 ] || ./scripts/make-test-tones.sh >/dev/null 2>&1
TESTDATA="$PWD/testdata"

CFG=$(mktemp -d)
trap 'rm -rf "$CFG"' EXIT
export XDG_CONFIG_HOME="$CFG"
mkdir -p "$CFG/gapless"
cat > "$CFG/gapless/state.json" <<EOF
{ "volume": 1.0, "repeat": "off", "shuffle": false, "trim_silence": true,
  "api_enabled": true, "api_port": $PORT }
EOF

KEY=$("$BIN" --api-key) || { echo "--api-key failed"; exit 1; }
[ -n "$KEY" ] || { echo "--api-key printed nothing"; exit 1; }

# The key file must not be readable by anyone else.
PERM=$(stat -c '%a' "$CFG/gapless/api-key")

"$BIN" >"$CFG/app.log" 2>&1 &
APP=$!
sleep 5
kill -0 "$APP" 2>/dev/null || { echo "app died on launch:"; cat "$CFG/app.log"; exit 1; }

BASE="http://127.0.0.1:$PORT"
api()  { curl -s -m 10 -H "Authorization: Bearer $KEY" "$@"; }
code() { curl -s -m 10 -o /dev/null -w '%{http_code}' "$@"; }
field() { python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get(sys.argv[1]))' "$1"; }

check() { # label  expected  actual
  if [ "$2" = "$3" ]; then
    printf '  [PASS] %-44s %s\n' "$1" "$3"; PASS=$((PASS+1))
  else
    printf '  [FAIL] %-44s got %s, want %s\n' "$1" "$3" "$2"; FAIL=$((FAIL+1))
  fi
}

echo "=== the key ==="
check "key is 64 hex characters" 64 "${#KEY}"
check "key file is mode 0600" 600 "$PERM"

echo
echo "=== authentication (must refuse before it obeys) ==="
check "no key at all"        401 "$(code "$BASE/api/status")"
check "wrong key"            401 "$(code -H 'Authorization: Bearer not-the-key' "$BASE/api/status")"
check "right key"            200 "$(code -H "Authorization: Bearer $KEY" "$BASE/api/status")"
check "X-API-Key header too" 200 "$(code -H "X-API-Key: $KEY" "$BASE/api/status")"
# A wrong key must not be able to change anything either, not just not read.
api -X POST -d '{"mode":"favorites"}' "$BASE/api/shuffle" >/dev/null
curl -s -m 10 -X POST -H 'Authorization: Bearer wrong' \
     -d '{"mode":"off"}' "$BASE/api/shuffle" >/dev/null
check "an unauthorised write changes nothing" favorites \
      "$(api "$BASE/api/status" | field shuffle)"

echo
echo "=== routing ==="
check "GET /api lists the endpoints" 200 "$(code -H "Authorization: Bearer $KEY" "$BASE/api")"
check "unknown route"                404 "$(code -H "Authorization: Bearer $KEY" "$BASE/api/nope")"
check "known route, wrong verb"      405 "$(code -H "Authorization: Bearer $KEY" "$BASE/api/play")"
check "trailing slash is the same route" 200 \
      "$(code -H "Authorization: Bearer $KEY" "$BASE/api/status/")"

echo
echo "=== the documentation endpoint ==="
DOCLEN=$(api "$BASE/api/docs" | wc -c)
BIG=$([ "$DOCLEN" -gt 2000 ] && echo yes || echo no)
check "GET /api/docs serves the reference" yes "$BIG"
CT=$(curl -s -m 10 -o /dev/null -w "%{content_type}" -H "Authorization: Bearer $KEY" "$BASE/api/docs")
MD=$(echo "$CT" | grep -q markdown && echo yes || echo no)
check "it is Markdown, not JSON" yes "$MD"
SEC=$(api "$BASE/api/docs?section=Ratings" | head -1 | grep -q "^## " && echo yes || echo no)
check "?section= narrows it" yes "$SEC"
check "a section that does not exist is a 404" 404 \
      "$(code -H "Authorization: Bearer $KEY" "$BASE/api/docs?section=nonsense")"
check "the docs need the key too" 401 "$(code "$BASE/api/docs")"

echo
echo "=== loading a queue ==="
COUNT=$(api -X POST -d "{\"path\":\"$TESTDATA\"}" "$BASE/api/open" | field count)
FILES=$(find "$TESTDATA" -maxdepth 1 -name '*.mp3' -o -maxdepth 1 -name '*.flac' | wc -l)
check "open loaded every file in testdata" "$FILES" "$COUNT"
check "missing path is a 404" 404 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"path":"/no/such/folder"}' "$BASE/api/open")"
check "queue endpoint agrees"  "$FILES" "$(api "$BASE/api/queue" | field count)"

echo
echo "=== modes ==="
api -X POST -d '{"mode":"favorites"}' "$BASE/api/shuffle" >/dev/null
check "shuffle favorites"  favorites "$(api "$BASE/api/status" | field shuffle)"
api -X POST -d '{"mode":"one"}' "$BASE/api/repeat" >/dev/null
check "repeat one"         one       "$(api "$BASE/api/status" | field repeat)"
check "a typo is rejected, not obeyed" 400 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"mode":"sideways"}' "$BASE/api/shuffle")"
check "still favorites after the typo" favorites "$(api "$BASE/api/status" | field shuffle)"

echo
echo "=== ratings ==="
api -X POST -d '{"index":0,"stars":5}' "$BASE/api/rating" >/dev/null
api -X POST -d '{"index":1,"stars":2}' "$BASE/api/rating" >/dev/null
FIRST=$(api "$BASE/api/queue" | python3 -c 'import json,sys;print(json.load(sys.stdin)["tracks"][0]["path"])')
ON_DISK=$(RJ="$CFG/gapless/ratings.json" P="$FIRST" python3 -c '
import json, os
print(json.load(open(os.environ["RJ"]))["stars"].get(os.environ["P"], 0))')
check "5 stars reached the sidecar file" 5 "$ON_DISK"
check "and the queue reports it"         5 \
      "$(api "$BASE/api/queue" | python3 -c 'import json,sys;print(json.load(sys.stdin)["tracks"][0]["rating"])')"
api -X POST -d '{"index":0,"stars":0}' "$BASE/api/rating" >/dev/null
CLEARED=$(RJ="$CFG/gapless/ratings.json" P="$FIRST" python3 -c '
import json, os
print(json.load(open(os.environ["RJ"]))["stars"].get(os.environ["P"], 0))')
check "0 clears it"                      0 "$CLEARED"
check "6 stars is rejected"              400 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"index":0,"stars":6}' "$BASE/api/rating")"
check "rating by path works too"         3 \
      "$(api -X POST -d "{\"path\":\"$FIRST\",\"stars\":3}" "$BASE/api/rating" \
         | python3 -c 'import json,sys;print(json.load(sys.stdin)["track"]["rating"])')"

echo
echo "=== playback (silenced first, so this makes no noise) ==="
api -X POST -d '{"volume":0.0}' "$BASE/api/volume" >/dev/null
check "volume is 0" 0.0 "$(api "$BASE/api/status" | field volume)"
# The response to a play must describe what it started. These three checks exist
# because driving the API by hand found all three defects the scripted checks
# above walked straight past: the answer named no track, rating without an index
# said "nothing is playing" while it was, and `playing` read false about a call
# that had just succeeded (the pipeline was still transitioning to PLAYING).
PLAYED=$(api -X POST -d '{"index":0}' "$BASE/api/play")
check "play names the track it started" yes \
      "$(printf '%s' "$PLAYED" | python3 -c 'import json,sys;print("yes" if (json.load(sys.stdin).get("track") or {}).get("title") else "no")')"
check "play reports playing immediately" True \
      "$(printf '%s' "$PLAYED" | field playing)"
check "rating with no index rates what is playing" 4 \
      "$(api -X POST -d '{"stars":4}' "$BASE/api/rating" \
         | python3 -c 'import json,sys;print(json.load(sys.stdin)["track"]["rating"])')"
sleep 2
check "playing"     True "$(api "$BASE/api/status" | field playing)"
P1=$(api "$BASE/api/status" | field position_secs)
sleep 2
P2=$(api "$BASE/api/status" | field position_secs)
ADVANCED=$(python3 -c "print('yes' if $P2 > $P1 else 'no')")
check "position advances while playing" yes "$ADVANCED"
api -X POST -d '{"position_secs":1.0}' "$BASE/api/seek" >/dev/null
sleep 1
NEAR=$(python3 -c "
p = $(api "$BASE/api/status" | field position_secs)
print('yes' if 0.5 <= p <= 3.0 else 'no (%.2f)' % p)")
check "seek lands where it was asked to" yes "$NEAR"
api -X POST "$BASE/api/pause" >/dev/null
sleep 1
check "pause"       False "$(api "$BASE/api/status" | field playing)"

echo
echo "=== sharing ==="
SHARE=$(api -X POST -d '{"index":0}' "$BASE/api/share")
check "share returns the details" yes \
      "$(printf '%s' "$SHARE" | python3 -c 'import json,sys;print("yes" if "Title" in json.load(sys.stdin).get("details","") else "no")')"
check "share names the file" yes \
      "$(printf '%s' "$SHARE" | python3 -c 'import json,sys;print("yes" if json.load(sys.stdin).get("file","").endswith((".mp3",".flac")) else "no")')"
OUT=$(mktemp -d)
COPIED=$(api -X POST -d "{\"index\":0,\"dest\":\"$OUT\"}" "$BASE/api/share")
check "share wrote the audio file" 1 "$(find "$OUT" -maxdepth 1 \( -name '*.mp3' -o -name '*.flac' \) | wc -l)"
check "share wrote the metadata beside it" 1 "$(find "$OUT" -maxdepth 1 -name '*.txt' | wc -l)"
check "the copy is byte-identical" yes \
      "$(SRC=$(printf '%s' "$SHARE" | python3 -c 'import json,sys;print(json.load(sys.stdin)["file"])');
         DST=$(find "$OUT" -maxdepth 1 \( -name '*.mp3' -o -name '*.flac' \) | head -1);
         cmp -s "$SRC" "$DST" && echo yes || echo no)"
# Sharing twice must not overwrite the first copy.
api -X POST -d "{\"index\":0,\"dest\":\"$OUT\"}" "$BASE/api/share" >/dev/null
check "sharing twice keeps both copies" 2 "$(find "$OUT" -maxdepth 1 \( -name '*.mp3' -o -name '*.flac' \) | wc -l)"
check "a bad destination is a 404" 404 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"index":0,"dest":"/no/such/dir"}' "$BASE/api/share")"
check "an unknown mode is refused, not guessed" 400 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"index":0,"mode":"telepathy"}' "$BASE/api/share")"
rm -rf "$OUT"

# The API must share FULLY - including the clipboard, which is the half that is
# easy to leave out because it is awkward to express over HTTP. The clipboard
# belongs to the running application, so the API can set exactly what the button
# sets, and this reads it back off the X server to prove it.
if command -v xclip >/dev/null; then
  api -X POST -d '{"index":0,"mode":"clipboard"}' "$BASE/api/share" >/dev/null
  sleep 1
  U=$(xclip -selection clipboard -t text/uri-list -o 2>/dev/null)
  P=$(xclip -selection clipboard -t UTF8_STRING -o 2>/dev/null)
  check "API clipboard share offers the file" yes \
        "$(case "$U" in file://*) echo yes;; *) echo no;; esac)"
  check "and the metadata from the same copy" yes \
        "$(case "$P" in *Title*) echo yes;; *) echo no;; esac)"
  api -X POST -d '{"index":1,"mode":"details"}' "$BASE/api/share" >/dev/null
  sleep 1
  D=$(xclip -selection clipboard -o 2>/dev/null)
  check "API details share puts text on the clipboard" yes \
        "$(case "$D" in *Title*File*) echo yes;; *) echo no;; esac)"
else
  echo "  [FAIL] xclip is not installed; the API clipboard share cannot be checked"
  FAIL=$((FAIL+1))
fi

echo
echo "=== playback settings ==="
api -X POST -d '{"crossfade_secs":3.0,"trim_silence":false}' "$BASE/api/settings" >/dev/null
check "crossfade set"   3.0   "$(api "$BASE/api/settings" | field crossfade_secs)"
check "trim off"        False "$(api "$BASE/api/settings" | field trim_silence)"
check "out of range is rejected" 400 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"crossfade_secs":99}' "$BASE/api/settings")"
check "and did not take effect"  3.0 "$(api "$BASE/api/settings" | field crossfade_secs)"

echo
echo "=== force tempo ==="
check "off by default"          False "$(api "$BASE/api/settings" | field force_tempo)"
check "and defaults to 140 BPM" 140   "$(api "$BASE/api/settings" | field target_bpm)"
api -X POST -d '{"force_tempo":true,"target_bpm":150}' "$BASE/api/settings" >/dev/null
check "switched on"             True  "$(api "$BASE/api/settings" | field force_tempo)"
check "target set"              150   "$(api "$BASE/api/settings" | field target_bpm)"
check "and status agrees"       150   "$(api "$BASE/api/status" | field target_bpm)"
check "a target below 60 is rejected"  400 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"target_bpm":30}' "$BASE/api/settings")"
check "a target above 200 is rejected" 400 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"target_bpm":400}' "$BASE/api/settings")"
check "and neither took effect"  150 "$(api "$BASE/api/settings" | field target_bpm)"
check "an absurd stretch is rejected"  400 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"max_stretch_percent":500}' "$BASE/api/settings")"

# The three states are the part of this feature most likely to be quietly wrong,
# because "unknown" and "none" both carry a null bpm and collapsing them looks
# harmless until a podcast is re-analysed on every pass.
TEMPO=$(api "$BASE/api/tempo?index=0")
STATE=$(printf '%s' "$TEMPO" | field state)
check "a track's tempo has a state" yes \
      "$(python3 -c "print('yes' if '$STATE' in ('known','none','unknown') else 'no ($STATE)')")"

# Type a tempo over it and the speed must follow arithmetically: 100 BPM against
# a 150 target is 1.5x, which the 30% ceiling clamps to 1.30.
api -X POST -d '{"index":0,"bpm":100}' "$BASE/api/tempo" >/dev/null
TYPED=$(api "$BASE/api/tempo?index=0")
check "a typed tempo is recorded"  known "$(printf '%s' "$TYPED" | field state)"
check "and is marked as the user's" user "$(printf '%s' "$TYPED" | field source)"
check "and it is the number typed"  100.0 "$(printf '%s' "$TYPED" | field bpm)"
check "the speed is clamped to the ceiling" 1.3 \
      "$(printf '%s' "$TYPED" | python3 -c 'import json,sys;print(round(json.load(sys.stdin)["speed"],4))')"
check "and that speed is in the sidecar file" 100.0 \
      "$(python3 -c "
import json
d = json.load(open('$CFG/gapless/bpm.json'))['tracks']
print(next(v['bpm'] for v in d.values() if v.get('source') == 'user'))")"

# A tempo outside any musical range is a refusal, not a clamp — the same rule as
# every other numeric field here.
check "an implausible tempo is refused" 400 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"index":0,"bpm":6000}' "$BASE/api/tempo")"
check "and the typed one survived"  100.0 "$(api "$BASE/api/tempo?index=0" | field bpm)"

# Clearing means "measure it again", NOT "this track has no tempo". Those are
# different states and landing in the wrong one means never looking again.
api -X POST -d '{"index":0}' "$BASE/api/tempo" >/dev/null
CLEARED=$(api "$BASE/api/tempo?index=0" | field state)
check "clearing restores it to unmeasured" yes \
      "$(python3 -c "print('yes' if '$CLEARED' != 'none' else 'no - it was recorded as having NO tempo')")"

# A fast track is left alone. This is the half of the feature that is a refusal,
# and an implementation that stretched everything would pass every check above.
api -X POST -d '{"index":0,"bpm":200}' "$BASE/api/tempo" >/dev/null
check "a track above the target is untouched" 1.0 \
      "$(api "$BASE/api/tempo?index=0" \
         | python3 -c 'import json,sys;print(round(json.load(sys.stdin)["speed"],4))')"
api -X POST -d '{"only_faster":false}' "$BASE/api/settings" >/dev/null
check "unless you say it may slow tracks down" yes \
      "$(api "$BASE/api/tempo?index=0" \
         | python3 -c 'import json,sys;print("yes" if json.load(sys.stdin)["speed"] < 1.0 else "no")')"
api -X POST -d '{"only_faster":true,"force_tempo":false}' "$BASE/api/settings" >/dev/null
check "and it can be switched back off" False "$(api "$BASE/api/settings" | field force_tempo)"

echo
echo "=== the listener can be moved, and moved onto itself ==="
# Rebinding to the SAME port is what the Regenerate-key button does. Before the
# accept thread was joined on drop, the old socket was still open when the new
# bind ran: "Address already in use", and the control API stayed DEAD until the
# app was restarted. It took the operator's API down the first time they used
# the button, so this check exists.
SAME=$(api -X POST -d "{\"port\":$PORT}" "$BASE/api/listen")
check "rebinding to the same port succeeds" "$PORT" \
      "$(printf '%s' "$SAME" | field port)"
sleep 1
check "and the API is still answering afterwards" 200 \
      "$(code -H "Authorization: Bearer $KEY" "$BASE/api/status")"
OTHER=$((PORT + 1))
api -X POST -d "{\"port\":$OTHER}" "$BASE/api/listen" >/dev/null
sleep 1
check "moving to another port works"        200 \
      "$(code -H "Authorization: Bearer $KEY" "http://127.0.0.1:$OTHER/api/status")"
check "and the old port is released"        000 \
      "$(curl -s -m 3 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $KEY" "$BASE/api/status")"
curl -sf -m 10 -X POST -H "Authorization: Bearer $KEY" -d "{\"port\":$PORT}" \
     "http://127.0.0.1:$OTHER/api/listen" >/dev/null
sleep 1
check "and it can come back"                200 \
      "$(code -H "Authorization: Bearer $KEY" "$BASE/api/status")"

echo
echo "=== one state, not two ==="
# SIGKILL, so nothing can be written by a clean-shutdown path: what is on disk
# has to have been saved as each API call was made.
sleep 2
kill -9 "$APP" 2>/dev/null
wait "$APP" 2>/dev/null
read -r disk_shuffle disk_repeat disk_xfade <<<"$(STATE="$CFG/gapless/state.json" python3 -c '
import json, os
s = json.load(open(os.environ["STATE"]))
print(s.get("shuffle_mode"), s["repeat"], s["crossfade_secs"])')"
check "shuffle reached state.json"   favorites "$disk_shuffle"
check "repeat reached state.json"    one       "$disk_repeat"
check "crossfade reached state.json" 3.0       "$disk_xfade"

echo
echo "=== the switch really is a switch ==="
# Same config, API turned off: nothing may answer on the port.
python3 - "$CFG/gapless/state.json" <<'EOF'
import json, sys
p = sys.argv[1]
s = json.load(open(p))
s["api_enabled"] = False
json.dump(s, open(p, "w"))
EOF
"$BIN" >"$CFG/app2.log" 2>&1 &
APP2=$!
sleep 5
OFF=$(curl -s -m 3 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $KEY" "$BASE/api/status")
kill -9 "$APP2" 2>/dev/null; wait "$APP2" 2>/dev/null
check "nothing answers when it is off" 000 "$OFF"

echo
if [ "$FAIL" = 0 ]; then
  echo "[PASS] $PASS/$PASS"
else
  echo "[FAIL] $FAIL of $((PASS+FAIL)) checks failed"
  exit 1
fi
