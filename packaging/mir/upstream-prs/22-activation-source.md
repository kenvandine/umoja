# 22: Tell the window manager whether an activation came from the user

PR title:      miral: distinguish user-requested activations (dock, valid xdg-activation token) and report refused ones
Umoja patch:   3026_activation-hooks.patch
Applies to canonical/mir main (d8f3edb): applies textually, but the dlsym hooks must become a MirAL API
Depends on:    nothing; touches xdg_activation_v1.cpp like 14-16, so rebase on whichever lands first
Kind:          new API. Open an issue first.
Before submitting:
  - Replace mir_user_activation_hook / mir_activation_refused_hook. Proposed: pass a reason to the existing `handle_activate_window` path, e.g. a new `handle_request_activate(WindowInfo&, ActivationReason)` where ActivationReason is {client_raise, xdg_activation_token, foreign_toplevel_activate}, with a default implementation that calls handle_activate_window() as today. And `advise_activation_refused(WindowInfo const&)` (empty default) for an xdg-activation request with an invalid token, so the shell can flag the window as wanting attention.
  - BasicWindowManager::handle_raise_surface currently drops raises older than the last input event; keep that.

---------------------------------------- PR description ----------------------------------------

Related: #??? (issue: "Window management can't tell a user-requested activation from an app raising itself")

## What's new?

`shell->raise_surface()` reaches the policy as `handle_activate_window()` the same way whether

- a taskbar/dock activated the window through `zwlr_foreign_toplevel_handle_v1.activate`,
- a launcher's valid xdg-activation token raised it, or
- the client raised itself.

Shells with workspaces need to tell these apart. GNOME's behaviour: a user-requested activation switches to the window's workspace and focuses it; an app that raises itself without a valid token does not steal focus or move the user, and instead gets marked "is ready" / demands attention. Today a Mir shell either lets a chat app pull the user to another workspace whenever it raises itself, or ignores the dock too.

This adds:

- an activation reason passed to the policy (`ActivationReason::foreign_toplevel`, `xdg_activation`, `client`), with a default that keeps calling `handle_activate_window()` so existing shells behave as before;
- `advise_activation_refused(WindowInfo const&)`, called when an `xdg_activation_v1.activate` request carries an invalid or expired token, so the shell can show an attention hint instead of silently doing nothing.

## How to test

1. A test shell that logs the reason for each activation.
2. Activate a window from a foreign-toplevel taskbar: `foreign_toplevel`. Launch an app from a launcher that passes a token: `xdg_activation`. Call `xdg_activation_v1.activate` with a bogus token: `advise_activation_refused`.
3. The default policies (minimal, floating) behave exactly as before.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
