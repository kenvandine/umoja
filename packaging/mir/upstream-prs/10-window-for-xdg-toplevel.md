# 10: miral::window_for() should accept an xdg_toplevel

PR title:      frontend: resolve xdg_toplevel resources in get_window() / miral::window_for()
Umoja patch:   3017_window-for-xdg-toplevel.patch
Applies to canonical/mir main (d8f3edb): yes, cleanly
Depends on:    nothing
Kind:          small API completion
Before submitting:
  - Mention the real user: shells implementing xdg-session-management-v1 (or other protocols that pass xdg_toplevel objects) through miral::WaylandExtensions. If Mir later implements xdg-session-management itself this is still useful for extensions.

---------------------------------------- PR description ----------------------------------------

## What's new?

`miral::window_for()` (via `mir::frontend::get_window()`) resolves `wl_surface`, `xdg_surface` and layer-shell resources to their window, but not `xdg_toplevel`. Several protocols pass `xdg_toplevel` objects rather than surfaces: `xdg-session-management-v1`'s `add_toplevel` / `restore_toplevel`, `xdg-toplevel-icon-v1`, `xdg-dialog-v1`, `xdg-toplevel-tag-v1`. A shell implementing one of them through `miral::WaylandExtensions` had no way to find the window, short of guessing from the order in which a client's windows appear.

`get_window()` now also accepts an `xdg_toplevel` and returns its scene surface if it has one.

## How to test

From a `WaylandExtensions` extension that receives an `xdg_toplevel` resource, call `miral::window_for(resource)`. Before: an empty `Window`. After: the toplevel's window once it is mapped.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added (update the window_for() doc comment to list xdg_toplevel)
