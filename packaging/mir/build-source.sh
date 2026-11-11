#!/bin/sh
# Fetch the archive's Mir source and apply Umoja's patches (see README.md).
# Usage: build-source.sh <workdir> [series]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=$1
series=${2:-stonking}
upstream=$(cat "$here/UPSTREAM")
suffix=$(cat "$here/SUFFIX")

mkdir -p "$work"
cd "$work"
dget -u "https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/mir/$upstream/mir_$upstream.dsc"
src=$(find . -maxdepth 1 -type d -name 'mir-*' | head -n1)
cd "$src"
for p in "$here"/patches/*.patch; do
    cp "$p" debian/patches/
    basename "$p" >> debian/patches/series
done
# Check they apply now rather than in the middle of a build.
QUILT_PATCHES=debian/patches quilt push -a
QUILT_PATCHES=debian/patches quilt pop -a
dch --newversion "$upstream$suffix" --distribution "$series" --force-distribution \
    "Pass libinput touchpad gestures to the shell (mir_touchpad_gesture_hook), for Umoja's GNOME-style workspace swipes."
dch --append "Bridge the PRIMARY selection between Xwayland and Wayland apps, for middle-click paste."
dch --append "Keep a layer surface's margin when a popup is repositioned, so tooltips stop covering their widget."
dch --append "Blur the background behind surfaces that ask for it (ext-background-effect-v1), for Umoja's frosted panel, dock, menus and notifications."
dch --append "Drop a surface's pending explicit-sync commit when the surface is destroyed (use-after-free that hung the Wayland thread)."
dch --append "Take control of our own logind session (XDG_SESSION_ID), so a compositor restarted while another VT is active resumes when switched back."
dch --append "Honour wl_surface.offset and buffer scale for client cursor hotspots, and keep a hidden cursor surface's role, so GTK terminal cursors line up and come back after hiding."
dch --append "Confine the pointer when it is pushed just past the top or left screen edge, so clicks in the top bar's corners reach it."
dch --append "wlr-screencopy: read a capture_output_region region as output-local, as the protocol says, so recording part of a monitor not at the layout origin is not cropped."
dch --append "Reuse the cross-GPU CPU copy buffers instead of allocating one per frame, and log the copy volume in Umoja's window debug mode."
dch --append "Screencopy: keep a renderer per capture size, so capturing monitors of different sizes in turn does not rebuild the GL renderer every time."
dch --append "xdg-activation: give a token to a client the user is interacting with even without keyboard focus (the top bar), stop revoking tokens on key presses and focus changes, and let them live 30 s, so clicked notifications and links bring their window forward."
echo "$PWD"
