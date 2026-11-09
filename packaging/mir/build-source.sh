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
echo "$PWD"
