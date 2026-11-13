# Plan: moving umoja-mir onto Mir's Rust crate (`mir`)

Status: **proposal, awaiting Ken's approval.** No code until a phase is approved;
phases run one at a time.

Written 2026-11-13 against umoja main `14479c8` and mir-rs main `2a70cc7`
(2026-09-15).

## TL;DR

- umoja-mir is *already* mostly Rust (about 10.4k lines: D-Bus API, tiling,
  switcher, workspaces, gestures, idle, screencast portal...). What the crate
  could replace is our hand-written C++ shim (about 6.3k lines), and in
  practice only part of it: the `MirRunner` set-up and the window-management
  policy (`UmojaWindowManager` in `shim/umoja_shim.cpp`, about 2.5k lines).
- The crate is very young: one crates.io release (`0.0.1`, 2026-08-26, ~100
  downloads), ~50 commits, one example compositor. It is Canonical's
  26.10-cycle roadmap item "Rust compositor support", so it is official and
  will grow, but its API will churn.
- **Hard prerequisite: Mir 2.29+ (miral 6.0).** We are on the archive's Mir
  2.28.0-4, and stonking and tenacious both still ship 2.28. Moving to the
  crate means we first package Mir 2.30 ourselves in the PPA and rebase our
  16 patches.
- **None of our 16 Mir patches move into the crate.** They change Mir's
  server (renderer, Wayland frontend, Xwayland, input platform). The crate
  only changes how our code talks to MirAL. The patches stay, get
  upstreamed, or get dropped where Mir 2.29/2.30 already fixed them.
- The crate today has **no escape hatch to `mir::Server`**, no custom Wayland
  extensions, no per-client extension gating, no event filters and no
  screen shooter. Six of our seven server hooks (idle, capture, screencast,
  remote input, EIS, input devices, session management) need those, so they
  stay C++ until either the crate grows an escape hatch or Mir grows public
  APIs.
- Recommendation: do Phase 1 (Mir 2.30 rebase) on its own merits, then the
  cheap Phase 2/3 (upstream an escape hatch, switch the runner). Decide on
  Phase 4 (port the policy to Rust) after Phase 3 ships, because that is
  where all the risk and most of the work is.

## What the crate is

`canonical/mir-rs`, crate name `mir` (directory `mir-rs/`), GPL-2/3.

```
your compositor (Rust)
   mir crate: safe Rust API  +  src/sys/bridge.cpp (cxx bridge, ~1.5k lines C++)
   libmiral (system C++ library, miral >= 6.0, mircore/mircommon >= 2.29)
```

It is the same pattern as our shim (a C++ bridge compiled by `build.rs`,
calling back into Rust), but written and maintained by the Mir team, built
on `cxx` instead of hand-rolled `extern "C"`.

What it covers (all public, safe API):

| Area | Crate API |
| --- | --- |
| Runner | `MirRunner::new(args).add(ext).add_window_management_policy::<P>().run()`, `on_start`/`on_stop`, `RunnerHandle::stop()`, config options |
| WM policy | `WindowManagementPolicy` trait: `place_new_window`, `handle_window_ready`, `handle_modify_window`, `handle_activate_window`, keyboard/pointer/touch handlers, `handle_request_move`/`resize`, `confirm_placement_on_display`, `confirm_inherited_move`, `advise(Advice)` (begin/end, app/window new/delete, focus, state, move, resize, raise, output and zone create/update/delete) |
| WM tools | raise, activate, modify, close, drag, `window_at`, `all_windows`, `place_and_size_for_state`, workspaces (create, add/remove tree, move content, `workspaces_containing`, `windows_in_workspace`), `move_cursor_to`, `invoke_under_lock` from other threads |
| Window spec | everything we use: depth layer, focus mode, input shape, exclusive rect, tiled edges, output id, placement hints, aux rect, alpha, app id, SSD; experimental `with_transform` |
| Extensions | Decorations, Keymap, X11Support, WaylandExtensions (enable/disable only), DisplayConfiguration (no options), CursorTheme, CursorScale, IdleListener (dim/off/wake), SessionLockListener, InputConfiguration, Magnifier, a11y keys, OutputFilter, AddInitCallback |

What it does not cover (things umoja-mir uses today):

| Missing | Used by us for |
| --- | --- |
| Any access to `mir::Server` (`the_idle_handler`, `the_session_lock`, `the_screen_shooter_factory`, `the_buffer_allocator`, `the_scene`, `the_cursor_observer_multiplexer`, `the_input_device_hub`, `the_input_device_registry`) | idle.cpp, capture.cpp, screencast.cpp, remote_input.cpp, eis.cpp, input_config.cpp |
| `WaylandExtensions::add_extension` (custom protocols) | xdg-session-management-v1 (session_management.cpp) |
| `WaylandExtensions::conditionally_enable` (per-client) | only umoja-* binaries get ext/wlr data-control (clipboard history); everything else is gated this way too |
| `PrependEventFilter` | natural scroll rewrite, idle activity, input capture |
| `DisplayConfiguration` layout option, reload | ~/.config/umoja-mir.display + `--display-layout` |
| `MinimalWindowManager` as a base | our policy derives from it (default move/resize gestures, focus rules, popup handling); the crate's policy derives from bare `WindowManagementPolicy` |
| Raw events: pointer relative motion, multi-point touch frames, the `MirEvent` itself | edge pressure (dx/dy), touch gestures (`umoja_touch` takes all points), input capture |
| `pid_of`, `x11_display()`, `wayland_display()` | shell-client check, session start-up env |

