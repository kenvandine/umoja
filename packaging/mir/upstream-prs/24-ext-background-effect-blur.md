# 24: Implement ext-background-effect-v1 (background blur)

PR title:      Implement ext-background-effect-v1: blur behind surfaces in the GL renderer
Umoja patch:   3004_background-blur.patch (plus the bounded GL error drain folded into it)
Applies to canonical/mir main (d8f3edb): no, needs a real port (renderer.cpp and frontend CMakeLists changed)
Depends on:    nothing
Kind:          new protocol + renderer feature, ~1,300 lines. Open an issue first to agree on the renderer integration.
Must land together: the protocol frontend and the renderer support are useless apart. Keep them as two commits in one PR
  (1) renderer: blur regions in the GL renderer (mir::graphics::background_blur)
  (2) frontend: ext_background_effect_manager_v1
Before submitting:
  - Use the protocol XML from wayland-protocols (staging/ext-background-effect) unchanged, and check the version Mir's build pulls in.
  - The region table is exported from libmirplatform in a private version node (MIR_PLATFORM_2.28_umoja). Upstream it needs a proper MIR_PLATFORM_x.y node and must pass .github/workflows/symbols-check.yml; better still, carry the blur region on the Renderable (a new virtual with a default) instead of a global table keyed by buffer stream id. Ask which they prefer.
  - MIR_BLUR_PASSES / MIR_BLUR_OFFSET env vars → Mir configuration options (or drop them and pick good defaults).
  - New files carry "Copyright © Umoja developers": use Mir's standard header (check the CLA/copyright expectation with maintainers).
  - Mir now has miral::CustomRenderer; maintainers might suggest a shell does blur in its own renderer. Point out that the protocol is generic and GTK 4.24+ already uses it (CSS backdrop-filter), so every Mir shell benefits.
  - Measure cost on a low-end iGPU and include numbers.

---------------------------------------- PR description ----------------------------------------

Related: #??? (issue: "Support ext-background-effect-v1")

## What's new?

`ext-background-effect-v1` (wayland-protocols, staging) lets a client ask the compositor to blur whatever is behind a region of its surface: frosted panels, docks, popovers, notification bubbles. GTK 4.24 uses it for CSS `backdrop-filter`; KWin and others implement it. Mir currently doesn't, so translucent shell surfaces either look muddy or have to fake it.

This PR adds:

**Renderer.** The GL renderer can blur behind a renderable. Before drawing a renderable that has a blur region, it grabs the region's bounding box from the frame being composed (`glBlitFramebuffer`; `glCopyTexSubImage2D` on GLES2), blurs it with dual-Kawase down/up passes at reduced resolution, and draws the result back masked by the surface's own alpha (smoothstep), so rounded corners and soft shadows inside the region stay crisp rather than getting a blurred halo. The number of passes and the sample offset are configurable (defaults: 3 passes, offset 2.0). Renderables without a region cost nothing extra. The GL error drain after the blur passes is bounded, so a lost context (which reports `GL_CONTEXT_LOST` forever) can't hang the renderer.

**Frontend.** `ext_background_effect_manager_v1` advertises the `blur` capability; `ext_background_effect_surface_v1.set_blur_region` sets the region (surface-local, a `wl_region`, or none to clear) and hands it to the renderer keyed by the surface's buffer stream. Destroying the effect object or the surface clears it, and using it after the surface is gone raises `surface_destroyed` as the protocol requires. Like other extensions, the shell has to enable it (`ext_background_effect_manager_v1`).

`mir::renderer::gl::Renderer` gains only a private class and a private non-virtual method, so its ABI is unchanged.

Known difference from the protocol text: the region applies when it is set rather than on the next commit. Clients set it and commit together, so this isn't visible; double-buffering it properly is a follow-up (or can be done here if reviewers prefer).

## How to test

1. Enable the extension in a test shell (`--add-wayland-extensions=ext_background_effect_manager_v1`, or miral::WaylandExtensions).
2. Run a GTK 4.24+ app with a translucent widget styled `backdrop-filter: blur(12px)`, or a layer-shell panel that sets a blur region, over a busy wallpaper.
3. The area behind the region is blurred; corners and shadows of the surface are not; moving windows underneath updates the blur each frame.
4. Without the extension enabled, nothing changes.

Screenshots/video: (attach before/after of a translucent panel and a popover).

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
- [ ] (optional) Added Screenshots or videos
