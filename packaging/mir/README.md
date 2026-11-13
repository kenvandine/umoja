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
  The copy buffers are pooled (umoja14): a window redrawing on that
  monitor used to zero and fault in a fresh multi-megabyte buffer and a new
  texture every frame. While `$XDG_RUNTIME_DIR/umoja-debug-windows` exists,
  Mir logs the amount copied every 10 s with the most-copied buffer sizes.
- `patches/3021_drm-connector-no-reprobe.patch` (umoja24): Mir asked the
  kernel to probe every connector again at each query, four times over at
  startup; two empty DisplayPort ports cost 0.68 s per probe on p16g3 (5.5 s
  of black screen at every login). Reads the kernel's kept state instead and
  probes only when there is none. `MIR_DRM_ALWAYS_PROBE=1` turns it off.
- `patches/3020_cross-gpu-map-linear.patch`: the 3009 copy cost 5.5 ms of
  `glReadPixels` per 6 MB buffer on the compositing thread (measured on
  p16g3: 450 MB/s, 4 s of every 10 s, with two windows redrawing on the
  NVIDIA monitor). A linear dma-buf is now mapped and copied with memcpy
  instead. `MIR_DMABUF_LINEAR_ONLY=1` makes Mir offer clients only the
  linear modifier so their buffers qualify; umoja-mir sets it when a
  monitor is on the other GPU (`crates/umoja-mir/src/gpu.rs`). Spare copy
  buffers are released when a new size is needed, and the debug line says
  how many copies were mapped.
- `patches/3010_xwayland-initial-net-wm-state.patch`: `XWaylandSurface::map()`
  queued a read of `_NET_WM_STATE` but never called the returned cookie, so
  an X11 window's initial state was ignored. Wine/Proton games map their
  window already fullscreen, and showed as a tiny restored window at the
  bottom of the screen until they asked again.
- `patches/3011_direct-scanout-switch.patch`: `MIR_DIRECT_SCANOUT=0` makes
  the compositor always composite instead of handing a fullscreen client's
  buffer straight to the display. umoja-mir sets it on hybrid laptops with
  a screen on the discrete GPU: a fullscreen game there (buffer drawn on
  the other GPU) showed a white screen and stalled until Alt+Tab.
- `patches/3012_cursor-surface-offset-and-hidden-role.patch`: cursors that
  clients draw themselves. GTK switches cursor images with `wl_surface.offset`
  instead of a new `set_cursor`; Mir ignored it, so the terminal's I-beam was
  drawn at the arrow's hotspot and selections didn't line up. The hotspot is
  now also converted to buffer pixels on scaled outputs. And hiding the
  pointer (VTE does on typing and paste) dropped the cursor surface's role,
  so the pointer stayed invisible until it left the window.
- `patches/3013_confine-pointer-fractional-edge.patch`: the pointer was
  confined to the screens after rounding its position towards zero, so a
  position between -1 and 0 (pushed past the top or left edge) was never
  clamped and clients got events at a negative surface position. A click
  with the pointer pushed into the top bar's corners missed the system menu
  and Activities.
- `patches/3014_screencopy-region-output-local.patch`: `wlr-screencopy`'s
  `capture_output_region` region is output-local (wf-recorder subtracts the
  output's position), but Mir clipped it against the output in layout
  coordinates, so recording a region on a monitor right of another came out
  cropped to the left part of that monitor.
- `patches/3015_screencopy-renderer-per-size.patch`: the screen shooter
  kept one GL renderer, for the last capture's size, so grim capturing two
  monitors of different sizes built two new renderers (shaders, blur
  targets) for every screenshot. The overview's background preview refresh
  did that every 10 seconds. It now keeps one renderer per size (up to 4).
- `patches/3016_xdg-activation-no-focus-stealing-prevention.patch`: Mir's
  xdg-activation behaved like GNOME's focus stealing prevention and left the
  window a click asked for behind. Only the focused client got a real token,
  so the top bar (a layer surface) got a bogus one when a notification was
  clicked; any key press or focus change revoked pending tokens; tokens
  expired after 3 s. A client naming the serial of an input event it
  received now gets a real token, key presses and focus changes don't revoke
  tokens, and they last 30 s (still single use).
- `patches/3017_window-for-xdg-toplevel.patch`: `miral::window_for()`
  accepted wl_surface, xdg_surface and layer surfaces but not xdg_toplevel.
  umoja-mir's xdg-session-management-v1 (`src/shim/session_management.cpp`)
  gets xdg_toplevels from `add_toplevel`/`restore_toplevel` and needs their
  window. Without the patch it falls back to matching a client's next
  window.
- `patches/3018_scene-filter-hook.patch`: Mir can only capture an area of
  the screen with everything on it. The surface stack now asks
  `mir_scene_filter_hook` (exported by umoja-mir, found with `dlsym`) about
  each surface when building a compositor's scene: 1 shows it even when
  hidden, 0 leaves it out, -1 keeps the usual rules. umoja-mir answers only
  for its own screen shooter (`CaptureWindow` on org.umoja.Compositor), so
  it renders one window alone, minimized or on another workspace too, for
  the Alt+Tab, overview and dock thumbnails. No ABI change.
- `patches/3019_xwayland-override-redirect-no-wm-state.patch` (umoja19):
  Steam's menus showed as blank white boxes or closed again at once. An X11
  override-redirect window never gets a MapRequest, so Mir creates its
  surface in the hidden (withdrawn) state, and the XWM reported that back:
  it set `_NET_WM_STATE_HIDDEN` on the menu Steam had just mapped and
  lowered it to the bottom of the X stack, and Chromium hid the menu. The
  XWM now leaves WM_STATE, `_NET_WM_STATE` and hidden-state restacking off
  override-redirect windows, as ICCCM expects.
- The blur patch's GL error drain is bounded (a lost context reports
  `GL_CONTEXT_LOST` forever).
- `UPSTREAM`: the archive source version we patch; our version adds
  `umoja<N>` (`SUFFIX`), which sorts above it and above `ubuntu<N>` updates.

`build-source.sh <workdir>` fetches that source from Launchpad, adds the
patches and the changelog entry, and leaves `mir-<ver>/` ready for
`dpkg-buildpackage`. `.github/workflows/mir.yml` test-builds the binaries
and uploads the source package to the PPA.

When the archive moves to a new Mir, bump `UPSTREAM` (and refresh the patch).
