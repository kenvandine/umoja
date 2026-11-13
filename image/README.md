# Umoja disk image

`imagecraft.yaml` builds a bootable disk image of Ubuntu 26.10 with the
Umoja desktop instead of GNOME Shell (`ubuntu-umoja-desktop`), Ubuntu's
signed shim and GRUB, and the snaps Ubuntu Desktop seeds. It is phase 2
of the installer ISO plan: the installer ISO is built from the same root
filesystem.

## Build

Imagecraft runs the build in an Ubuntu 26.10 LXD container:

```sh
sudo snap install lxd && sudo lxd init --auto
sudo snap install imagecraft --classic
image/prepare            # copies the PPA key and Mir pin into image/apt
cd image && imagecraft pack
```

The result is `image/umoja.img`: a GPT disk with a 1 GiB EFI partition and
a 16 GiB ext4 root (`LABEL=umoja-root`).

Packages come from the Ubuntu archive and `ppa:ken-vandine/umoja`. To try
debs that aren't published there yet, put them in `image/local-debs/`
before packing; they win over the PPA's. CI does this with the debs of
the matching Packages workflow run.

## Boot it

```sh
image/test/boot-vm image/umoja.img /tmp/umoja-boot 240
```

This boots the image in QEMU with Secure Boot on and saves screenshots.
For an interactive VM, open `umoja.img` in GNOME Boxes or virt-manager
(UEFI firmware). The test image has one account, `umoja` with password
`umoja`; the installer will create the user's own account instead.

## CI

`.github/workflows/image.yml` builds the image for pull requests that
touch it (using that commit's debs), after each Packages run on main, and
on demand. Artifacts: `umoja-image` (the compressed image, kept 7 days)
and `umoja-image-boot` (screenshots and the serial log). When the run has
debs to add, it also builds the installer ISO:
`ubuntu-umoja-desktop-vVERSION+HASH_amd64` holds the `.iso`, its
`.sha256` and the `.list` package manifest (kept 7 days), and the `.list`
is also uploaded on its own, unzipped.

## Installer ISO

`image/iso/build` turns `umoja.img` into a live ISO that boots into the
Umoja session with the installer (`umoja-installer`, from the
`umoja-installer` package) on top:

```sh
sudo apt install xorriso mtools dosfstools squashfs-tools
make deb                                  # for umoja-installer_*.deb
sudo image/iso/build image/umoja.img .. umoja.iso
```

The installer erases the chosen disk and lays out an EFI partition,
`/boot`, and a LUKS2-encrypted ext4 root that the login password unlocks
(`bin/umoja-install` does the work). Boot is UEFI only, through Ubuntu's
signed shim and GRUB, with Secure Boot on.

Next to the ISO goes a package manifest with the same name and a `.list`
extension: every package the installed system gets, one `name version`
per line.

`--autoinstall` builds a test ISO that installs to the first disk
unattended (account `umoja`, password `umoja-install`) and powers off.
`image/test/install-vm` runs that ISO in QEMU, then boots the installed
disk, types the disk password and takes screenshots:

```sh
sudo image/iso/build --autoinstall image/umoja.img .. umoja-auto.iso
image/test/install-vm umoja-auto.iso /tmp/umoja-install
```

## Everything at once

`image/build-local` builds the debs, the image and the installer ISO from a
checkout in one go, and names the ISO after the version and commit:

```sh
image/build-local                 # build/ubuntu-umoja-desktop-v2026.46.7+1a2b3c4d_amd64.iso
image/build-local --autoinstall   # the unattended CI variant
image/build-local --skip-debs --skip-image   # just redo the ISO
```

Only the last step needs root (it asks through sudo). Logs go to
`build/logs/`.
