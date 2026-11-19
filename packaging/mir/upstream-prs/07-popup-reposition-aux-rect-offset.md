# 07: Keep the layer surface margin offset when an xdg_popup is repositioned

PR title:      xdg-shell: apply aux_rect_offset in xdg_popup.reposition like on create
Umoja patch:   3003_popup-reposition-aux-rect-offset.patch
Applies to canonical/mir main (d8f3edb): yes, cleanly
Depends on:    nothing
Kind:          bug fix

---------------------------------------- PR description ----------------------------------------

## What's new?

`XdgPopupStable::reposition()` takes the new positioner's anchor rect as is, but the create path (`set_aux_rect_offset_now`) moves it by `aux_rect_offset`, the offset between the parent's window geometry and its surface (a layer surface's margin, for one). So a popup that is repositioned lands that many pixels off.

GTK repositions tooltips right after mapping them, so on a layer surface with a top margin every tooltip covered the widget it belonged to and swallowed the click meant for it.

`reposition()` now applies the same offset to the anchor rect before handing the spec to the shell (or to `apply_spec` before the popup is mapped).

## How to test

1. A GTK 4 layer-shell client anchored to the top with a non-zero top margin, with a button that has a tooltip.
2. Hover the button.
3. Before: the tooltip appears `margin` pixels too high, over the button. After: it is placed where GTK asked, as on first map.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
