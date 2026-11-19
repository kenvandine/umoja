# 13: Don't force-probe connectors that i915/xe already report as disconnected

PR title:      kms: avoid forced connector probes on i915/xe for connectors known to be disconnected
Umoja patch:   3021_drm-connector-no-reprobe.patch
Applies to canonical/mir main (d8f3edb): yes, cleanly
Depends on:    nothing
Kind:          startup performance; probably RFC (a driver name allowlist is a judgement call)
Before submitting:
  - Rename the escape hatch to fit Mir's option style if asked (MIR_DRM_ALWAYS_PROBE=1 today; could be a --kms-always-probe-connectors option).
  - Maintainers may prefer fewer get_connector() calls during startup instead (the four repeated queries are the multiplier). Worth offering both in the discussion.
  - Atomic-KMS is being reworked on main; check where connector queries happen now before rebasing.

---------------------------------------- PR description ----------------------------------------

## What's new?

`drmModeGetConnector()` makes the kernel probe the connector again. On an Intel DisplayPort/USB-C port with nothing plugged in, that waits for AUX timeouts: 0.68 s per port on a ThinkPad P16 Gen 3, which has two such ports. Mir queries every connector four times while it starts, so 5.5 s of the 6.6 s between exec and "ready" went to probing two empty ports, with the screen black. That is every login and the greeter at boot.

i915 and xe run detection themselves on a hotplug interrupt, before they send the uevent. So on those drivers, when the kernel's cached state (`drmModeGetConnectorCurrent()`) says a connector is disconnected, Mir now uses that instead of forcing a probe. Everything else is unchanged:

- a connector reported connected (or unknown) is still probed: that is quick, and its mode list is only refreshed by a probe;
- other drivers are always probed. An earlier version applied to all drivers, and nvidia-drm went on reporting an unplugged monitor as connected until asked to probe;
- `MIR_DRM_ALWAYS_PROBE=1` restores the probe on every query.

Result on the P16: login to desktop went from 7.6 s to about 1.3 s.

## How to test

1. On an Intel laptop with empty DisplayPort/USB-C ports, time compositor startup (`journalctl` timestamps, or `MIR_SERVER_*` debug logs around display configuration).
2. Plug and unplug a monitor on an Intel port and on any non-Intel GPU port: both are still detected.
3. Set `MIR_DRM_ALWAYS_PROBE=1` and confirm the old timing.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added
