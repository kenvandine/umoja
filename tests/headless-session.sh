#!/bin/bash
# Headless Umoja session for tests/install-test.sh; runs as a normal user.
# umoja-mir draws to Mir's virtual output (software rendering), and every
# screenshot goes through grim and zwlr_screencopy, like the real portal.
set -uo pipefail
OUT=${1:?output directory}
LOG="$OUT/session.log"
: >"$LOG"
FAILED=0
fail() { echo "FAIL: $*" | tee -a "$LOG"; FAILED=1; }
pass() { echo "PASS: $*" | tee -a "$LOG"; }

export XDG_RUNTIME_DIR=$(mktemp -d /tmp/umoja-runtime.XXXXXX)
export XDG_CURRENT_DESKTOP=Umoja:ubuntu:GNOME XDG_SESSION_TYPE=wayland XDG_SESSION_DESKTOP=Umoja
export NO_AT_BRIDGE=1 RUST_LOG=info
# No systemd --user in a container: start the shell pieces by hand below.
export UMOJA_NO_SESSION_TARGET=1 UMOJA_NO_XWAYLAND=1
unset DISPLAY WAYLAND_DISPLAY
cd "$HOME"
eval "$(dbus-launch --sh-syntax)"

# Ubuntu's desktop defaults must apply, as they do in the Ubuntu session.
expect() { # schema key value
    local v; v=$(gsettings get "$1" "$2" 2>&1)
    [ "$v" = "$3" ] && pass "$1 $2 is $3" || fail "$1 $2 is $v, expected $3"
}
expect org.gnome.shell.extensions.dash-to-dock dock-position "'LEFT'"
expect org.gnome.shell.extensions.dash-to-dock extend-height true
expect org.gnome.desktop.interface gtk-theme "'Yaru'"
expect org.gnome.desktop.interface icon-theme "'Yaru'"
expect org.gnome.desktop.interface font-name "'Ubuntu Sans 11'"

overlay() { gdbus call --session --dest org.umoja.Overlay --object-path /org/umoja/Overlay \
    --method "org.umoja.Overlay.$1" >>"$LOG" 2>&1; }
shot() { grim "$OUT/$1.png" 2>>"$LOG" && echo "captured $1.png" >>"$LOG"; }

umoja-mir --platform-display-libs mir:virtual --virtual-output 1920x1080 >"$OUT/umoja-mir.log" 2>&1 &
MIR=$!
for _ in $(seq 1 30); do [ -S "$XDG_RUNTIME_DIR/wayland-0" ] && break; sleep 1; done
if [ ! -S "$XDG_RUNTIME_DIR/wayland-0" ]; then
    fail "umoja-mir did not start (see umoja-mir.log)"
    exit 1
fi
pass "umoja-mir started"
export WAYLAND_DISPLAY=wayland-0

for c in panel dock overlay; do umoja-$c >"$OUT/umoja-$c.log" 2>&1 & done
/usr/libexec/xdg-desktop-portal-umoja >"$OUT/portal.log" 2>&1 &
sleep 8
shot 01-desktop && pass "grim can capture via wlr-screencopy" || fail "grim could not capture"

gnome-text-editor >"$OUT/gnome-text-editor.log" 2>&1 &
sleep 4
nautilus --new-window >"$OUT/nautilus.log" 2>&1 &
sleep 8
shot 02-apps

# The app grid is built on first show, which is slow with software GL.
overlay Show && pass "overlay answers Show" || fail "overlay Show failed"
sleep 10
shot 03-overlay
overlay Hide
sleep 2

overlay ShowScreenshotUI && pass "screenshot UI opens" || fail "screenshot UI failed"
sleep 5
shot 04-screenshot-ui

for p in mir panel dock overlay; do
    pgrep -u "$(id -u)" -x "umoja-$p" >/dev/null && pass "umoja-$p still running" \
        || fail "umoja-$p exited (see umoja-$p.log)"
done
pgrep -u "$(id -u)" -f 'gnome-text-editor' >/dev/null && pass "Text Editor running" || fail "Text Editor exited"
pgrep -u "$(id -u)" -x nautilus >/dev/null && pass "Files running" || fail "Files exited"

pkill -u "$(id -u)" -f 'gnome-text-editor|nautilus|umoja-(panel|dock|overlay)|xdg-desktop-portal-umoja'
kill $MIR $DBUS_SESSION_BUS_PID 2>/dev/null
exit $FAILED
