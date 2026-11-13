#!/bin/bash
# Screen sharing, end to end on the compositor side, in a headless Umoja:
# the window list the portal's picker shows (org.gnome.Shell.Introspect),
# monitor and window streams (org.gnome.Mutter.ScreenCast) read back from
# PipeWire, remote desktop input (org.gnome.Mutter.RemoteDesktop) and input
# capture zones (org.gnome.Mutter.InputCapture).
#
#   tests/screencast-test.sh OUTDIR [path/to/umoja-mir]
#
# Needs pipewire, wireplumber, gst-launch-1.0 with pipewiresrc
# (gstreamer1.0-pipewire), python3-gi and gnome-text-editor. Runs as a
# normal user with its own D-Bus and PipeWire; touches nothing of the
# running session.
set -uo pipefail
OUT=${1:?output directory}
MIR=${2:-umoja-mir}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
LOG="$OUT/screencast.log"
: >"$LOG"
FAILED=0
fail() { echo "FAIL: $*" | tee -a "$LOG"; FAILED=1; }
pass() { echo "PASS: $*" | tee -a "$LOG"; }

export XDG_RUNTIME_DIR=$(mktemp -d /tmp/umoja-cast-runtime.XXXXXX)
export XDG_CURRENT_DESKTOP=Umoja:ubuntu:GNOME XDG_SESSION_TYPE=wayland
export UMOJA_NO_SESSION_TARGET=1 UMOJA_NO_XWAYLAND=1 UMOJA_IDLE_BUS=1
# Lets this test list windows without being the portal (gnome-shell's
# unsafe mode).
export UMOJA_UNSAFE_MODE=1
export NO_AT_BRIDGE=1 RUST_LOG=info
unset DISPLAY WAYLAND_DISPLAY
eval "$(dbus-launch --sh-syntax)"
cleanup() {
    kill $(jobs -p) 2>/dev/null
    kill "$DBUS_SESSION_BUS_PID" 2>/dev/null
    rm -rf "$XDG_RUNTIME_DIR"
}
trap cleanup EXIT

pipewire >"$OUT/pipewire.log" 2>&1 &
sleep 1
wireplumber >"$OUT/wireplumber.log" 2>&1 &
sleep 1

"$MIR" --platform-display-libs mir:virtual --virtual-output 1280x720 >"$OUT/umoja-mir.log" 2>&1 &
for _ in $(seq 1 30); do [ -S "$XDG_RUNTIME_DIR/wayland-0" ] && break; sleep 1; done
[ -S "$XDG_RUNTIME_DIR/wayland-0" ] || { fail "umoja-mir did not start"; exit 1; }
export WAYLAND_DISPLAY=wayland-0
for _ in $(seq 1 20); do
    gdbus introspect --session --dest org.gnome.Mutter.ScreenCast --object-path /org/gnome/Mutter/ScreenCast \
        >/dev/null 2>&1 && break
    sleep 0.5
done
pass "umoja-mir started"

gnome-text-editor --new-window >"$OUT/gnome-text-editor.log" 2>&1 &
sleep 6

python3 - "$OUT" <<'EOF' 2>&1 | tee -a "$LOG"
import os, subprocess, sys, time
from gi.repository import Gio, GLib

out = sys.argv[1]
bus = Gio.bus_get_sync(Gio.BusType.SESSION)
failed = False

def ok(cond, what):
    global failed
    print(("PASS: " if cond else "FAIL: ") + what)
    failed |= not cond

def call(name, path, iface, method, args=None, reply=None):
    return bus.call_sync(name, path, iface, method, args, GLib.VariantType(reply) if reply else None,
                         Gio.DBusCallFlags.NONE, 10000, None).unpack()

def prop(name, path, iface, key):
    return call(name, path, "org.freedesktop.DBus.Properties", "Get", GLib.Variant("(ss)", (iface, key)))[0]

