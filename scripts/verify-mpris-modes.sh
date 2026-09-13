#!/usr/bin/env bash
# Three things MPRIS has to get right that nothing else checks: that a mode set
# only over D-Bus survives a crash, that it cannot silently downgrade favorites
# shuffle to a plain one, and that Play on a freshly launched player resumes
# where the last session stopped.
#
# The bug case 1 guards against: repeat and shuffle move no pipeline, so an MPRIS
# SetProperty used to set the flag on the Player and nothing else — no button
# repaint, no save scheduled. The state then survived only if the window was
# closed cleanly, because the close handler re-read the Player on its way out.
#
# The bug case 2 guards against: MPRIS `Shuffle` is a bool, and Gapless now has
# three shuffle states. We publish `true` for both on-states, so any client that
# echoes a property back — and lock-screen widgets do — hands us a bare `true`.
# Mapping that to plain shuffle turns the user's favorites shuffle off without
# anyone touching it, and the only visible symptom is that the music stops
# preferring their favorites.
#
# The bug case 4 guards against, found by operating the shipped v0.3.0 rather
# than by any test: the resume point lived in the GTK front-end's `Ui`, which
# `mpris.rs` cannot see. So the play *button* resumed where the last session
# stopped and a media key or a lock-screen Play did **not** — it called
# `play_index(0)`, started the queue from the top and threw the resume point
# away. The operator's music came back on the wrong track.
#
# So the kill here is deliberate and must stay a SIGKILL: closing the window
# would let the close handler mask the very bug under test.
#
# Runs on a private D-Bus session and a private XDG_CONFIG_HOME, so a copy of
# Gapless already running on your desktop is neither disturbed nor talked to by
# accident (it owns org.mpris.MediaPlayer2.Gapless on the real bus).
#
#   ./scripts/verify-mpris-modes.sh
set -uo pipefail
cd "$(dirname "$0")/.."

if [ -z "${DBUS_SESSION_BUS_ADDRESS_PRIVATE:-}" ]; then
  exec dbus-run-session -- env DBUS_SESSION_BUS_ADDRESS_PRIVATE=1 "$0" "$@"
fi

