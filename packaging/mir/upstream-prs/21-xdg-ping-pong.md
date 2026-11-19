# 21: Implement xdg_wm_base ping/pong so shells can detect hung apps

PR title:      xdg-shell: let the shell ping clients and hear their pongs (unresponsive app detection)
Umoja patch:   3025_xdg-window-menu-and-ping.patch, part 2 of 2
Applies to canonical/mir main (d8f3edb): no; dlsym hooks must become an API, needs rebase
Depends on:    nothing (textual neighbour of 20 in xdg_shell_stable.cpp)
Kind:          missing protocol request (pong is a `// TODO`) + new API. Open an issue first; there are a few possible designs.
Before submitting:
  - Replace mir_xdg_ping_setup_hook / mir_xdg_pong_hook. Two options to offer upstream:
      (a) Mir does the bookkeeping: `miral::WindowManagerTools::ping_application(Application)` plus a policy advise `advise_application_unresponsive(ApplicationInfo&, bool)` fired when a ping goes unanswered for N ms and again when it answers.
      (b) Thin API: `ping_application()` and `advise_pong(ApplicationInfo&)`; the shell keeps the timers.
    (a) is closer to what mutter offers and keeps the timeout policy in one place; (b) is what our patch does.
  - Pings must be sent on the Wayland thread; our patch already marshals that.

---------------------------------------- PR description ----------------------------------------

Related: #??? (issue: "No way to detect an unresponsive Wayland client")

## What's new?

Mir binds `xdg_wm_base` but never sends `ping`, and `pong` is a `// TODO`. Shells therefore can't offer the usual "<App> is not responding: Wait / Force Quit" dialog that GNOME and KDE users expect when an app hangs.

This adds:

- `miral::WindowManagerTools::ping_application(Application const&)`: sends `xdg_wm_base.ping` on every `xdg_wm_base` the application has bound, with a fresh serial, on the Wayland thread;
- `WindowManagementPolicy::advise_application_unresponsive(ApplicationInfo const&, bool unresponsive)` (empty default): Mir calls it with `true` if a ping isn't answered within a timeout, and with `false` when the client answers later. (Or the thinner `advise_pong` variant; see the issue.)

Nothing is pinged unless the shell asks, so existing shells see no change and no extra traffic.

## How to test

1. A test shell that pings the focused application when the user clicks its window, and logs the advise calls.
2. `kill -STOP` a GTK app, click its window: after the timeout the policy hears `unresponsive = true`.
3. `kill -CONT` it: the policy hears `false`.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
