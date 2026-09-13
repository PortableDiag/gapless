#!/usr/bin/env bash
# The two rating paths that no other check can reach: the number-key
# accelerators, and the right-click menu on a list row.
#
# Everything else in this project is verified through the engine, over D-Bus or
# over HTTP. These two are *keyboard and mouse*, and they were the last part of
# the ratings feature covered only by inspection.
#
# Why this needs care rather than a one-liner:
#
#   * **GTK4 ignores synthetic key events.** `xdotool key --window <id>` uses
#     XSendEvent and does nothing at all — silently. Only real XTEST input works,
#     which means the window has to genuinely hold focus.
#   * **A window manager can refuse focus** to a window that just appeared. KDE's
#     focus-stealing prevention did exactly that here until the window was mapped
#     and raised first. `windowmap` -> `windowraise` -> `windowactivate --sync`
#     -> `windowfocus --sync`, then *verify* with `getactivewindow` before
#     sending anything. If focus is refused this FAILS rather than skipping: a
#     check that quietly does nothing is worse than no check.
#
# Runs on a private D-Bus session and a private XDG_CONFIG_HOME, at volume 0, so
# a copy already running on your desktop is untouched and nothing makes noise.
# It does take the keyboard for a few seconds — that is unavoidable, because
# taking the keyboard is the thing being tested.
#
#   DISPLAY=:0 ./scripts/verify-input.sh
set -uo pipefail
cd "$(dirname "$0")/.."

if [ -z "${DBUS_SESSION_BUS_ADDRESS_PRIVATE:-}" ]; then
  exec dbus-run-session -- env DBUS_SESSION_BUS_ADDRESS_PRIVATE=1 "$0" "$@"
fi

command -v xdotool >/dev/null || { echo "[FAIL] xdotool is not installed"; exit 1; }
[ -n "${DISPLAY:-}" ] || { echo "[FAIL] no DISPLAY — this check drives the real window"; exit 1; }

PASS=0
FAIL=0
check() { # label expected actual
  if [ "$2" = "$3" ]; then
    printf '  [PASS] %-46s %s\n' "$1" "$3"; PASS=$((PASS+1))
  else
    printf '  [FAIL] %-46s got %s, want %s\n' "$1" "$3" "$2"; FAIL=$((FAIL+1))
  fi
}

cargo build 2>/dev/null || { echo "build failed"; exit 1; }
BIN=$(cargo metadata --format-version 1 --no-deps \
      | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/debug/gapless

[ -f testdata/sweep.mp3 ] || ./scripts/make-test-tones.sh >/dev/null 2>&1
TD="$PWD/testdata"

CFG=$(mktemp -d)
trap 'rm -rf "$CFG"' EXIT
export XDG_CONFIG_HOME="$CFG"
mkdir -p "$CFG/gapless"
# A track is cued so the star strip and the number keys have a target before
# anything plays, which is also the state the app launches in.
cat > "$CFG/gapless/state.json" <<EOF
{ "volume": 0.0, "trim_silence": true, "last_source": "$TD",
  "last_track": "$TD/sweep.mp3", "last_position_secs": 2.0 }
EOF
RJ="$CFG/gapless/ratings.json"

stars_for() { # $1 = basename
  RJ="$RJ" WANT="$TD/$1" python3 -c '
import json, os
try:
    print(json.load(open(os.environ["RJ"]))["stars"].get(os.environ["WANT"], 0))
except Exception:
    print(0)'
}
rated_count() {
  RJ="$RJ" python3 -c '
import json, os
try:
    print(len(json.load(open(os.environ["RJ"]))["stars"]))
except Exception:
    print(0)'
}

before=$(xdotool search --name "^Gapless$" 2>/dev/null | tr '\n' ' ')
"$BIN" >"$CFG/app.log" 2>&1 &
APP=$!
sleep 7
if ! kill -0 "$APP" 2>/dev/null; then
  echo "[FAIL] app died on launch:"; cat "$CFG/app.log"; exit 1
fi

# The new window is the one that was not there before. Matching by name alone
# picks up any other Gapless window on the display, including a stale one.
after=$(xdotool search --name "^Gapless$" 2>/dev/null | tr '\n' ' ')
WIN=""
for w in $after; do case " $before " in *" $w "*) ;; *) WIN=$w;; esac; done
if [ -z "$WIN" ]; then
  echo "[FAIL] the app started but put no new window on $DISPLAY"
  kill -9 "$APP"; exit 1
fi

xdotool windowmap "$WIN" 2>/dev/null
xdotool windowraise "$WIN" 2>/dev/null
xdotool windowactivate --sync "$WIN" 2>/dev/null
xdotool windowfocus --sync "$WIN" 2>/dev/null
sleep 1
ACTIVE=$(xdotool getactivewindow 2>/dev/null)
if [ "$ACTIVE" != "$WIN" ]; then
  echo "[FAIL] the window manager refused focus (active=$ACTIVE want=$WIN)."
  echo "       GTK4 ignores synthetic keys, so nothing below could be trusted."
  kill -9 "$APP"; exit 1
