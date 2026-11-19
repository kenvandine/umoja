# 03: Read an X11 window's _NET_WM_STATE when it maps

PR title:      xwayland: complete the _NET_WM_STATE read in XWaylandSurface::map()
Umoja patch:   3010_xwayland-initial-net-wm-state.patch
Applies to canonical/mir main (d8f3edb): yes, cleanly
Depends on:    nothing
Kind:          bug fix (one line)

---------------------------------------- PR description ----------------------------------------

## What's new?

`XWaylandSurface::map()` queues a read of `_NET_WM_STATE` with `connection->read_property(...)` but never calls the returned cookie, so the reply handler never runs and the window's initial state is ignored.

X11 windows that map already fullscreen or maximized are then shown restored at their pre-map size. Wine/Proton games hit this all the time: a Unity game showed as a 314x208 window at the bottom of the screen until it asked to go fullscreen again later.

The cookie is now called right after it is queued, as the comment below it intends ("If we had more properties to read we would queue them all up before completing the first one").

## How to test

1. Run an X11 client that sets `_NET_WM_STATE_FULLSCREEN` before mapping, e.g. `xterm -fullscreen` or a Wine/Proton game.
2. Before: it maps as a small restored window. After: it maps fullscreen.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
