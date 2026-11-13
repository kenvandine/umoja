#!/bin/bash
# Headless test: windows leave a monitor that goes away and come back to it,
# as in GNOME (mutter's meta_window_update_for_monitors_changed).
#
# umoja-mir runs on two virtual outputs (Mir 2.28 wants --virtual-output
# once per output; a colon list gives one output with two modes). Windows are moved to the second
# one (one floating, one maximized, one tiled), then the second output is
# switched off through org.gnome.Mutter.DisplayConfig (as Settings would,
# and as unplugging it looks to the window manager), and on again.
#
#   tests/monitor-unplug-test.sh OUTDIR [path/to/umoja-mir]
set -uo pipefail
OUT=${1:?output directory}
MIR_BIN=${2:-umoja-mir}
mkdir -p "$OUT"
LOG="$OUT/test.log"
: >"$LOG"
FAILED=0
fail() { echo "FAIL: $*" | tee -a "$LOG"; FAILED=1; }
pass() { echo "PASS: $*" | tee -a "$LOG"; }

export XDG_RUNTIME_DIR=$(mktemp -d /tmp/umoja-runtime.XXXXXX)
export XDG_CONFIG_HOME=$(mktemp -d /tmp/umoja-config.XXXXXX)
export XDG_CURRENT_DESKTOP=Umoja:ubuntu:GNOME XDG_SESSION_TYPE=wayland XDG_SESSION_DESKTOP=Umoja
export NO_AT_BRIDGE=1 RUST_LOG=info
export UMOJA_NO_SESSION_TARGET=1 UMOJA_NO_XWAYLAND=1 UMOJA_TRACK_MODIFIERS=1 UMOJA_IDLE_BUS=1
unset DISPLAY WAYLAND_DISPLAY
eval "$(dbus-launch --sh-syntax)"
touch "$XDG_RUNTIME_DIR/umoja-debug-windows"

cleanup() {
    kill $(jobs -p) 2>/dev/null
    # gvfsd (from nautilus) starts a wsdd that outlives the test bus.
    pkill -f "wsdd.*$XDG_RUNTIME_DIR" 2>/dev/null
    kill "$DBUS_SESSION_BUS_PID" 2>/dev/null
}
trap cleanup EXIT

"$MIR_BIN" --platform-display-libs mir:virtual --virtual-output 1920x1080 --virtual-output 1920x1080 >"$OUT/umoja-mir.log" 2>&1 &
for _ in $(seq 1 30); do [ -S "$XDG_RUNTIME_DIR/wayland-0" ] && break; sleep 1; done
[ -S "$XDG_RUNTIME_DIR/wayland-0" ] || { fail "umoja-mir did not start (see umoja-mir.log)"; exit 1; }
export WAYLAND_DISPLAY=wayland-0
sleep 2

comp() { gdbus call --session --dest org.umoja.Compositor --object-path /org/umoja/Compositor \
    --method "org.umoja.Compositor.$1" "${@:2}" 2>>"$LOG"; }

# Prints "x y w h flags" of the window whose app id contains $1.
geometry() {
    comp GetWindows | python3 -c '
import ast, re, sys
text = sys.stdin.read().strip()
text = re.sub(r"(\b(uint64|uint32|int32)|@a\([^)]*\))\s*", "", text)
rows = ast.literal_eval(text)[0]
for row in rows:
    if sys.argv[1].lower() in (row[1] + " " + row[2]).lower():
        print(*row[3:8])
        break
' "$1"
}

# Switch the second output off (only the first listed) or both on.
monitors() { # one|two
    python3 - "$1" >>"$LOG" 2>&1 <<'EOF'
import sys
from gi.repository import Gio, GLib
bus = Gio.bus_get_sync(Gio.BusType.SESSION)
def call(method, args, reply):
    return bus.call_sync("org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig",
                         "org.gnome.Mutter.DisplayConfig", method, args, GLib.VariantType(reply),
                         Gio.DBusCallFlags.NONE, -1, None)
serial, monitors, logical, props = call("GetCurrentState", None, "(ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})").unpack()
print("state:", [m[0][0] for m in monitors], [(l[0], l[1], [c[0] for c in l[5]]) for l in logical])
def mode(m):
    for mode in m[1]:
        if mode[6].get("is-current") or mode[6].get("is-preferred"):
            return mode[0]
    return m[1][0][0]
mons = sorted(monitors, key=lambda m: m[0][0])
want = mons[:1] if sys.argv[1] == "one" else mons[:2]
cfg, x = [], 0
for i, m in enumerate(want):
    cfg.append((x, 0, 1.0, 0, i == 0, [(m[0][0], mode(m), {})]))
    x += 1920
call("ApplyMonitorsConfig", GLib.Variant("(uua(iiduba(ssa{sv}))a{sv})", (serial, 1, cfg, {})), "()")
print("applied", sys.argv[1])
EOF
}

