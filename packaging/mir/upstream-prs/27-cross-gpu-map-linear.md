# 27: Cross-GPU copy: map linear dma-bufs instead of reading them back

PR title:      platform: map linear dma-bufs for the cross-GPU copy, and optionally offer only linear modifiers
Umoja patch:   3020_cross-gpu-map-linear.patch (minus the two fixes moved into 26)
Applies to canonical/mir main (d8f3edb): no; on top of 26
Depends on:    26
Kind:          performance
Before submitting:
  - The CPU cache flush uses clflushopt: guard it for x86 and do nothing (or use the DMA_BUF_IOCTL_SYNC path only) elsewhere. Check whether DMA_BUF_IOCTL_SYNC alone is supposed to cover this and say why it doesn't here (i915/xe on a non-LLC iGPU), so reviewers can judge whether it is a kernel/driver bug.
  - MIR_DMABUF_LINEAR_ONLY=1 → a Mir option, and maybe only applied automatically on systems where 26's copy is active.
  - The debug self-check (compare each mapped copy with a GL read) is useful during review but should probably not be merged, or be behind a debug option.

---------------------------------------- PR description ----------------------------------------

Related: #??? (PR 26)

## What's new?

The cross-GPU CPU copy from #??? reads each buffer back with `glReadPixels`. Measured on a ThinkPad P16 Gen 3 with two windows redrawing at about 30 fps on the NVIDIA-driven monitor: 450 MB/s copied and about 4 s of every 10 s in `glReadPixels` (5.5 ms per 6 MB buffer) on the compositing thread.

A linear dma-buf can be mapped and copied with `memcpy` instead, which is several times cheaper (0.27 s per GB vs 0.9 s per GB in our measurements):

- ARGB/XRGB and ABGR/XBGR linear buffers are mapped and copied keeping their byte order; anything else still uses the GL read.
- A mapped read doesn't queue behind the client's rendering on the GPU the way a GL read does, so Mir first waits for the dma-buf's write fence (`poll(POLLIN)` on the dma-buf fd).
- The CPU's cached lines of the mapping are flushed before each read. Without this, the Intel iGPU (no shared last-level cache on recent parts) writes new frames without the CPU noticing, and stale cache lines from the previous read showed as runs of old pixels; with the flush, 0 mismatches in ~5,700 checked buffers.
- Clients only allocate linear buffers when nothing better is on offer, so an option makes Mir advertise just the linear modifier (for formats that have one). It is meant for systems where the cross-GPU copy is in use.

Overall compositor CPU on that machine dropped from about 73% to 38% (together with `__GL_YIELD=USLEEP` for the NVIDIA driver).

## How to test

1. Same setup as #??? (PR 26), with the linear-only option on.
2. Run two GL windows animating on the NVIDIA monitor; compare compositor CPU and frame times with and without this PR.
3. Check for stale-pixel artefacts (runs of old content) while windows animate: none.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
