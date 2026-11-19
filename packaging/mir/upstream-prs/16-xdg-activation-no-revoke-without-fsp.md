# 16: xdg-activation: let shells turn off token revocation on key press and focus change

PR title:      xdg-activation: don't revoke pending tokens on key press/focus change unless focus stealing prevention is on
Umoja patch:   3016_xdg-activation-no-focus-stealing-prevention.patch, part 3 of 3
Applies to canonical/mir main (d8f3edb): yes (as part of 3016); but the upstreamable form needs new plumbing
Depends on:    nothing; easiest after 14 and 15
Kind:          policy option. OPEN AN ISSUE FIRST: our patch just removes the revocation, which upstream won't take as is.
Before submitting:
  - MirAL already has miral::FocusStealing (allow / prevent) on MinimalWindowManager and FloatingWindowManager, but the xdg-activation frontend can't see it. Propose plumbing a "revoke tokens on user input" setting from the shell to XdgActivationV1 (a miral option, or derive it from FocusStealing), defaulting to today's behaviour.

---------------------------------------- PR description ----------------------------------------

Related: #??? (issue: "xdg-activation always applies focus stealing prevention, even with FocusStealing::allow")

## What's new?

Mir's xdg-activation revokes *every* pending token whenever a key is pressed, and when keyboard focus moves to another client (or to nothing). That is GNOME-style focus stealing prevention, and it is built into the frontend regardless of the shell's window management policy, even though MirAL lets a shell choose `FocusStealing::allow` for its window manager.

For shells that don't want focus stealing prevention, the effect is that the window a click asked for is left in the background whenever the user types anything while the app starts up (e.g. clicks a launcher, then keeps typing in a terminal), or when focus changes in the meantime.

This PR makes the revocation conditional: `XdgActivationV1` gets a setting (default: revoke, as today) that the shell can turn off. Tokens still expire and are still single use. With the setting off, key presses and focus changes no longer revoke pending tokens.

Open question for reviewers: should this follow the window manager's existing `FocusStealing` choice automatically, or be its own option?

## How to test

1. Start a slow app from a launcher, then immediately type in another window.
2. Default setting: the app's window maps unfocused (unchanged). Setting off: it is activated.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