def wait_node(stream_path, start):
    """Run start(), return the node id from PipeWireStreamAdded(V2)."""
    got = {}
    loop = GLib.MainLoop()
    def on_signal(_c, _s, _p, _i, signal, params):
        if signal == "PipeWireStreamAddedV2":
            got["v2"] = params.unpack()[0]
        elif signal == "PipeWireStreamAdded":
            got["node"] = params.unpack()[0]
        if "node" in got and "v2" in got:
            loop.quit()
    sub = bus.signal_subscribe("org.gnome.Mutter.ScreenCast", "org.gnome.Mutter.ScreenCast.Stream", None,
                               stream_path, None, Gio.DBusSignalFlags.NONE, on_signal)
    start()
    GLib.timeout_add(8000, loop.quit)
    loop.run()
    bus.signal_unsubscribe(sub)
    return got

def grab(node, png):
    """Two frames from the node: the last one as a PNG."""
    r = subprocess.run(["gst-launch-1.0", "-q", "pipewiresrc", f"path={node}", "num-buffers=2", "do-timestamp=true",
                        "!", "videoconvert", "!", "pngenc", "snapshot=false", "!", "multifilesink",
                        f"location={png}"], timeout=20, capture_output=True, text=True)
    if r.returncode != 0:
        print(r.stderr[-2000:])
    return r.returncode == 0 and os.path.exists(png) and os.path.getsize(png) > 1000

def png_size(png):
    with open(png, "rb") as f:
        head = f.read(24)
    return int.from_bytes(head[16:20], "big"), int.from_bytes(head[20:24], "big")

# --- The window list -----------------------------------------------------
INTRO = ("org.gnome.Shell.Introspect", "/org/gnome/Shell/Introspect", "org.gnome.Shell.Introspect")
windows = call(*INTRO, "GetWindows")[0]
print("GetWindows:", windows)
editor = [(wid, p) for wid, p in windows.items() if "TextEditor" in p.get("app-id", "")]
ok(bool(editor), "Introspect lists the text editor window with its desktop id")
apps = call(*INTRO, "GetRunningApplications")[0]
ok(any("TextEditor" in a for a in apps), "Introspect lists running apps")
ok(tuple(prop(*INTRO, "ScreenSize")) == (1280, 720), "Introspect screen size")

# --- A monitor stream ------------------------------------------------------
SC = "org.gnome.Mutter.ScreenCast"
session = call(SC, "/org/gnome/Mutter/ScreenCast", SC, "CreateSession", GLib.Variant("(a{sv})", ({},)))[0]
connector = call("org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig",
                 "org.gnome.Mutter.DisplayConfig", "GetCurrentState")[1][0][0][0]
stream = call(SC, session, SC + ".Session", "RecordMonitor",
              GLib.Variant("(sa{sv})", (connector, {"cursor-mode": GLib.Variant("u", 1)})))[0]
params = prop(SC, stream, SC + ".Stream", "Parameters")
ok(tuple(params.get("size", ())) == (1280, 720), f"monitor stream parameters {params}")
got = wait_node(stream, lambda: call(SC, session, SC + ".Session", "Start"))
ok("node" in got and got.get("v2", {}).get("node-id") == got.get("node"),
   f"monitor stream announced its PipeWire node {got}")
if "node" in got:
    png = os.path.join(out, "cast-monitor.png")
    ok(grab(got["node"], png) and png_size(png) == (1280, 720), "monitor stream frames read back from PipeWire")
sharing = call("org.umoja.Compositor", "/org/umoja/Compositor", "org.umoja.Compositor", "IsScreenSharing")[0]
ok(sharing, "the top bar is told the screen is shared")
call(SC, session, SC + ".Session", "Stop")
time.sleep(1)
sharing = call("org.umoja.Compositor", "/org/umoja/Compositor", "org.umoja.Compositor", "IsScreenSharing")[0]
ok(not sharing, "sharing ends with the session")

