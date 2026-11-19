# 18: One Num Lock for the whole seat

PR title:      input: share Num Lock state across all keyboards of the seat
Umoja patch:   3023_seat-num-lock.patch, part 1 of 2 (the seat-wide state)
Applies to canonical/mir main (d8f3edb): no, needs rebase (xkb_mapper_registrar changed)
Depends on:    nothing
Kind:          behaviour change (matches mutter); may be combined with 19 if maintainers prefer one PR
Before submitting:
  - In our patch the seat-wide state only switches on when the server's dlsym hook supplies an initial value. For upstream, split it: this PR makes the state seat-wide (always, or behind an option if maintainers want the old per-keyboard behaviour kept), and 19 adds the API.
  - Add unit tests in tests/unit-tests/input/test_xkb_mapper*.cpp: toggle on keyboard A, expect keyboard B's modifier state and LED to follow; hot-plug keyboard C, expect it to start with the seat's state.

---------------------------------------- PR description ----------------------------------------

## What's new?

`XKBMapperRegistrar` keeps lock state per keyboard. With a laptop keyboard and a USB keyboard, Num Lock can be on for one and off for the other, the LED on one keyboard doesn't reflect the state that applies when typing on the other, and a keyboard plugged in later always starts with Num Lock off.

mutter (and users' expectations) treat Num Lock as a property of the seat. With this change:

- the registrar keeps one Num Lock state for the seat;
- every keyboard, including ones plugged in later, follows it;
- toggling Num Lock on any keyboard toggles it, and its LED, on all of them.

Other modifiers and Caps Lock are unchanged. Private members of an internal class only; no ABI change.

## How to test

1. Attach two keyboards. Press Num Lock on one.
2. Before: only that keyboard's LED and keypad change. After: both LEDs light, and the other keyboard's keypad types digits too.
3. Plug in a third keyboard while Num Lock is on: its LED lights and it types digits.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
