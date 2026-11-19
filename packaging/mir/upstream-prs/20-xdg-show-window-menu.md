# 20: Implement xdg_toplevel.show_window_menu

PR title:      xdg-shell: forward xdg_toplevel.show_window_menu to the window management policy
Umoja patch:   3025_xdg-window-menu-and-ping.patch, part 1 of 2
Applies to canonical/mir main (d8f3edb): no; the dlsym hook must become a MirAL policy call, and the patch needs rebase
Depends on:    nothing
Kind:          missing protocol request (currently `// TODO`) + new policy API
Before submitting:
  - Replace mir_show_window_menu_hook with a WindowManagementPolicy method. Proposed: `virtual void handle_request_window_menu(WindowInfo& window_info, Point position, MirInputEvent const* input_event)` with an empty default, mirroring handle_request_move/resize. Adding a virtual needs a MirAL ABI decision (new virtual at the next ABI break, or an opt-in interface the policy can also implement); ask in the issue.
  - Validate the serial like move/resize do (event_for(serial)), and pass the event so the shell can position a menu at the pointer.
  - Mir's own server-side decorations may want to call the same policy method on right-click on the title bar.

---------------------------------------- PR description ----------------------------------------

Related: #??? (issue: "xdg_toplevel.show_window_menu is not implemented")

## What's new?

`XdgToplevelStable::show_window_menu` is an empty `// TODO`. Clients with client-side decorations (GTK, libadwaita, Qt with CSD, Firefox, Chromium) send it when the user right-clicks the title bar or presses Alt+Space, and expect the compositor's window menu (Minimize, Maximize, Move, Resize, Always on Top, Close...). On Mir nothing happens.

This forwards the request to the window management policy:

- a new `WindowManagementPolicy::handle_request_window_menu(WindowInfo&, Point, MirInputEvent const*)`, called on the policy thread with the window and the requested position converted from surface-local to global coordinates;
- the default implementation does nothing, so existing shells are unaffected;
- the request is ignored, as for move/resize, if the serial doesn't name an input event the client received.

## How to test

1. A test shell whose policy implements `handle_request_window_menu` and logs or shows a menu.
2. Right-click a GTK 4 window's header bar: the policy is called with that window and the click position.
3. With the default policy: no change from today.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
