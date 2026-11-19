# 12: Answer frame callbacks whose buffer is never composited

PR title:      frontend: answer wl_surface.frame callbacks of occluded surfaces after a timeout
Umoja patch:   3022_unconsumed-frame-callbacks.patch
Applies to canonical/mir main (d8f3edb): no, needs rebase (wayland_connector.cpp and WlSurface constructor changed)
Depends on:    nothing (textual overlap with 01 in wl_surface.cpp only)
Kind:          bug fix (client stall)
Before submitting:
  - Make the 500 ms value a named constant and say why (0.5-1 s effective, ~1 Hz for hidden clients).
  - Upstream may prefer a different mechanism (throttling occluded surfaces explicitly, or answering callbacks of surfaces dropped by occlusion in the compositor). Open with the reproducer; the patch is one valid fix.

---------------------------------------- PR description ----------------------------------------

## What's new?

Mir answers `wl_surface.frame` callbacks committed with a buffer when a compositor consumes that buffer, and occlusion culling skips the buffers of fully covered surfaces. So a window that is completely covered never gets its frame callback answered. Toolkits that only draw on a frame callback (GTK) stop drawing, which is fine while the window stays covered.

The problem is what happens when it is uncovered: that does not reliably lead to a composite that consumes the waiting buffer, so the client stays frozen. Input still works (a click moves a wizard to its next page) but the window never redraws until something redraws the whole screen, such as a VT switch.

We hit this with an installer started underneath a full-screen layer-shell welcome screen: after the welcome screen went away, the installer looked dead.

A second `FrameExecutor` with a 500 ms delay now checks surfaces that still have frame callbacks waiting. Callbacks still pending 0.5-1 s after their commit are answered anyway. This is what other compositors effectively do for hidden surfaces (throttle rather than stall):

- a visible surface has its buffer consumed within a frame and never reaches the fallback;
- a covered GTK window redraws at about 1 Hz instead of not at all (measured in a VM with llvmpipe: 34% CPU for an animated installer page, rather than 333% if callbacks were answered at full rate).

## How to test

1. Map a full-screen layer surface (or a fullscreen window) over a GTK 4 window that animates (a spinner).
2. Remove the covering surface.
3. Before: the GTK window shows its old contents until a VT switch or an output reconfiguration. After: it is up to date within a second.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
