# Upstreaming Umoja's Mir patches to canonical/mir

Written 2026-11-19 from the 25 patches in packaging/mir/patches (Mir 2.28.0-4umoja29), checked one by one against canonical/mir main at d8f3edb (2026-11-19; latest release v2.31.0). Text only: no upstream PRs have been opened.

Each NN-*.md file is one PR: a header for you (PR title, which Umoja patch, whether it applies to main, dependencies, what to change before submitting), then the PR description below the dashed line, laid out like Mir's .github/pull_request_template.md (Closes / What's new? / How to test / Checklist).

## Before the first PR

- Canonical projects usually require the Canonical CLA; sign it before opening anything.
- Our patches are against 2.28. Every PR has to be rebased onto main; "applies cleanly" below means `git apply --check` succeeded on main, not that it builds or passes tests there.
- Strip Umoja-isms when porting: "Umoja:" comment prefixes, "Author: Umoja developers" / DEP-3 headers, the $XDG_RUNTIME_DIR/umoja-debug-windows switch, the MIR_PLATFORM_2.28_umoja symbol node, "Copyright © Umoja developers" headers.
- Mir runs pre-commit (formatting), a symbols check for ABI changes, spread tests and TICS static analysis on PRs. Unit tests are expected for behaviour changes.
- Six of our patches talk to umoja-mir through C functions looked up with dlsym (mir_*_hook). Upstream won't take that mechanism. The five of them still needed upstream (all but 3018) map to PRs 19, 20, 21, 22, 23 and 25, which describe the MirAL API each hook should become instead, and need an issue first to agree on its shape. Once one lands, umoja-mir can drop its hook for that feature.
- For the two bugs with drafted reports (packaging/mir/upstream-bugs/), file the issue first and reference it from the PR.

## Submission order

Wave 1 is small, self-contained bug fixes: good first PRs, little to argue about. Within a wave the order is a suggestion; PRs in the same wave can go in parallel unless "Depends on" says otherwise.

| #  | PR title | Umoja patch | Applies to main | Depends on |
|----|----------|-------------|-----------------|------------|
| **Wave 1: bug fixes** |
| 01 | frontend: remove a pending explicit-sync commit when its WlSurface is destroyed | 3005 | rebase (small) | none |
| 02 | console: take control of our own logind session (XDG_SESSION_ID) | 3006 | rebase (small) | none |
| 03 | xwayland: complete the _NET_WM_STATE read in XWaylandSurface::map() | 3010 | clean | none |
| 04 | xwayland: leave WM_STATE and _NET_WM_STATE off override-redirect windows | 3019 | clean | none |
| 05 | input: round down, not towards zero, when confining the cursor | 3013 | clean | none |
| 06 | wlr-screencopy: treat capture_output_region coordinates as output-local | 3014 | clean | none |
| 07 | xdg-shell: apply aux_rect_offset in xdg_popup.reposition like on create | 3003 | clean | none |
| 08 | wl_pointer: move the cursor hotspot by wl_surface.offset | 3012 part 1 | rebase | none |
| 09 | wl_pointer: convert a surface cursor's hotspot to buffer pixels | 3012 part 2 | rebase | 08 (same functions) |
| 10 | frontend: resolve xdg_toplevel resources in miral::window_for() | 3017 | clean | none |
| 11 | miral: MRU focus fallback skips minimized windows | 3024 | clean | none |
| **Wave 2: behaviour and performance (expect discussion)** |
| 12 | frontend: answer frame callbacks of occluded surfaces after a timeout | 3022 | rebase | none |
| 13 | kms: avoid forced connector probes on i915/xe for disconnected connectors | 3021 | clean | none |
| 14 | xdg-activation: grant tokens to unfocused clients that prove recent input | 3016 part 1 | clean (split) | none |
| 15 | shell: make the activation token timeout configurable (or raise it) | 3016 part 2 | clean (split) | none |
| 16 | xdg-activation: don't revoke tokens on key press/focus change unless FSP is on | 3016 part 3 | needs new plumbing | issue first; after 14, 15 |
| 17 | xwayland: sync the PRIMARY selection as well as CLIPBOARD | 3002 | clean | none |
| **Wave 3: new MirAL APIs (issue first, code needs redesign)** |
| 18 | input: share Num Lock state across all keyboards of the seat | 3023 part 1 | rebase | none |
| 19 | miral: API to set the initial Num Lock state and observe changes | 3023 part 2 | redesign | 18 |
| 20 | xdg-shell: forward xdg_toplevel.show_window_menu to the policy | 3025 part 1 | redesign | none |
| 21 | xdg-shell: ping clients and report pongs (unresponsive app detection) | 3025 part 2 | redesign | none |
| 22 | miral: distinguish user-requested activations and report refused ones | 3026 | redesign | none (rebase after 14-16) |
| 23 | input: expose libinput touchpad gestures to the shell | 3001 | redesign (largest API) | none |
| 24 | Implement ext-background-effect-v1 (background blur) | 3004 | port (~1,300 lines) | none |
| 25 | scene: keep a destroyed surface on screen briefly (close animations) | 3027 | redesign | none upstream |
| **Wave 4: hybrid Intel+NVIDIA (one issue for all three, re-test on main first)** |
| 26 | platform: CPU-copy fallback for client dma-bufs drawn on another GPU | 3009 + 2 fixes from 3020 | rebase | none |
| 27 | platform: map linear dma-bufs for the cross-GPU copy, linear-only option | 3020 | rebase | 26 |
| 28 | compositor: don't direct-scanout buffers rendered on another GPU | 3011 (rework) | rework | none (file with 26) |

