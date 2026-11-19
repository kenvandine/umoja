# 01: Fix use-after-free when a surface dies with an explicit-sync commit pending

PR title:      frontend: remove a pending explicit-sync commit when its WlSurface is destroyed
Umoja patch:   3005_surface-destroy-pending-syncobj-commit.patch
Applies to canonical/mir main (d8f3edb): no, small rebase (destructor now starts with archetype->make_inert())
Depends on:    nothing
Kind:          bug fix (crash/hang), still present on main
Before submitting:
  - File packaging/mir/upstream-bugs/syncobj-commit-after-surface-destroy.md as an issue and put its number in "Closes".
  - Drop the Umoja-specific wording from the comment (no "umoja-panel").
  - A unit or WLCS test would help: destroy a surface while its commit waits on an unsignalled acquire point, then signal it.

---------------------------------------- PR description ----------------------------------------

Closes #???

## What's new?

`WlSurface::commit()` with a `wp_linux_drm_syncobj_surface_v1` acquire point adds an fd source to the Wayland event loop whose data is `pending_context`. `WlSurface::~WlSurface()` never removed that source. If the client disconnects (crashes, or is killed for a protocol error) while a commit is still waiting for its acquire fence, the surface and its pending context are freed but the source stays registered. When the fence later signals, `handle_buffer_ready` runs `complete_commit()` on freed memory.

In practice we saw the `Mir/Wayland` thread spin at 100% forever inside `complete_commit()` (an inlined `std::advance` over a freed `std::list`), and at the next VT switch the main loop blocked for good in `WaylandConnector::stop()` joining that thread: frozen screen, no input.

The destructor now does what `commit()` already does for a superseded pending buffer: remove the event source, signal the release point so the client may reuse the buffer, and drop the pending context.

## How to test

1. Run a client that uses linux-drm-syncobj (GTK 4.16+ with explicit sync, or a small test client) and commit a buffer whose acquire point is not yet signalled.
2. Kill the client before the acquire point signals.
3. Without this change the compositor later spins in `complete_commit()` (or crashes under ASan with a heap-use-after-free). With it, nothing happens.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