cargo build 2>/dev/null || { echo "build failed"; exit 1; }
BIN=$(cargo metadata --format-version 1 --no-deps \
      | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/debug/gapless

FAILED=0

# Launch on a throwaway config, drive MPRIS, SIGKILL, and print what reached disk.
#   $1  label
#   $2  seed state.json
#   $3  Shuffle value to Set
#   $4  LoopStatus value to Set, or "" to leave it alone
#   $5  expected repeat on disk
#   $6  expected shuffle_mode on disk
run_case() {
  local label="$1" seed="$2" shuffle_set="$3" loop_set="$4" want_repeat="$5" want_mode="$6"
  local CFG
  CFG=$(mktemp -d)
  export XDG_CONFIG_HOME="$CFG"
  mkdir -p "$CFG/gapless"
  printf '%s\n' "$seed" > "$CFG/gapless/state.json"

  "$BIN" >"$CFG/app.log" 2>&1 &
  local APP=$!
  sleep 4
  if ! kill -0 "$APP" 2>/dev/null; then
    echo "[FAIL] $label — app died on launch:"
    cat "$CFG/app.log"
    rm -rf "$CFG"
    FAILED=1
    return
  fi

  set_prop() {
    gdbus call --session --dest org.mpris.MediaPlayer2.Gapless \
      --object-path /org/mpris/MediaPlayer2 \
      --method org.freedesktop.DBus.Properties.Set \
      org.mpris.MediaPlayer2.Player "$1" "$2" >/dev/null
  }
  if ! set_prop Shuffle "<boolean $shuffle_set>"; then
    echo "[FAIL] $label — Set Shuffle failed (is MPRIS up?)"
    kill -9 "$APP"; rm -rf "$CFG"; FAILED=1; return
  fi
  if [ -n "$loop_set" ] && ! set_prop LoopStatus "<string '$loop_set'>"; then
    echo "[FAIL] $label — Set LoopStatus failed"
    kill -9 "$APP"; rm -rf "$CFG"; FAILED=1; return
  fi

  sleep 2                      # well past SAVE_DEBOUNCE (600 ms)
  kill -9 "$APP" 2>/dev/null   # SIGKILL: the close handler must not run
  wait "$APP" 2>/dev/null

  # shuffle_mode is the three-state key; shuffle is the legacy bool, still
  # written so an older build (and the first case below) can read it.
  local state="$CFG/gapless/state.json"
  read -r repeat mode legacy <<<"$(STATE="$state" python3 -c '
import json, os
s = json.load(open(os.environ["STATE"]))
print(s["repeat"], s.get("shuffle_mode"), s["shuffle"])')"

  echo "  $label"
  echo "    seeded:        $seed"
  echo "    set via MPRIS: Shuffle=$shuffle_set${loop_set:+  LoopStatus=$loop_set}"
  echo "    on disk:       repeat=$repeat  shuffle_mode=$mode  shuffle=$legacy"

  local want_legacy=True
  [ "$want_mode" = "off" ] && want_legacy=False

  if [ "$repeat" = "$want_repeat" ] && [ "$mode" = "$want_mode" ] \
     && [ "$legacy" = "$want_legacy" ]; then
    echo "    [PASS]"
  else
    echo "    [FAIL] expected repeat=$want_repeat shuffle_mode=$want_mode shuffle=$want_legacy"
    FAILED=1
  fi
  rm -rf "$CFG"
}

echo "MPRIS mode persistence"

# 1. The original check. Seeded with the opposite of what we set, so a pass
#    cannot be a no-op — and seeded in the *old* format, with no shuffle_mode
#    key at all, which also proves the migration path survives a crash.
run_case "an MPRIS-only change persists without a clean close" \
  '{ "volume": 1.0, "repeat": "off", "shuffle": false, "trim_silence": true }' \
  true Track one on

# 2. Shuffle=true against a player already in favorites mode must leave it in
#    favorites. This is what a lock-screen widget echoing our own published
#    `true` looks like.
run_case "MPRIS true does not downgrade favorites shuffle" \
  '{ "volume": 1.0, "repeat": "all", "shuffle": true, "shuffle_mode": "favorites", "trim_silence": true }' \
  true "" all favorites

# 3. ...but false still turns it off. Otherwise case 2 could pass by ignoring
#    the property altogether.
run_case "MPRIS false still turns favorites shuffle off" \
  '{ "volume": 1.0, "repeat": "all", "shuffle": true, "shuffle_mode": "favorites", "trim_silence": true }' \
  false "" all off

# ---------------------------------------------------------------------------
# 4. Play over MPRIS on a freshly launched player must resume the cued track.
echo
echo "MPRIS Play resumes the session"

[ -f testdata/sweep.mp3 ] || ./scripts/make-test-tones.sh >/dev/null 2>&1
TESTDATA="$PWD/testdata"
WANT="$TESTDATA/sweep.mp3"          # deliberately NOT the first track in scan order

CFG=$(mktemp -d)
export XDG_CONFIG_HOME="$CFG"
mkdir -p "$CFG/gapless"
cat > "$CFG/gapless/state.json" <<EOF
{ "volume": 0.0, "repeat": "off", "shuffle": false, "trim_silence": true,
  "last_source": "$TESTDATA", "last_track": "$WANT", "last_position_secs": 3.0 }
EOF

"$BIN" >"$CFG/app.log" 2>&1 &
APP=$!
sleep 5
if ! kill -0 "$APP" 2>/dev/null; then
  echo "  [FAIL] app died on launch:"; cat "$CFG/app.log"; FAILED=1
else
  # Wait for the bus name rather than sleeping at it. A fixed sleep raced the
  # MPRIS server coming up, the Play landed on nothing, and the check reported
  # an empty title as a failure of the thing it was testing.
  for _ in $(seq 1 30); do
    gdbus call --session --dest org.mpris.MediaPlayer2.Gapless \
      --object-path /org/mpris/MediaPlayer2 \
      --method org.freedesktop.DBus.Properties.Get \
      org.mpris.MediaPlayer2.Player PlaybackStatus >/dev/null 2>&1 && break
    sleep 0.5
  done

  if ! gdbus call --session --dest org.mpris.MediaPlayer2.Gapless \
       --object-path /org/mpris/MediaPlayer2 \
       --method org.mpris.MediaPlayer2.Player.Play >/dev/null 2>&1; then
    echo "    [FAIL] MPRIS never came up, so Play could not be sent"
    FAILED=1
  fi

  got=""
  for _ in $(seq 1 20); do
    got=$(gdbus call --session --dest org.mpris.MediaPlayer2.Gapless \
          --object-path /org/mpris/MediaPlayer2 \
          --method org.freedesktop.DBus.Properties.Get \
          org.mpris.MediaPlayer2.Player Metadata 2>/dev/null \
          | grep -oP "xesam:title': <'\K[^']+")
    [ -n "$got" ] && break
    sleep 0.5
  done
  # Match on the TITLE TAG, not the filename: sweep.mp3 is tagged
  # "Linear sweep 200-2200 Hz", and comparing against the file stem is how this
  # check first reported a false failure against a working fix.
  echo "    cued:   sweep.mp3 - deliberately not the first track in scan order"
  echo "    played: $got"
  case "$(printf '%s' "$got" | tr '[:upper:]' '[:lower:]')" in
    *sweep*) echo "    [PASS]" ;;
    "")      echo "    [FAIL] no metadata at all - MPRIS Play did nothing"; FAILED=1 ;;
    *)       echo "    [FAIL] MPRIS Play ignored the resume point and started the queue instead"
             FAILED=1 ;;
  esac
  kill -9 "$APP" 2>/dev/null; wait "$APP" 2>/dev/null
fi
rm -rf "$CFG"

echo
if [ "$FAILED" = 0 ]; then
  echo "[PASS] 4/4"
else
  echo "[FAIL] see above"
  exit 1
fi
