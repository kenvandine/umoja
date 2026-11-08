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
# wtype's modifiers aren't reported on Mir key events; let umoja-mir track them.
export UMOJA_TRACK_MODIFIERS=1
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

# Window management, as in GNOME on Ubuntu.
comp() { gdbus call --session --dest org.umoja.Compositor --object-path /org/umoja/Compositor \
    --method "org.umoja.Compositor.$1" "${@:2}" 2>>"$LOG"; }
windows=$(comp GetWindows)
echo "GetWindows: $windows" >>"$LOG"
[[ "$windows" == *TextEditor* && "$windows" == *Nautilus* ]] \
    && pass "compositor lists both windows" || fail "compositor window list: $windows"

# Super+Left tiles the focused window into the left half (tiled flag 32).
wtype -P Super_L -k Left -p Super_L 2>>"$LOG"
sleep 2
shot 06-tiled-left
comp GetWindowsMru | grep -Eq "^\(\[\([^)]*uint32 ([0-9]+)" && {
    flags=$(comp GetWindowsMru | sed -E 's/^\(\[\([^)]*uint32 ([0-9]+).*/\1/')
    (( flags & 32 )) && pass "Super+Left tiled the focused window" || fail "Super+Left did not tile (flags $flags)"
} || fail "no focused window after Super+Left"

# Alt+Tab: hold Alt, press Tab, capture while the switcher is up, release.
before=$(comp GetWindowsMru | grep -oE "uint64 [0-9]+" | head -1)
wtype -P Alt_L -k Tab -s 2500 -p Alt_L 2>>"$LOG" &
sleep 1.5
shot 07-alt-tab
wait
sleep 1
after=$(comp GetWindowsMru | grep -oE "uint64 [0-9]+" | head -1)
[ -n "$before" ] && [ "$before" != "$after" ] && pass "Alt+Tab switched windows" \
    || fail "Alt+Tab did not switch ($before -> $after)"

# Super opens the overview's window spread.
overlay ToggleOverview && pass "overlay answers ToggleOverview" || fail "ToggleOverview failed"
sleep 8
shot 08-overview
overlay ToggleOverview
sleep 2

# Workspaces, as in GNOME: dynamic by default (always one empty at the end).
ws() { comp GetWorkspaces | grep -oE "uint32 [0-9]+" | awk '{print $2}' | tr '\n' ' '; }
[ "$(ws)" = "0 2 " ] && pass "dynamic workspaces: one in use plus a spare" || fail "GetWorkspaces: $(ws)"
first=$(comp GetWindows | grep -oE "uint64 [0-9]+" | head -1 | awk '{print $2}')
comp SendWindowToWorkspace "$first" 1 >/dev/null
sleep 1
echo "GetWindowWorkspaces: $(comp GetWindowWorkspaces)" >>"$LOG"
comp GetWindowWorkspaces | grep -q "uint64 $first, 1)" && pass "window sent to workspace 2" \
    || fail "SendWindowToWorkspace did not move window $first"
[ "$(ws)" = "0 3 " ] && pass "a new spare workspace appeared" || fail "after moving: $(ws)"
overlay ToggleOverview
sleep 8
shot 09-overview-workspaces
overlay ToggleOverview
sleep 2
# Super+End: switch-to-workspace-last; Super+Home: back to the first.
wtype -P Super_L -k End -p Super_L 2>>"$LOG"
sleep 1
[ "$(ws)" = "2 3 " ] && pass "Super+End switched to the last workspace" || fail "after Super+End: $(ws)"
wtype -P Super_L -k Home -p Super_L 2>>"$LOG"
sleep 1
[ "$(ws)" = "0 3 " ] && pass "Super+Home switched to the first workspace" || fail "after Super+Home: $(ws)"
# Fixed workspaces apply at once.
gsettings set org.gnome.mutter dynamic-workspaces false
gsettings set org.gnome.desktop.wm.preferences num-workspaces 4
sleep 2
[ "$(ws)" = "0 4 " ] && pass "fixed num-workspaces 4 applied" || fail "fixed workspaces: $(ws)"
gsettings set org.gnome.desktop.wm.preferences num-workspaces 1
sleep 2
[ "$(ws)" = "0 1 " ] && pass "shrinking to one workspace folds the rest" || fail "fold: $(ws)"
gsettings reset org.gnome.desktop.wm.preferences num-workspaces
gsettings reset org.gnome.mutter dynamic-workspaces
sleep 2

# Drag-to-snap chrome: the compositor drives these while a window is dragged.
# (Mir's virtual output sends no frame callbacks, so only check the calls.)
ov() { gdbus call --session --dest org.umoja.Overlay --object-path /org/umoja/Overlay \
    --method "org.umoja.Overlay.$1" "${@:2}" >>"$LOG" 2>&1; }
ov TilePreview true 0 32 960 1048 600 300 800 600 && ov LayoutPicker 2 1 774 32 \
    && ov TilePreview false 0 0 0 0 0 0 0 0 && ov LayoutPicker 0 0 0 0 \
    && pass "overlay accepts tile preview and layout picker" || fail "tile preview / layout picker calls failed"

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
