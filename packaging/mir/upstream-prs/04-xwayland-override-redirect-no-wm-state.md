# 04: Don't set WM state on X11 override-redirect windows

PR title:      xwayland: leave WM_STATE and _NET_WM_STATE off override-redirect windows
Umoja patch:   3019_xwayland-override-redirect-no-wm-state.patch
Applies to canonical/mir main (d8f3edb): yes, cleanly
Depends on:    nothing
Kind:          bug fix
Before submitting:
  - Mention the Steam/Chromium symptom in an issue too if maintainers want one; the PR stands alone.

---------------------------------------- PR description ----------------------------------------

## What's new?

An override-redirect window never gets a MapRequest, so its cached state stays "withdrawn" and Mir creates its scene surface as `mir_window_state_hidden`. The surface observer then reports that state back to X: the XWM writes `WM_STATE` Iconic and `_NET_WM_STATE_HIDDEN` on the window and lowers it to the bottom of the X stack, a few milliseconds after the client mapped it.

Chromium-based clients take that literally. Steam's menus showed up as blank white boxes or closed again immediately, because Chromium saw its freshly mapped menu marked hidden and stacked below its parent.

ICCCM leaves override-redirect windows entirely to their client. The XWM now:

- doesn't write `WM_STATE` / `_NET_WM_STATE` on override-redirect windows (`inform_client_of_window_state()` returns early), and
- doesn't restack them to the bottom for a hidden/minimized scene state.

Managed windows are unchanged.

## How to test

1. Run Steam (or any Chromium/Electron X11 app with native menus, under Xwayland) and open a menu from its menu bar.
2. Before: the menu is a blank white box or closes at once; `xprop` on it shows `_NET_WM_STATE_HIDDEN`. After: the menu draws and works, and has no WM state set.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
