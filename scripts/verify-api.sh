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
api -X POST -d '{"index":0}' "$BASE/api/play" >/dev/null
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
echo "=== playback settings ==="
api -X POST -d '{"crossfade_secs":3.0,"trim_silence":false}' "$BASE/api/settings" >/dev/null
check "crossfade set"   3.0   "$(api "$BASE/api/settings" | field crossfade_secs)"
check "trim off"        False "$(api "$BASE/api/settings" | field trim_silence)"
check "out of range is rejected" 400 \
      "$(code -X POST -H "Authorization: Bearer $KEY" -d '{"crossfade_secs":99}' "$BASE/api/settings")"
check "and did not take effect"  3.0 "$(api "$BASE/api/settings" | field crossfade_secs)"

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
