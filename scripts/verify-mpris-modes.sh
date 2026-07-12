#!/usr/bin/env bash
# Proves that a shuffle/repeat change made *only over MPRIS* survives a crash.
#
# The bug this guards against: repeat and shuffle move no pipeline, so an MPRIS
# SetProperty used to set the flag on the Player and nothing else — no button
# repaint, no save scheduled. The state then survived only if the window was
# closed cleanly, because the close handler re-read the Player on its way out.
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

CFG=$(mktemp -d)
trap 'rm -rf "$CFG"' EXIT
export XDG_CONFIG_HOME="$CFG"
mkdir -p "$CFG/gapless"

# Seed the opposite of what we are about to set, so a pass cannot be a no-op.
cat > "$CFG/gapless/state.json" <<'EOF'
{ "volume": 1.0, "repeat": "off", "shuffle": false, "trim_silence": true }
EOF

"$BIN" >"$CFG/app.log" 2>&1 &
APP=$!
sleep 4
kill -0 "$APP" 2>/dev/null || { echo "app died on launch:"; cat "$CFG/app.log"; exit 1; }

set_prop() {
  gdbus call --session --dest org.mpris.MediaPlayer2.Gapless \
    --object-path /org/mpris/MediaPlayer2 \
    --method org.freedesktop.DBus.Properties.Set \
    org.mpris.MediaPlayer2.Player "$1" "$2" >/dev/null \
    || { echo "  Set $1 FAILED (is MPRIS up?)"; kill -9 "$APP"; exit 1; }
}
set_prop Shuffle "<boolean true>"
set_prop LoopStatus "<string 'Track'>"

sleep 2                      # well past SAVE_DEBOUNCE (600 ms)
kill -9 "$APP" 2>/dev/null   # SIGKILL: the close handler must not run
wait "$APP" 2>/dev/null

repeat=$(python3 -c 'import json;print(json.load(open("'"$CFG"'/gapless/state.json"))["repeat"])')
shuffle=$(python3 -c 'import json;print(json.load(open("'"$CFG"'/gapless/state.json"))["shuffle"])')

echo "seeded:    repeat=off  shuffle=False"
echo "set via MPRIS:  LoopStatus=Track  Shuffle=true"
echo "on disk after SIGKILL:  repeat=$repeat  shuffle=$shuffle"

if [ "$repeat" = "one" ] && [ "$shuffle" = "True" ]; then
  echo "[PASS] MPRIS-only mode change was persisted without a clean close"
else
  echo "[FAIL] mode change was lost"
  exit 1
fi
