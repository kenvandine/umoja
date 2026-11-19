# 09: Scale the client cursor hotspot to buffer pixels

PR title:      wl_pointer: convert a surface cursor's hotspot to buffer pixels on scaled outputs
Umoja patch:   3012_cursor-surface-offset-and-hidden-role.patch, part 2 of 3
Applies to canonical/mir main (d8f3edb): no, needs rebase
Depends on:    08 textually (same functions); logically independent. Submit after 08 or rebase on whichever lands first.
Kind:          bug fix
Before submitting:
  - This is a partial fix: the image is still drawn at buffer size (the existing TODO about discarding the surface's scale stays). Say so, or go further and draw the cursor at logical size if maintainers prefer.

---------------------------------------- PR description ----------------------------------------

## What's new?

A client gives the cursor hotspot in surface (logical) coordinates, but `WlSurfaceCursor::apply_latest_buffer()` handed it unchanged to `BufferCursorImage`, which works in buffer pixels. With `buffer_scale 2`, or a fractional scale through `wp_viewport`, the image sat off the real pointer position, by half the hotspot at 2x.

The hotspot is now converted using the ratio of the buffer's pixel size to the surface's logical size before building the cursor image. Unscaled cursors are unchanged.

This does not yet draw the cursor at its logical size (the existing TODO about discarding scale remains); it only makes the hotspot line up with the image that is drawn.

## How to test

1. Set an output scale of 2 (or 1.5).
2. Use a client that draws its own cursor at scale (GTK 4 does), e.g. a text view with the I-beam.
3. Before: clicks land away from the visible cursor tip. After: they land at the tip.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