fi

echo "=== number keys rate the cued track ==="
check "nothing rated to begin with" 0 "$(stars_for sweep.mp3)"
for k in 4 2 5; do
  xdotool key --clearmodifiers "$k"; sleep 1
  check "pressing '$k'" "$k" "$(stars_for sweep.mp3)"
done
xdotool key --clearmodifiers 0; sleep 1
check "pressing '0' clears it" 0 "$(stars_for sweep.mp3)"
check "and removes the entry rather than storing a zero" 0 "$(rated_count)"

echo
echo "=== the right-click menu rates THAT row, not the cued track ==="
eval "$(xdotool getwindowgeometry --shell "$WIN")"
# Row 1 of the list. The cued track is sweep.mp3, which is row 6 — so a rating
# landing anywhere in the list proves the menu addressed the row under the
# pointer and not whatever the star strip is pointing at.
xdotool mousemove --sync $((X + 300)) $((Y + 160)); sleep 0.4
xdotool click 3; sleep 1.5
# The popover opens with its first item already focused, so one Down lands on
# the SECOND item. Items run 5,4,3,2,1 then "Clear rating", so this is 4 stars.
xdotool key Down; sleep 0.4
xdotool key Return; sleep 1.5

check "one track was rated by the menu" 1 "$(rated_count)"
check "and it was not the cued track"    0 "$(stars_for sweep.mp3)"
MENU_STARS=$(RJ="$RJ" python3 -c '
import json, os
try:
    d = json.load(open(os.environ["RJ"]))["stars"]
    print(next(iter(d.values())) if d else 0)
except Exception:
    print(0)')
check "the menu item chosen applied its rating" 4 "$MENU_STARS"

echo
echo "=== Share puts the file AND the text on the clipboard ==="
if ! command -v xclip >/dev/null; then
  echo "  [FAIL] xclip is not installed, so the clipboard cannot be checked"
  FAIL=$((FAIL+1))
else
  # Driven through the window action, which is exactly what the menu item
  # activates. The menu itself is a GtkPopoverMenu; its items are these actions.
  gdbus call --session --dest com.procomputation.Gapless \
    --object-path /com/procomputation/Gapless/window/1 \
    --method org.gtk.Actions.Activate share "[<'details'>]" "{}" >/dev/null 2>&1
  sleep 1
  TEXT=$(xclip -selection clipboard -o 2>/dev/null)
  case "$TEXT" in
    *Title*File*) echo "  [PASS] 'Copy details' put the metadata on the clipboard"; PASS=$((PASS+1)) ;;
    *) echo "  [FAIL] 'Copy details' clipboard was: ${TEXT:0:60}"; FAIL=$((FAIL+1)) ;;
  esac

  gdbus call --session --dest com.procomputation.Gapless \
    --object-path /com/procomputation/Gapless/window/1 \
    --method org.gtk.Actions.Activate share "[<'copy'>]" "{}" >/dev/null 2>&1
  sleep 1
  # The whole point of the union provider: a file manager or a chat window asks
  # for uri-list, a text field asks for plain text, from the SAME copy.
  URIS=$(xclip -selection clipboard -t text/uri-list -o 2>/dev/null)
  PLAIN=$(xclip -selection clipboard -t UTF8_STRING -o 2>/dev/null)
  case "$URIS" in
    file://*sweep.mp3*) echo "  [PASS] 'Copy file' offers the audio file as text/uri-list"; PASS=$((PASS+1)) ;;
    *) echo "  [FAIL] uri-list was: ${URIS:0:70}"; FAIL=$((FAIL+1)) ;;
  esac
  case "$PLAIN" in
    *Title*) echo "  [PASS] the same copy also offers the metadata as text"; PASS=$((PASS+1)) ;;
    *) echo "  [FAIL] text/plain was: ${PLAIN:0:70}"; FAIL=$((FAIL+1)) ;;
  esac
  # A paste must COPY the user's music, never move it.
  GNOME=$(xclip -selection clipboard -t x-special/gnome-copied-files -o 2>/dev/null)
  case "$GNOME" in
    copy$'\n'file://*) echo "  [PASS] the file-manager payload says copy, not cut"; PASS=$((PASS+1)) ;;
    *) echo "  [FAIL] gnome-copied-files was: ${GNOME:0:40}"; FAIL=$((FAIL+1)) ;;
  esac
fi

kill -9 "$APP" 2>/dev/null
wait "$APP" 2>/dev/null

echo
if [ "$FAIL" = 0 ]; then
  echo "[PASS] $PASS/$PASS"
else
  echo "[FAIL] $FAIL of $((PASS+FAIL)) checks failed"
  exit 1
fi
