# LogindConsoleServices controls seat0's active session, not the compositor's own

Draft for https://github.com/canonical/mir/issues (not filed yet).

## Summary

`LogindConsoleServices` picks its session with `object_path_for_current_session()`,
which reads seat0's `ActiveSession`. That is only the compositor's own
session if it starts in the foreground. A session that restarts its
compositor after a crash (Umoja's session script does, to keep the login
alive) while the user is on another VT, for example a text console used to
kill the hung compositor, makes Mir `TakeControl` of *that* session.

When the user switches back to the graphical VT, the session Mir controls
goes inactive, Mir gets `PauseDevice` for everything and pauses. The
compositor's real session becoming active is never seen, so Mir stays
paused: 0% CPU, black screen, input dead. Only restarting the display
manager recovers.

Seen on Mir 2.28.0: the restarted compositor logged
`Discovered object path for current session = /org/freedesktop/login1/session/_34`
while its own session was 6, then a run of `Received logind force-pause event`
lines and no resume.

## Fix

Prefer the caller's own session: when `XDG_SESSION_ID` is set, resolve it with
`org.freedesktop.login1.Manager.GetSession` and use that path; fall back to
the seat's active session otherwise. (`GetSessionByPID(0)` would work too.)
The initial `active` state already comes from the chosen session's `State`,
and `notify::active` on it then drives `switch_to()` when the user returns.

Patch carried by Umoja: `packaging/mir/patches/3006_logind-own-session.patch`.
