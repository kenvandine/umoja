# 06: wlr-screencopy: capture_output_region's region is output-local

PR title:      wlr-screencopy: treat capture_output_region coordinates as output-local
Umoja patch:   3014_screencopy-region-output-local.patch
Applies to canonical/mir main (d8f3edb): yes, cleanly
Depends on:    nothing
Kind:          bug fix (protocol conformance)

---------------------------------------- PR description ----------------------------------------

## What's new?

`zwlr_screencopy_manager_v1.capture_output_region` gives the region in the output's own logical coordinate space, with 0,0 at the output's top left; clients such as wf-recorder and grim subtract the output's position before sending it. Mir intersected the region with the output's extents in layout coordinates, so on any output not at the layout origin the region was shifted back by the output's position and mostly clipped away.

Example: recording a 3440x1440 monitor placed to the right of a 1920 px wide one with `wf-recorder -g` produced a 1520x1440 video of its left part.

The region is now moved onto the output (`extents.top_left + {x, y}`) before clipping. Output-origin outputs are unaffected.

## How to test

1. Two monitors side by side, the second one not at x = 0.
2. `grim -g "$(slurp)"` or `wf-recorder -g "$(slurp)"` a region on the second monitor.
3. Before: the result is cropped/shifted. After: it matches the selection.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
