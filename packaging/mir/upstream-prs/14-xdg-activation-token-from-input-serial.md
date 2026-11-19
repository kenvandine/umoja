# 14: xdg-activation: a real token for a client that names a recent input serial

PR title:      xdg-activation: grant tokens to unfocused clients that prove recent user input
Umoja patch:   3016_xdg-activation-no-focus-stealing-prevention.patch, part 1 of 3
Applies to canonical/mir main (d8f3edb): yes as a whole; split it into three PRs (14, 15, 16)
Depends on:    nothing
Kind:          bug fix (panels/notification daemons can't hand focus on)
Before submitting:
  - Keep only the create_token(from_user_input) change and the serial check in this PR.
  - Remove the "Umoja:" comment prefix.
  - Check the serial is for this seat and recent (event_for() already ties it to the client); maintainers may want it limited to button/key/touch-down events.

---------------------------------------- PR description ----------------------------------------

## What's new?

Mir only hands a real xdg-activation token to the client that currently has keyboard focus; every other client gets a bogus token that won't activate anything. That rules out a whole class of legitimate requesters that never hold keyboard focus: layer-shell panels and docks, and notification daemons living in them. Clicking a notification in the top bar produced a bogus token, so the app it belonged to stayed behind.

The xdg-activation protocol already has a better signal: the client can attach the serial of an input event it received (`xdg_activation_token_v1.set_serial`). If that serial resolves, through `WlClient::event_for()`, to an input event this client was sent, the user is interacting with it right now, and it now gets a real token even without keyboard focus. Clients without a valid serial behave exactly as before.

## How to test

1. Run a layer-shell panel that requests an activation token with the serial of the click it received, then passes the token to an app (`gtk_show_uri`, `g_app_info_launch` with a launch context, or a notification "default" action).
2. Before: the app's window opens or stays behind the focused window. After: it is activated.
3. A client without keyboard focus and with no (or a stale) serial still gets a bogus token.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