launch() { # app-id-substring command...
    "${@:2}" >"$OUT/$1.log" 2>&1 &
    for _ in $(seq 1 20); do [ -n "$(geometry "$1")" ] && return 0; sleep 0.5; done
    fail "$1 did not open"
    return 1
}
key() { wtype "$@" 2>>"$LOG"; sleep 1; }

on_second() { read -r x _ <<<"$1"; [ "${x:-0}" -ge 1920 ]; }
on_first() { read -r x _ w _ <<<"$1"; [ -n "$x" ] && [ "$x" -ge 0 ] && [ $((x + w)) -le 1920 ]; }

# Floating: gnome-text-editor, moved over with Super+Shift+Right.
launch TextEditor gnome-text-editor --new-window || exit 1
key -P Super_L -P Shift_L -k Right -p Shift_L -p Super_L
# Maximized: gnome-calculator, moved over then maximized.
launch Calculator gnome-calculator || exit 1
key -P Super_L -P Shift_L -k Right -p Shift_L -p Super_L
key -P Super_L -k Up -p Super_L
# Tiled: nautilus, moved over then tiled into the right half.
launch Nautilus nautilus --new-window || exit 1
key -P Super_L -P Shift_L -k Right -p Shift_L -p Super_L
key -P Super_L -k Right -p Super_L

editor0=$(geometry TextEditor) calc0=$(geometry Calculator) files0=$(geometry Nautilus)
echo "before: editor=$editor0 calc=$calc0 files=$files0" >>"$LOG"
on_second "$editor0" && on_second "$calc0" && on_second "$files0" \
    && pass "three windows on the second monitor" || fail "setup: editor=$editor0 calc=$calc0 files=$files0"

monitors one
sleep 3
editor1=$(geometry TextEditor) calc1=$(geometry Calculator) files1=$(geometry Nautilus)
echo "one monitor: editor=$editor1 calc=$calc1 files=$files1" >>"$LOG"
on_first "$editor1" && pass "floating window moved to the remaining monitor" || fail "editor at $editor1"
on_first "$calc1" && pass "maximized window moved to the remaining monitor" || fail "calculator at $calc1"
read -r _ _ cw ch _ <<<"$calc1"
[ "${cw:-0}" -gt 1500 ] && pass "maximized window fills the remaining monitor ($cw x $ch)" || fail "calculator size $cw x $ch"
on_first "$files1" && pass "tiled window moved to the remaining monitor" || fail "nautilus at $files1"
read -r fx _ fw _ <<<"$files1"
[ "${fx:-0}" -ge 900 ] && [ "${fw:-0}" -le 1000 ] && pass "tiled window is in the right half there" || fail "nautilus at $files1"

monitors two
sleep 3
editor2=$(geometry TextEditor) calc2=$(geometry Calculator) files2=$(geometry Nautilus)
echo "two monitors again: editor=$editor2 calc=$calc2 files=$files2" >>"$LOG"
[ "$(cut -d' ' -f1-4 <<<"$editor2")" = "$(cut -d' ' -f1-4 <<<"$editor0")" ] \
    && pass "floating window back exactly where it was" || fail "editor back at $editor2, was $editor0"
[ "$calc2" = "$calc0" ] && pass "maximized window back, maximized" || fail "calculator back at $calc2, was $calc0"
[ "$files2" = "$files0" ] && pass "tiled window back in its tile" || fail "nautilus back at $files2, was $files0"

# The screen blanking turns every output off: nothing may move.
comp ActivateScreensaver >/dev/null
sleep 3
key -k Shift_L
sleep 3
editor3=$(geometry TextEditor) calc3=$(geometry Calculator) files3=$(geometry Nautilus)
echo "after blank: editor=$editor3 calc=$calc3 files=$files3" >>"$LOG"
[ "$editor3 $calc3 $files3" = "$editor2 $calc2 $files2" ] \
    && pass "screen blank and wake leave windows where they were" || fail "after blank: editor=$editor3 calc=$calc3 files=$files3"

grep "monitors changed\|monitor change moves" "$OUT/umoja-mir.log" >>"$LOG"
exit $FAILED
