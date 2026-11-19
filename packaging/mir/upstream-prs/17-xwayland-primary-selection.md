# 17: Bridge the PRIMARY selection between Xwayland and Wayland clients

PR title:      xwayland: sync the PRIMARY selection as well as CLIPBOARD
Umoja patch:   3002_xwayland-primary-selection.patch
Applies to canonical/mir main (d8f3edb): yes, cleanly
Depends on:    nothing
Kind:          feature (fills a gap; no API change)
Before submitting:
  - Should PRIMARY bridging follow an option? Mir already lets shells disable primary selection for Wayland clients; if that setting exists on main, honour it here too.

---------------------------------------- PR description ----------------------------------------

## What's new?

Mir's Xwayland support only syncs the CLIPBOARD selection, so middle-click paste doesn't cross between X11 and Wayland apps: text selected in an X11 app can't be pasted with the middle button into a Wayland app, and vice versa. Every other Xwayland-capable compositor (mutter, KWin, wlroots) bridges PRIMARY.

`XWaylandClipboardSource` and `XWaylandClipboardProvider` are now parameterised by the X11 selection atom. The XWM runs a second pair for `PRIMARY` against the primary selection clipboard that `zwp_primary_selection_device_manager_v1` already uses, alongside the existing `CLIPBOARD` pair against the main clipboard. Internal only, no ABI change.

## How to test

1. Run an X11 app (xterm) and a Wayland app (gedit / GNOME Text Editor) under Mir with Xwayland enabled.
2. Select text in xterm, middle-click in the Wayland app: the text is pasted.
3. Select text in the Wayland app, middle-click in xterm: the text is pasted.
4. Ctrl+C / Ctrl+V between them still works as before.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
