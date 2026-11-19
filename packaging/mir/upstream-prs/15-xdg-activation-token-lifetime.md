# 15: xdg-activation: make the token lifetime longer or configurable

PR title:      shell: make the activation token timeout configurable (or raise it from 3 s)
Umoja patch:   3016_xdg-activation-no-focus-stealing-prevention.patch, part 2 of 3
Applies to canonical/mir main (d8f3edb): yes (as part of 3016)
Depends on:    nothing (textual neighbour of 14)
Kind:          behaviour change; decide with maintainers between a new default and an option
Before submitting:
  - Our patch hard-codes 30 s and updates tests/unit-tests/shell/test_token_authority.cpp. Upstream will likely prefer an option with the current default; offer both. Suggested option name: --xdg-activation-token-timeout (seconds).

---------------------------------------- PR description ----------------------------------------

## What's new?

`TokenAuthority` revokes activation tokens 3 seconds after they are issued. That is too short for the common launcher case: a launcher (dock, app grid, notification) creates a token, starts the app, and the app uses the token when its first window maps. Snap, Flatpak and large apps (browsers, office suites, Electron apps) regularly take longer than 3 s from exec to first window on a cold start, so their window opens behind whatever had focus.

Tokens are still single use, and the change doesn't widen who can obtain one. This PR raises the timeout to 30 seconds, *or* makes it a server option with the current 3 s default (reviewers' choice; both versions are small). The token authority unit test is updated to the new timeout.

## How to test

1. Launch a slow-starting app from a dock or launcher that passes an activation token (e.g. a snap on a cold cache).
2. Before: if the first window takes more than 3 s, it maps unfocused. After: it is activated.
3. `ctest -R TokenAuthority` (unit test updated).

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
