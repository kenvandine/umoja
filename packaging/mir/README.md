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
- `patches/3005_surface-destroy-pending-syncobj-commit.patch`: a commit
  with a linux-drm-syncobj acquire point waits on an fd source in the
  Wayland event loop, but `~WlSurface()` never removed it. When the client
  died first (umoja-panel killed for a protocol error while the screen was
  off, 2026-11-09), the fence later ran `complete_commit()` on the freed
  surface, which spun forever in `std::advance` over a garbage list; the
  main loop then blocked in `WaylandConnector::stop()` at the next VT
  switch. Upstream report draft: `upstream-bugs/syncobj-commit-after-surface-destroy.md`.
- `patches/3006_logind-own-session.patch`: Mir took control of seat0's
  *active* logind session. umoja-session restarts a crashed umoja-mir in the
  same login, and if the user was on another VT then, Mir controlled that
  session and never resumed after switching back. It now uses
  `XDG_SESSION_ID` (falling back to the active session). Upstream report
  draft: `upstream-bugs/logind-active-session-not-own.md`.
- `patches/3009_cross-gpu-cpu-copy.patch`: GL/Vulkan windows still showed
  stripes of wrong content on NVIDIA-driven monitors (wl_shm windows were
  fine). A client dma-buf drawn on a GPU other than the one that imported it
  is now read back once per commit through the importing GPU and uploaded
  like a wl_shm buffer (`MIR_CROSS_GPU_CPU_COPY=0` turns this off).
- `patches/3010_xwayland-initial-net-wm-state.patch`: `XWaylandSurface::map()`
  queued a read of `_NET_WM_STATE` but never called the returned cookie, so
  an X11 window's initial state was ignored. Wine/Proton games map their
  window already fullscreen, and showed as a tiny restored window at the
  bottom of the screen until they asked again.
- The blur patch's GL error drain is bounded (a lost context reports
  `GL_CONTEXT_LOST` forever).
- `UPSTREAM`: the archive source version we patch; our version adds
  `umoja<N>` (`SUFFIX`), which sorts above it and above `ubuntu<N>` updates.

`build-source.sh <workdir>` fetches that source from Launchpad, adds the
patches and the changelog entry, and leaves `mir-<ver>/` ready for
`dpkg-buildpackage`. `.github/workflows/mir.yml` test-builds the binaries
and uploads the source package to the PPA.

When the archive moves to a new Mir, bump `UPSTREAM` (and refresh the patch).
