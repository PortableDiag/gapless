#!/usr/bin/env bash
# Proves that a shuffle/repeat change made *only over MPRIS* survives a crash,
# and that MPRIS cannot silently downgrade favorites shuffle to a plain one.
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

if [ "$FAILED" = 0 ]; then
  echo "[PASS] 3/3"
else
  echo "[FAIL] see above"
  exit 1
fi
