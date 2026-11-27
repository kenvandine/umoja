# Explicit sync: a frame still waiting for its fence is dropped by the next commit, so GPU-bound MAILBOX clients never update

Draft for https://github.com/canonical/mir/issues (not filed yet).
Umoja carries the fix as `patches/3029_fenced-commits-in-order.patch`.

## Summary

`WlSurface::commit()` with a `wp_linux_drm_syncobj_surface_v1` acquire point
waits for the fence before applying the commit. If another commit arrives
before that fence signals, the waiting one is dropped ("Any previously
committed, but not yet ready, buffer is now superceded"): its buffer is
released unused and only the newer commit is kept, waiting on its own fence.

A client that commits faster than its GPU finishes always has the next frame
committed before the previous fence signals. Vulkan `MAILBOX` (or
`IMMEDIATE`) while GPU bound does exactly that: the CPU records frame N+1 and
presents it while the GPU is still drawing frame N. Every frame is dropped,
the surface keeps showing an old buffer, and the client keeps running at full
speed because each dropped buffer is handed back at once.

Still present on `main` (checked 2026-11-27).

## What we saw (Mir 2.28.0, Ubuntu 26.10, Mesa 26.2 RADV, Radeon 8060S)

* A Bevy game presenting with `MAILBOX` kept the GPU 97–99% busy, but its
  window stopped updating: clicks in its menu took effect, yet the screen
  only changed when keyboard focus moved to another window (the game then
  slowed to 60 fps, leaving a gap in which a fence signalled before the next
  commit).
* The same game with `FIFO` was fine: Mesa waits for the frame callback,
  which Mir only sends once a buffer is used, so a commit never arrives
  before the previous one is ready.

## Reproduction

1. Run any Vulkan client that uses `MAILBOX` and is GPU bound (a heavy
   fragment shader, or a game at a high resolution), on a driver that uses
   `linux-drm-syncobj-v1` (Mesa RADV/ANV).
2. The window shows its first frames, then freezes while the client keeps
   drawing.

## Expected

Commits are applied in order as their fences signal, as other compositors do
(mutter's transactions, wlroots' `wlr_surface_lock_pending`/`unlock_cached`).
A waiting frame can be dropped once a newer frame is ready, never while the
newest one is still waiting.

## Related

* canonical/mir#5460: fenced commits do not snapshot `pending`, so requests
  sent after `commit()` fold into the deferred commit. The fix above
  snapshots each commit's state, which covers that half.
* `WlSurfaceState::update_from()` does not merge `surface_spec` or
  `opaque_region`, so a synchronized subsurface's cached commits lose them.
