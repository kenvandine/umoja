# 02: Control the compositor's own logind session, not seat0's active one

PR title:      console: take control of our own logind session (XDG_SESSION_ID) rather than the seat's active one
Umoja patch:   3006_logind-own-session.patch
Applies to canonical/mir main (d8f3edb): no, small rebase (include block changed)
Depends on:    nothing
Kind:          bug fix
Before submitting:
  - File packaging/mir/upstream-bugs/logind-active-session-not-own.md as an issue first.
  - Maintainers may prefer GetSessionByPID(0) over XDG_SESSION_ID; both are mentioned below so either is an easy change.

---------------------------------------- PR description ----------------------------------------

Closes #???

## What's new?

`LogindConsoleServices` picks its session with `object_path_for_current_session()`, which reads seat0's `ActiveSession`. That is the compositor's own session only when it starts in the foreground.

A session that restarts its compositor after a crash while the user is on another VT (for example a text console used to kill the hung compositor) makes Mir `TakeControl` of that other session. When the user switches back, the session Mir controls goes inactive, Mir is force-paused, and the compositor's real session becoming active is never seen: black screen, dead input, only restarting the display manager recovers. On 2.28 the restarted compositor logged `Discovered object path for current session = .../session/_34` while its own session was 6.

When `XDG_SESSION_ID` is set, Mir now resolves it with `org.freedesktop.login1.Manager.GetSession` and uses that path. If it is unset or logind doesn't know it, Mir logs a warning and falls back to the seat's active session as before. The initial `active` state already comes from the chosen session's `State`, and its `notify::active` drives `switch_to()` when the user comes back. (`GetSessionByPID(0)` would work as well if that is preferred.)

## How to test

1. Log in to a Mir-based session on VT 2.
2. Switch to a text console (VT 3), log in there, and restart the compositor in the graphical session (e.g. `systemctl --user restart <compositor unit>`, or kill it and let the session restart it).
3. Switch back to VT 2.
4. Before: the screen stays black and the log shows Mir controlling the VT 3 session. After: the desktop resumes, and the debug log names the session from `XDG_SESSION_ID`.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
