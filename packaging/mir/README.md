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
- `patches/3003_popup-reposition-aux-rect-offset.patch`: `xdg_popup.reposition`
  dropped the parent layer surface's margin offset (the create path keeps
  it), so a repositioned popup, such as a GTK tooltip, landed margin pixels
  too high. The quick-reply Send tooltip covered the button and ate clicks.
- `patches/3004_background-blur.patch`: implements `ext-background-effect-v1`.
  A client sets a blur region on its surface and the GL renderer blurs what
  is behind it before drawing the surface: it grabs the region's bounding
  box from the frame, runs dual-Kawase down/up passes at reduced resolution
  and draws the result back weighted by the surface's own alpha, so rounded
  corners and shadows stay crisp. `MIR_BLUR_PASSES` (1-6, default 3) and
  `MIR_BLUR_OFFSET` (default 2.0) in umoja-mir's environment tune the
  strength. The frontend hands regions to the renderer through
  `mir::graphics::background_blur` (exported from libmirplatform in its own
  `MIR_PLATFORM_2.28_umoja` version node); `mrg::Renderer` only gains a
  private class and method, so its ABI is unchanged. umoja-panel and
  umoja-dock request it (`umoja_common::blur`).
- `UPSTREAM`: the archive source version we patch; our version adds
  `umoja<N>` (`SUFFIX`), which sorts above it and above `ubuntu<N>` updates.

`build-source.sh <workdir>` fetches that source from Launchpad, adds the
patches and the changelog entry, and leaves `mir-<ver>/` ready for
`dpkg-buildpackage`. `.github/workflows/mir.yml` test-builds the binaries
and uploads the source package to the PPA.

When the archive moves to a new Mir, bump `UPSTREAM` (and refresh the patch).