## Our Mir patches vs. the crate

The crate can host none of these, because they are inside Mir. What matters is
what Mir 2.29/2.30 already does and what we should send upstream.

| Patch | What it is | With Mir 2.30 |
| --- | --- | --- |
| 3001 touchpad gesture hook | dlsym hook from the evdev platform | keep (exported from our binary, works with the crate too); upstream a MirAL gesture API |
| 3002 Xwayland PRIMARY | PRIMARY selection bridging | keep, upstream |
| 3003 popup reposition offset | frontend bug | rebase, upstream |
| 3004 background blur (ext-background-effect-v1) | renderer + frontend | keep; overlaps roadmap item "rendering pipeline / window effects" |
| 3005 syncobj after surface destroy | crash fix | rebase, upstream (draft exists) |
| 3006 logind own session | session fix | rebase, upstream (draft exists) |
| 3009 cross-GPU CPU copy | hybrid NVIDIA laptops | keep |
| 3010 Xwayland initial _NET_WM_STATE | games start fullscreen | rebase, upstream |
| 3011 direct scanout switch | hybrid laptops | keep |
| 3012 cursor offset + hidden role | GTK/VTE cursors | **2.30 has "hidden cursor surfaces receive the cursor role"**; keep only the offset half |
| 3013 confine fractional edge | top-corner clicks | rebase, upstream |
| 3014 screencopy region output-local | wf-recorder regions | rebase, upstream |
| 3015 screencopy renderer per size | grim perf | **2.30: "screen capture reuses its renderer and handles output resizing"**; likely drop |
| 3016 xdg-activation no FSP | notification click focus | keep; ask upstream for a policy switch |
| 3017 window_for(xdg_toplevel) | session management | rebase, upstream |
| 3018 scene filter hook | CaptureWindow thumbnails | **2.29 adds ext-foreign-toplevel-image-capture-source and single-surface screenshots**; investigate replacing 3018 + capture.cpp |

So the Mir 2.30 rebase is worth doing on its own: it probably drops one or two
patches, gives us xdg-dialog-v1, wl_fixes and the toplevel image capture, and
it is the version the crate needs.

## Costs of Mir 2.29+

- miral 6.0 is an API break: `std::span` in workspace advice, reference-
  qualified `WindowSpecification`, `handle_raise_window` became
  `handle_activate_window`, policy hierarchy reorganised, `SetTerminator`
  gone. Our C++ shim needs porting even if we never use the crate.
- Mir 2.29 needs C++26; Mir 2.30 needs Rust to build (Rust evdev platform).
  Launchpad builds for stonking need a new enough g++ and rustc/cargo; check
  before committing.
- EGLStream and wl-drm are removed. EGLStream was the old NVIDIA path; p16g3
  (Intel+NVIDIA hybrid) uses GBM and dma-buf, but this must be checked on
  that machine before anything ships.
- We become the maintainers of a Mir that is newer than the archive. When
  the archive moves to 2.29+ we go back to patching the archive version.

## Phased plan

Each phase ends with something shippable, live-tested on the laptop and
p16g3 (hybrid), and needs Ken's go-ahead before it starts.

### Phase 0: spike (about a day, nothing shipped)

- Build Mir 2.30 from upstream with our patches rebased, on the laptop.
- Build `mir-rs-tiling` against it and run it nested and on a VT.
- Check Launchpad toolchain for stonking (g++ for C++26, rustc for 2.30).
- Check whether 2.29's toplevel image capture can replace 3018 + capture.cpp
  for thumbnails.
- Output: a short go/no-go note with the patch rebase results.

### Phase 1: Mir 2.30 in the PPA, shim ported to miral 6 (no crate yet)

- `packaging/mir`: new `UPSTREAM` source (2.30 tarball instead of the
  archive package), rebased patches, drop the ones 2.30 fixed.
- Port `crates/umoja-mir/src/shim/*.cpp` to the miral 6 API.
- Bump `umoja-mir` build-dep to `libmiral-dev (>= 6.0)`.
- Upload to umoja-edge first, run the install tests, full live test (games,
  hybrid monitor, screencast, thumbnails, blur, greeter, lock/unlock).
- Value on its own: newer Mir, fewer patches, upstream fixes.

### Phase 2: give the crate an escape hatch (upstream PRs to mir-rs)

Smallest set that lets us use the crate without losing anything:

1. A public "raw server option" extension: a C++ `std::function<void(mir::Server&)>`
   supplied by the downstream crate (a C symbol, or a `cxx` type we can
   construct). This alone lets every C++ hook we have keep working.
