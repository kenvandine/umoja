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
and `umoja-image-boot` (screenshots and the serial log).
