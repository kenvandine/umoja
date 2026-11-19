# 28: Don't scan out a client buffer that comes from another GPU

PR title:      compositor: don't use direct scanout for buffers rendered on a different GPU
Umoja patch:   3011_direct-scanout-switch.patch (rework needed)
Applies to canonical/mir main (d8f3edb): no; direct scanout / overlay code on main is being reworked (atomic-kms mixed proposals)
Depends on:    nothing in code; same hybrid-GPU issue as 26, so file them together
Kind:          bug fix. Our patch is a workaround (MIR_DIRECT_SCANOUT=0 disables direct scanout everywhere); upstream should get the targeted fix.
Before submitting:
  - Rework: in the framebuffer adaptor / buffer_to_framebuffer path, refuse to scan out a buffer whose dma-buf was imported on a different device than the output's (or that 26 copied), so composition is used for those only. Keep a global off switch only if maintainers want one for debugging.
  - Re-test on main first: atomic-kms now builds and validates overlay proposals with TEST_ONLY commits, which might already reject these buffers.

---------------------------------------- PR description ----------------------------------------

Related: #??? (hybrid GPU issue, see PR 26)

## What's new?

When one client buffer covers an output, the compositor hands it straight to the display (direct scanout). On Intel+NVIDIA laptops, a fullscreen game on a monitor wired to the NVIDIA GPU has its buffer rendered on the Intel GPU (or copied, see #???). Scanning that buffer out on the NVIDIA output showed a white screen and stalled the output until something (Alt+Tab) forced composition again.

Direct scanout is now skipped for buffers that don't belong to the GPU driving the output; they are composited instead. Same-GPU fullscreen clients keep direct scanout.

## How to test

1. Intel+NVIDIA laptop, monitor on the NVIDIA GPU.
2. Run a fullscreen game or `glmark2-wayland --fullscreen` on that monitor, rendering on the Intel GPU.
3. Before: white screen / stall. After: the game is shown (composited).
4. A fullscreen client on the Intel-driven internal panel still gets direct scanout.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