## Patchsets that must land together

Most PRs stand alone. The real couplings:

- **08 → 09**: cursor fixes split out of 3012; 09 edits the code 08 touches. Either order works if the second one is rebased.
- **14, 15, 16**: three independent parts of 3016 in the same files. 16 also needs an agreed design.
- **18 → 19**: the Num Lock API (19) is built on the seat-wide state (18). They can be one PR if maintainers prefer.
- **24**: protocol and renderer must land together; keep them as two commits in one PR.
- **26 → 27**: 27 optimises the copy that 26 adds. 28 is the same hardware bug but independent code.

## Not for upstream

| Umoja patch | Why not | What Umoja should do |
|-------------|---------|----------------------|
| 3018 scene filter hook (window thumbnails) | Superseded. Since v2.29.0 Mir has single-surface capture: `screen_shooter_factory->create_for_scene(executor, scene::create_surface_scene(surface))`, used by ext-foreign-toplevel-image-capture-source-v1. SingleSurfaceScene has no visibility check, so minimized and other-workspace windows can be captured too. | Switch umoja-mir's CaptureWindow to it when Umoja moves to Mir >= 2.30, then drop the patch. |
| 3015 screen shooter renderer per size | Superseded by upstream f9acf04 (v2.30.0): one renderer now resizes to any capture size and is only rebuilt when the pixel format changes. | Drop when moving to Mir >= 2.30. |
| 3012 part 3 (hidden cursor keeps its role) | Already on main: WlHiddenCursor sets the role and the old cursor is reset first. | Drops with the rebase. |
| dlsym hook mechanism (3001, 3018, 3023, 3025, 3026, 3027) | Umoja-specific plumbing between Mir and umoja-mir. | Keep carrying the hooks until the MirAL APIs in 19-23 and 25 land, then port umoja-mir to them. |

## Where Umoja's patch numbers ended up

3001 → 23 · 3002 → 17 · 3003 → 07 · 3004 → 24 · 3005 → 01 · 3006 → 02 · 3009 → 26 · 3010 → 03 · 3011 → 28 · 3012 → 08 + 09 (+ part already upstream) · 3013 → 05 · 3014 → 06 · 3015 → not needed · 3016 → 14 + 15 + 16 · 3017 → 10 · 3018 → not needed · 3019 → 04 · 3020 → 27 (+ two fixes into 26) · 3021 → 13 · 3022 → 12 · 3023 → 18 + 19 · 3024 → 11 · 3025 → 20 + 21 · 3026 → 22 · 3027 → 25

(There never were patches 3007 or 3008 in the repo.)
