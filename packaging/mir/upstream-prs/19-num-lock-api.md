# 19: Let the shell set Num Lock and hear when it changes

PR title:      miral: API to set the initial Num Lock state and observe changes
Umoja patch:   3023_seat-num-lock.patch, part 2 of 2 (the hooks)
Applies to canonical/mir main (d8f3edb): no; the dlsym hooks must become a MirAL API
Depends on:    18
Kind:          new API. Open an issue or discuss the API shape first.
Before submitting:
  - Replace mir_num_lock_initial_hook / mir_num_lock_changed_hook (dlsym'd from the executable) with a MirAL interface. Proposed shape, to be discussed: a method on miral::InputConfiguration (or miral::Keymap), e.g. `set_num_lock(bool)` usable before and after startup, plus a callback `on_num_lock_changed(std::function<void(bool)>)`.
  - Add the symbols to src/miral/symbols.map under the next MIRAL_x.y node and bump the MirAL minor version per Mir's process.

---------------------------------------- PR description ----------------------------------------

Related: #??? (issue: "Shells can't restore the user's Num Lock state")

## What's new?

Desktop shells remember Num Lock across sessions (GNOME: `org.gnome.desktop.peripherals.keyboard remember-numlock-state` / `numlock-state`), and some users expect it on at login. Mir starts every keyboard with Num Lock off and gives the shell no way to change it, or to learn that the user toggled it so it can be saved.

This adds, on top of the seat-wide Num Lock state from #??? (PR 18):

- `miral::InputConfiguration::set_num_lock(bool)` (name open for discussion): sets the seat's Num Lock. Called before the server runs, it sets the initial state for every keyboard; called later, it toggles it on all keyboards, LEDs included.
- `miral::InputConfiguration::on_num_lock_changed(std::function<void(bool)>)`: called whenever the user toggles Num Lock on any keyboard.

Without either call, behaviour is unchanged.

## How to test

1. A test shell that calls `set_num_lock(true)` at startup: every keyboard starts with Num Lock on (LED lit, keypad types digits).
2. Register `on_num_lock_changed` and press Num Lock: the callback fires with the new state once per toggle.
3. Unit tests in tests/miral cover both.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