2. `WaylandExtensions::conditionally_enable(name, Fn(&ClientInfo) -> bool)`
   with pid access.
3. Event filter (`PrependEventFilter`) with access to relative motion.
4. `DisplayConfiguration` with the layout option and config path.
5. Optional: a `MinimalWindowManager`-based policy adapter, or ports of its
   default move/resize/focus behaviour as helpers.

If upstream is slow we carry these as a vendored patch on the crate
(`vendor/mir` with a `patches/` series, like we do for Mir). Escape hatch 1 is
the must-have; the others can stay C++ behind it.

### Phase 3: switch the runner to the crate, keep the C++ policy

- `main.rs` builds a `mir::MirRunner`, adds the crate's X11Support,
  CursorTheme, CursorScale, Keymap, plus our C++ pieces (including
  `set_window_management_policy<UmojaWindowManager>()`) through the raw
  server option.
- `umoja_mir_run`, `umoja_mir_stop` and the C++ runner set-up go away.
- Behaviour should be byte-for-byte the same; this is a plumbing change.

### Phase 4: port the window-management policy to Rust

The big one. `UmojaWindowManager` (placement, layer-shell keyboard rules,
fullscreen lift, depth layers, workspaces, tiling/snap, modifier drag,
window menu, switcher, xdg-activation behaviour, edge clicks) becomes an
`impl WindowManagementPolicy`.

- Much of the logic is already Rust (`tiling.rs`, `snap.rs`, `switcher.rs`,
  `workspaces.rs`, `edge_pressure.rs`, `input.rs`), called through
  `umoja_*` FFI. Those become direct calls, and the FFI table in `ffi.rs`
  shrinks.
- Run both policies side by side behind `UMOJA_RUST_POLICY=1` for a release
  or two, then remove the C++ one.
- Any crate gap found here goes upstream (as in Phase 2).
- Needs a policy test harness first: the crate's seam pattern plus headless
  Mir sessions, covering the regressions we have fixed before (focus after
  notification click, Quick Settings keyboard grab, games fullscreen, drag
  tile preview stuck, new window placement at 250%).

### Phase 5: shrink the remaining C++ as Mir/crate APIs appear

| Piece | Path off C++ |
| --- | --- |
| idle.cpp | crate `IdleListener`/`SessionLockListener` once they expose timeouts and cursor hiding |
| input_config.cpp | crate `InputConfiguration` (needs per-device settings) |
| capture.cpp (+3018) | toplevel image capture from Mir 2.29, maybe as a Wayland client in the panel |
| screencast.cpp | Mir's ext-image-copy-capture, or keep |
| remote_input.cpp, eis.cpp | stay C++ until Mir has a libei/virtual input API |
| session_management.cpp | upstream xdg-session-management-v1 into Mir |

Realistically some C++ stays for a long time; the goal is that what stays is
server plumbing, not policy.

## What does not change

- The D-Bus API (org.umoja.Compositor) is already Rust (`shell.rs`,
  `introspect.rs`, zbus). Panel, dock and overlay are untouched in every
  phase.
- Debian packaging keeps vendoring crates; the crate adds `cxx`,
  `cxx-build` (pinned `<1.0.195`), `bindgen` and `bitflags`.

## Risks

1. **Crate maturity.** 0.0.1, one release, API will change. Mitigation: pin
   a git revision, vendor it, keep our wrapper thin.
2. **Owning a non-archive Mir.** Bigger PPA package, our own security
   updates, C++26 and Rust build deps on Launchpad. Mitigation: Phase 0
   checks the toolchain first; follow the archive again once it moves.
3. **Hybrid GPU regressions** (EGLStream/wl-drm removal, cross-GPU patch
   rebase). Mitigation: p16g3 rear-port test is a gate for Phase 1.
4. **Policy regressions in Phase 4.** Lots of hand-tuned behaviour. Mitigation:
   side-by-side switch, test harness first, port in one go but keep the C++
   policy until Ken signs off.
5. **Upstream says no to the escape hatch.** Then we carry a small crate
   patch indefinitely; still cheaper than our own runner.
6. **Two Rust/C++ bridges at once** during Phases 3 and 4 (crate's cxx
   bridge and our extern "C" shim) share global state such as the tools
   lock. Mitigation: one owner per object, documented in `ffi.rs`.

## Questions for Ken

- OK to package Mir 2.30 ourselves (Phase 1), rather than wait for the
  archive?
- For the escape hatch, upstream first and wait, or carry a vendored crate
  patch so we can start?
- Is Phase 4 (policy in Rust) the real goal, or is it enough to stop there
  after Phase 3?

## Sources

- crate: https://crates.io/crates/mir, repo https://github.com/canonical/mir-rs
- Mir 2.29 notes: https://canonical.com/mir/docs/latest/release-notes/2.29/
- Mir 2.30 notes: https://canonical.com/mir/docs/latest/release-notes/2.30/
- Mir roadmap (26.10): https://canonical.com/mir/docs/latest/roadmap/
- Archive versions: packages.ubuntu.com libmiral-dev (stonking 2.28.0-4)
