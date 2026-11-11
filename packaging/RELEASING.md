# Publishing and releases

Maintainer notes for the PPA uploads and cutting a release.

`.github/workflows/packages.yml` builds `.deb`s for amd64 and arm64 on
every PR and uploads signed source packages to two PPAs:

- [ppa:ken-vandine/umoja](https://launchpad.net/~ken-vandine/+archive/ubuntu/umoja)
  gets tagged releases only (`v*` tags, or a manual run with `release`).
- [ppa:ken-vandine/umoja-edge](https://launchpad.net/~ken-vandine/+archive/ubuntu/umoja-edge)
  gets every other push to `main`, versioned as the latest release plus the
  commits since it (`2026.46.3+git4.1a2b3c4d~stonking1`), so edge always
  sorts above the release it builds on. It depends on the umoja PPA, so
  add both (`sudo add-apt-repository ppa:ken-vandine/umoja`, then the same
  for `ppa:ken-vandine/umoja-edge`).

It needs
these repository secrets: `PPA_GPG_PRIVATE_KEY`, `PPA_GPG_PASSPHRASE`,
`PPA_SIGNER_NAME`, `PPA_SIGNER_EMAIL` (the key must be registered with your
Launchpad account).

## Versions

Umoja versions are `vYYYY.WW.REVISION`: the ISO year and week of the
release, then a counter that starts at 1 each week, for example `v2026.46.1`.
Git tags carry the `v`; `debian/changelog` drops it (`2026.46.1`), and
`Cargo.toml` also drops a leading zero in the week (`2027.5.1` for
`v2027.05.1`) because Cargo versions are semver.

To cut a release:

```sh
packaging/bump-version --tag "What changed in this release."
git push origin main v2026.46.2      # the tag name it printed
```

Without `--tag` it only edits `debian/changelog`, `Cargo.toml` and
`Cargo.lock`. The PPA job refuses a tag that doesn't match
`debian/changelog`. Instead of pushing a tag you can also run the Packages
workflow by hand on `main` with **release** ticked: it uploads the
`debian/changelog` version as a release and pushes the `v` tag itself.

Snapshots from `main` are versioned `<release>+git<date>.<sha>` once that
release is tagged, and `<release>~git<date>.<sha>` before, so a release
always sorts above the snapshots that led up to it.
