# 08: Honour wl_surface.offset on cursor surfaces

PR title:      wl_pointer: move the cursor hotspot by wl_surface.offset (and attach x/y)
Umoja patch:   3012_cursor-surface-offset-and-hidden-role.patch, part 1 of 3
Applies to canonical/mir main (d8f3edb): no, needs rebase (wl_pointer.cpp changed; split out of a larger patch)
Depends on:    nothing
Kind:          bug fix / missing protocol feature
Before submitting:
  - Split 3012 into three: (1) this, (2) PR 09 hotspot scaling, (3) the hidden-cursor role fix, which DROP: upstream main already sets the role in WlHiddenCursor and resets the old cursor first.
  - WLCS has pointer tests; a test that commits a cursor buffer with an offset and checks the hotspot would be ideal.

---------------------------------------- PR description ----------------------------------------

## What's new?

When a client changes its cursor image it may attach the new buffer to the same cursor surface and move the hotspot with `wl_surface.offset` (or the `x, y` of `wl_surface.attach` before version 5), without calling `wl_pointer.set_cursor` again. GTK does exactly this when switching from the arrow to the I-beam. Mir logged "Client requested unimplemented non-zero offset" and ignored it, so the I-beam was drawn at the arrow's hotspot and a text selection didn't start under the visible cursor.

- `WlSurfaceState` gains `buffer_offset`, set by `wl_surface.offset` and non-zero `attach` offsets (accumulated across cached commits of synchronized subsurfaces).
- The cursor role passes the committed offset to `CommitHandler::on_commit()`, and `WlPointer` subtracts it from the hotspot, as the protocol specifies.
- Other roles still ignore the offset (and no longer log a warning for it on the cursor path).

## How to test

1. Open a GTK 4 terminal (Ptyxis, GNOME Console) or text editor.
2. Move the pointer from the header bar into the text area so the cursor changes to the I-beam, then drag to select.
3. Before: the selection starts a few pixels away from the visible I-beam. After: it starts under it.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