# --- A window stream, also while the window is minimized -------------------
if editor:
    wid = editor[0][0]
    session = call(SC, "/org/gnome/Mutter/ScreenCast", SC, "CreateSession", GLib.Variant("(a{sv})", ({},)))[0]
    stream = call(SC, session, SC + ".Session", "RecordWindow",
                  GLib.Variant("(a{sv})", ({"window-id": GLib.Variant("t", wid)},)))[0]
    w, h = prop(SC, stream, SC + ".Stream", "Parameters")["size"]
    got = wait_node(stream, lambda: call(SC, session, SC + ".Session", "Start"))
    ok("node" in got, "window stream announced its PipeWire node")
    if "node" in got:
        png = os.path.join(out, "cast-window.png")
        ok(grab(got["node"], png) and png_size(png) == (w, h), f"window stream frames are the window's size {w}x{h}")
    call(SC, session, SC + ".Session", "Stop")

# --- Remote desktop: absolute pointer through the stream, keys -------------
RD = "org.gnome.Mutter.RemoteDesktop"
rd = call(RD, "/org/gnome/Mutter/RemoteDesktop", RD, "CreateSession")[0]
rd_id = prop(RD, rd, RD + ".Session", "SessionId")
cast = call(SC, "/org/gnome/Mutter/ScreenCast", SC, "CreateSession",
            GLib.Variant("(a{sv})", ({"remote-desktop-session-id": GLib.Variant("s", rd_id)},)))[0]
stream = call(SC, cast, SC + ".Session", "RecordMonitor", GLib.Variant("(sa{sv})", (connector, {})))[0]
got = wait_node(stream, lambda: call(RD, rd, RD + ".Session", "Start"))
ok("node" in got, "remote desktop session starts its screen cast")
call(RD, rd, RD + ".Session", "NotifyPointerMotionAbsolute", GLib.Variant("(sdd)", (stream, 640.0, 360.0)))
call(RD, rd, RD + ".Session", "NotifyPointerButton", GLib.Variant("(ib)", (0x110, True)))
call(RD, rd, RD + ".Session", "NotifyPointerButton", GLib.Variant("(ib)", (0x110, False)))
for ch in "Umoja":
    for down in (True, False):
        call(RD, rd, RD + ".Session", "NotifyKeyboardKeysym", GLib.Variant("(ub)", (ord(ch), down)))
time.sleep(1)
subprocess.run(["grim", os.path.join(out, "remote-typed.png")])
print("remote typing screenshot: remote-typed.png (should show 'Umoja' in the editor)")
try:
    fd_reply = bus.call_with_unix_fd_list_sync(RD, rd, RD + ".Session", "ConnectToEIS",
                                               GLib.Variant("(a{sv})", ({},)), GLib.VariantType("(h)"),
                                               Gio.DBusCallFlags.NONE, 5000, None, None)
    ok(fd_reply[1] is not None and fd_reply[1].get_length() == 1, "ConnectToEIS hands out a libei socket")
except GLib.Error as e:
    ok(False, f"ConnectToEIS: {e.message}")
call(RD, rd, RD + ".Session", "Stop")

# --- Input capture zones and barriers ----------------------------------------
IC = "org.gnome.Mutter.InputCapture"
ic = call(IC, "/org/gnome/Mutter/InputCapture", IC, "CreateSession", GLib.Variant("(u)", (3,)))[0]
serial, zones = call(IC, ic, IC + ".Session", "GetZones")
ok(list(map(tuple, zones)) == [(1280, 720, 0, 0)], f"input capture zones {zones}")
barrier = call(IC, ic, IC + ".Session", "AddBarrier", GLib.Variant("(u(iiii))", (serial, (1280, 0, 1280, 719))))[0]
ok(barrier > 0, "a barrier on the right edge is accepted")
try:
    call(IC, ic, IC + ".Session", "AddBarrier", GLib.Variant("(u(iiii))", (serial, (600, 0, 600, 719))))
    ok(False, "a barrier in the middle of the screen is refused")
except GLib.Error:
    ok(True, "a barrier in the middle of the screen is refused")
call(IC, ic, IC + ".Session", "Close")

sys.exit(1 if failed else 0)
EOF
[ "${PIPESTATUS[0]}" -eq 0 ] || FAILED=1
grep -iE "screen cast|remote desktop|input capture|PipeWire" "$OUT/umoja-mir.log" | tail -20 >>"$LOG"
[ $FAILED -eq 0 ] && echo "screen sharing: all passed" || echo "screen sharing: FAILURES (see $LOG)"
exit $FAILED
