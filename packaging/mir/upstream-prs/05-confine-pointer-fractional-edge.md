# 05: Confine the pointer at the top and left edges too

PR title:      input: round down, not towards zero, when confining the cursor to the outputs
Umoja patch:   3013_confine-pointer-fractional-edge.patch
Applies to canonical/mir main (d8f3edb): yes, cleanly
Depends on:    nothing
Kind:          bug fix (one line)
Before submitting:
  - A unit test in tests/unit-tests/input (move the pointer by -0.5 from the origin, expect it at 0,0) would be easy to add.

---------------------------------------- PR description ----------------------------------------

## What's new?

`SeatInputDeviceTracker::confine_pointer()` turns the float cursor position into an integer `Point`, which truncates towards zero. A position between -1 and 0, which relative motion pushing past the top or left edge often leaves, becomes 0, looks inside the screen and is never clamped. Clients then get pointer events at a negative surface position.

The visible effect: with the pointer pushed into the top-left or top-right corner, a click misses a panel button that sits right in the corner (an "Activities" or system menu button), which is exactly where users aim with "infinite edges".

The position is now rounded down with `std::floor` before confining, so anything left of or above the origin counts as outside and gets clamped.

## How to test

1. Run a layer-shell panel at the top of the screen with a button in the top-left corner.
2. Slam the pointer into the top-left corner and click.
3. Before: the click sometimes goes nowhere (the client sees y < 0). After: it always hits the button.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
