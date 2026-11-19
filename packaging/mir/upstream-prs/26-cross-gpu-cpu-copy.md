# 26: Fall back to a CPU copy for dma-bufs that render wrong on another GPU

PR title:      platform: CPU-copy fallback for client dma-bufs drawn on a different GPU
Umoja patch:   3009_cross-gpu-cpu-copy.patch, plus from 3020: buffer pool never destroyed (exit segfault), spare buffers released on size change
Applies to canonical/mir main (d8f3edb): no, needs rebase (linux_dmabuf.cpp has moved on, e.g. 03db75f "use dma buf bound to the FBO")
Depends on:    nothing
Kind:          hybrid-GPU correctness fix, sizeable (~300 lines). OPEN AN ISSUE FIRST with the hardware details.
Before submitting:
  - Re-test on main first. Mir's existing cross-GPU path in DMABufEGLProvider::as_texture (import directly with explicit modifiers, else blit on the importing GPU) is what showed stripes on the NVIDIA-driven monitor. If main still shows them, the issue should ask whether the real bug is in that path (modifier choice, missing implicit sync) before offering a CPU copy.
  - Remove the Umoja debug switch ($XDG_RUNTIME_DIR/umoja-debug-windows); use mir::log_debug / a report object for the 10 s statistics, or drop them.
  - MIR_CROSS_GPU_CPU_COPY=0 → a Mir option if they keep an escape hatch.
  - Fold in the two fixes from 3020 listed above; they fix bugs in this code, not the optimisation in 27.

---------------------------------------- PR description ----------------------------------------

Closes #??? (issue: "Corrupted GL/Vulkan windows on a monitor driven by the NVIDIA GPU of an Intel+NVIDIA laptop")

## What's new?

On Intel+NVIDIA hybrid laptops where a monitor is wired to the NVIDIA GPU (ThinkPad P16 Gen 3, rear USB-C/DisplayPort), Mir composites that monitor on NVIDIA. Windows drawn by GL or Vulkan clients, on either GPU, showed horizontal stripes of wrong content there all the time. `wl_shm` windows were fine, which points at the cross-GPU dma-buf import.

When a client dma-buf has to be drawn on a GPU other than the one that imported it, Mir now reads it back once per commit through the importing GPU (where it renders correctly) and uploads it like a `wl_shm` buffer on the rendering GPU. Same-GPU buffers are untouched.

- Copy buffers are pooled and reused once nothing holds them, so a window redrawing on that monitor doesn't zero and fault in a new multi-megabyte buffer, and allocate a new texture, every frame. Spare buffers are released when a new size is needed rather than accumulating.
- The pool is deliberately never destroyed: at `exit()` its destructor ran after the platform modules were unloaded, and releasing a texture then called into unmapped code (a segfault at every shutdown).
- The copy can be turned off (`MIR_CROSS_GPU_CPU_COPY=0`, or an option) to get the direct import back.

Cost: on a P16 Gen 3 the read-back took about 5.5 ms per 6 MB buffer on the compositor thread; 27 makes this much cheaper for linear buffers.

## How to test

1. Intel+NVIDIA laptop (BIOS in hybrid mode), external monitor on a port wired to the NVIDIA GPU.
2. Run a GL client (glmark2-wayland, Firefox, a GTK 4 app using GL) on that monitor.
3. Before: striped/garbled content. After: correct content. Monitors on the Intel GPU unchanged.
4. Log out / stop the compositor: no crash on exit.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
