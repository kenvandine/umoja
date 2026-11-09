# Patched Mir for ppa:ken-vandine/umoja

We carry small patches on top of the archive's Mir and publish it to the
Umoja PPA:

- `patches/3001_touchpad-gesture-hook.patch`: the evdev input platform looks
  up `mir_touchpad_gesture_hook` in the server executable (`dlsym`) and calls
  it for each swipe, pinch and hold event. No Mir ABI changes; a server
  without the hook behaves as before. umoja-mir defines and exports it
  (`crates/umoja-mir/src/ffi.rs`, `build.rs`).
- `patches/3002_xwayland-primary-selection.patch`: Mir's Xwayland support
  only syncs CLIPBOARD, so middle-click paste didn't cross between X11 and
  Wayland apps. The patch runs a second clipboard source/provider pair for
  PRIMARY against the server's primary selection clipboard (the one behind
  `zwp_primary_selection_device_manager_v1`). Internal only, no ABI change.
- `UPSTREAM`: the archive source version we patch; our version adds
  `umoja<N>` (`SUFFIX`), which sorts above it and above `ubuntu<N>` updates.

`build-source.sh <workdir>` fetches that source from Launchpad, adds the
patches and the changelog entry, and leaves `mir-<ver>/` ready for
`dpkg-buildpackage`. `.github/workflows/mir.yml` test-builds the binaries
and uploads the source package to the PPA.

When the archive moves to a new Mir, bump `UPSTREAM` (and refresh the patch).
