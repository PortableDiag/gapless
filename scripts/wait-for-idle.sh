#!/usr/bin/env bash
# Block until the person at the keyboard has stopped using it.
#
# Some checks in this project have to take the keyboard and the focus —
# `verify-input.sh` drives the real window with real XTEST input, because GTK4
# ignores synthetic key events. Running that while somebody is typing means two
# things fight over the same input: their keystrokes land in the test's window,
# the test's keystrokes land in their editor, and both lose.
#
# So: wait for them to be idle first, and get out of the way the moment they come
# back.
#
#   ./scripts/wait-for-idle.sh [idle_secs] [max_wait_secs]
#       idle_secs      how long they must have been idle   (default 12)
#       max_wait_secs  give up and fail after this          (default 600)
#
# Exit 0 once idle, 1 if it never went quiet, 2 if idle time cannot be read.
#
# Also usable as a tripwire mid-run — see `idle_ms` below, which is what
# `verify-input.sh` polls between keystrokes so it can abandon the run the
# instant a real keypress arrives, rather than wrestling for the focus.
set -uo pipefail

IDLE_SECS="${1:-12}"
MAX_WAIT="${2:-600}"

# `xprintidle` when it is installed; the XScreenSaver extension directly when it
# is not, which is the same source of truth without the dependency.
idle_ms() {
  if command -v xprintidle >/dev/null 2>&1; then
    xprintidle 2>/dev/null && return 0
  fi
  python3 - <<'PY' 2>/dev/null
from ctypes import cdll, Structure, c_int, c_ulong, POINTER
class XSS(Structure):
    _fields_ = [("window", c_ulong), ("state", c_int), ("kind", c_int),
                ("til_or_since", c_ulong), ("idle", c_ulong), ("event_mask", c_ulong)]
x11 = cdll.LoadLibrary("libX11.so.6")
xss = cdll.LoadLibrary("libXss.so.1")
d = x11.XOpenDisplay(None)
xss.XScreenSaverAllocInfo.restype = POINTER(XSS)
info = xss.XScreenSaverAllocInfo()
xss.XScreenSaverQueryInfo(d, x11.XDefaultRootWindow(d), info)
print(info.contents.idle)
PY
}

# Exported for callers that want the tripwire rather than the gate.
if [ "${1:-}" = "--idle-ms" ]; then
  ms=$(idle_ms)
  [ -n "$ms" ] || exit 2
  echo "$ms"
  exit 0
fi

first=$(idle_ms)
if [ -z "$first" ]; then
  echo "cannot read the X idle time — install xprintidle, or run this where an X display is reachable"
  exit 2
fi

want_ms=$((IDLE_SECS * 1000))
waited=0
announced=no
while :; do
  ms=$(idle_ms)
  [ -n "$ms" ] || { echo "lost the X idle time mid-wait"; exit 2; }
  if [ "$ms" -ge "$want_ms" ]; then
    [ "$announced" = yes ] && echo "  keyboard has been quiet for $((ms / 1000))s — going ahead"
    exit 0
  fi
  if [ "$announced" = no ]; then
    echo "  waiting for the keyboard to go quiet (${IDLE_SECS}s idle needed; you are using it now)…"
    announced=yes
  fi
  sleep 2
  waited=$((waited + 2))
  if [ "$waited" -ge "$MAX_WAIT" ]; then
    echo "  still in use after ${MAX_WAIT}s — not taking the keyboard out from under you"
    exit 1
  fi
done
