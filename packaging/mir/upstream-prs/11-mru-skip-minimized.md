# 11: Don't fall back to focusing minimized windows

PR title:      miral: MRU focus fallback skips minimized windows
Umoja patch:   3024_mru-skip-minimized.patch
Applies to canonical/mir main (d8f3edb): yes, cleanly
Depends on:    nothing
Kind:          behaviour fix in MirAL (may need discussion: it changes which window gets focus)
Before submitting:
  - Remove the "Umoja:" comment prefix.
  - Check the MRU/focus unit tests in tests/miral (some may expect the old behaviour) and add one: minimize the active window while another minimized window exists, expect neither restored.

---------------------------------------- PR description ----------------------------------------

## What's new?

`MRUWindowList`'s `visible()` filter only skips hidden windows. So when the active window is minimized or closed, the focus fallback (`refocus()`, the minimize path of `set_state()`) can pick a *minimized* window, and `select_active_window()` restores it.

Visible effects with the floating/minimal window managers:

- minimizing two windows in a row brings the first one back;
- closing the last open window brings back a minimized one, even from another workspace.

Other desktops (mutter's `focus_default_window`, KWin) never do this: a minimized window comes back only when the user asks for it. The filter now skips `mir_window_state_minimized` as well. Explicit activation (a taskbar, Alt+Tab, `foreign-toplevel` activate) does not go through this list and still restores minimized windows.

## How to test

1. Open windows A and B, minimize A, then minimize B.
2. Before: A is restored and focused. After: nothing is restored; focus goes to the next non-minimized window or nowhere.
3. Close every non-minimized window: before, a minimized one comes back; after, none does.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
