# 25: Let the shell keep drawing a closing window so it can animate it

PR title:      scene: let the shell keep a destroyed surface on screen briefly (close animations)
Umoja patch:   3027_surface-linger-hook.patch
Applies to canonical/mir main (d8f3edb): no (its context includes 3018, which we are not upstreaming); and the dlsym hook must become an API
Depends on:    nothing upstream. (In Umoja it sits on 3018; drop the scene-filter interplay for upstream.)
Kind:          new API. Open an issue first: Mir may have animation plans of its own.
Before submitting:
  - Replace mir_surface_linger_hook (called with the scene::Surface, polled each scene) with an explicit API. Proposed: when a window is about to be removed, the policy may return a "linger" handle (e.g. `WindowManagementPolicy::advise_delete_window` gains a sibling `auto keep_drawing_after_delete(WindowInfo const&) -> std::chrono::milliseconds`, or a miral::Window method returning a ref-counted handle; the surface stays drawn until the handle is dropped).
  - Keep the existing semantics: the surface leaves the stack and input immediately, observers are told it is gone, it is drawn on top of its old layer, and alpha/transform changes on it schedule a redraw.

---------------------------------------- PR description ----------------------------------------

Related: #??? (issue: "No way to animate a window closing")

## What's new?

A shell can already animate a window opening or minimizing by changing its scene surface's alpha and transformation. Closing can't be animated: the surface leaves the scene the moment the client destroys it, so there is nothing left to fade or scale.

This lets the shell keep a destroyed surface's last frame on screen for a short time:

- when a surface is removed, the shell may ask to keep drawing it (proposed API above);
- the surface is removed from the stack as usual: no input, observers told it is gone, the client's resources freed as before except for the last buffer;
- it keeps being drawn on top of its former layer until the shell lets it go;
- changes to its alpha and transformation during that time redraw the scene, so the animation runs.

Shells that don't ask see no change.

## How to test

1. A test shell that keeps closing windows for 150 ms and fades their alpha to 0 over that time.
2. Close a window: it fades out instead of vanishing; clicks during the fade go to whatever is underneath.
3. Kill a client mid-fade and close many windows at once: no leaks (surfaces released after the fade), no crash.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
