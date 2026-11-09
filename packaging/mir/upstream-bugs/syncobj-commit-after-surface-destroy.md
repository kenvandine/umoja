# Use-after-free: pending linux-drm-syncobj commit outlives its WlSurface

Draft for https://github.com/canonical/mir/issues (not filed yet).

## Summary

`WlSurface::commit()` with a `wp_linux_drm_syncobj_surface_v1` acquire point
adds an fd source to the Wayland event loop (`wl_event_loop_add_fd`) whose
`data` is `pending_context.get()`. `WlSurface::~WlSurface()` never removes
that source. If the client disconnects (or is killed for a protocol error)
while a commit is still waiting for its acquire fence, the surface and
`pending_context` are freed but the source stays registered. When the fence
signals, `handle_buffer_ready` calls `complete_commit(state.surf)` on freed
memory.

Still present on `main` (checked 2026-11-09).

## What we saw (Mir 2.28.0, Ubuntu 26.10, amdgpu/radeonsi, GTK 4.24 clients using explicit sync)

* A layer-shell client was disconnected three times in two seconds with
  protocol errors (`validate_modification_request ... width must be
  positive`, committing while no output was powered on).
* The `Mir/Wayland` thread then spun at 100% CPU in
  `WlSurface::complete_commit()`, in the inlined `std::advance` of
  `surf->children.assign(pending_surface_order->begin(), ->end())`: the
  `std::optional<std::list<WlSubsurface*>>` was freed heap (list head
  pointing to itself, garbage size ~1.8e19), so it never finished.
* `surf` was a freed malloc chunk (vtable slot 0, chunk size 0x91, free-list
  pointers).
* At the next VT switch logind force-paused the session; the main loop ran
  the pause handler, `WaylandConnector::stop()`, and blocked forever in
  `std::thread::join()` on the spinning Wayland thread. Screen frozen, no
  input, only killing the compositor recovers.

Backtrace of the spinning thread:

```
#0 mir::frontend::WlSurface::complete_commit(WlSurface*)
#1 WlSurface::commit()::{lambda(int, unsigned int, void*)#1}::_FUN   (handle_buffer_ready)
#2 wl_event_loop_dispatch
#3 wl_display_run
```

Main thread:

```
#6 std::thread::join()
#7 mir::frontend::WaylandConnector::stop()
#8 DisplayServer::Private::Private(...)::{lambda()#2}   (pause handler)
#9 mir::LogindConsoleServices::on_state_change
...
#28 mir::GLibMainLoop::run()
```

## Reproducer (sketch)

1. Client binds `wp_linux_drm_syncobj_manager_v1`, attaches a buffer with an
   acquire point that is not yet signalled, commits.
2. Client disconnects (or triggers a protocol error) before signalling.
3. Signal the acquire point (or let the GPU do it).
4. The compositor runs `complete_commit` on the destroyed surface.

## Fix

Remove the source in the destructor (and release the client's buffer):

```cpp
mf::WlSurface::~WlSurface()
{
    if (buffer_ready_source)
    {
        wl_event_source_remove(buffer_ready_source);
        buffer_ready_source = nullptr;
        if (pending_context)
        {
            pending_context->release.signal();
            pending_context.reset();
        }
    }
    ...
```

Patch carried by Umoja: `packaging/mir/patches/3005_surface-destroy-pending-syncobj-commit.patch`.
Separately, `WaylandConnector::stop()` joining with no timeout turns any
Wayland-thread hang into a frozen session at the next VT switch.
