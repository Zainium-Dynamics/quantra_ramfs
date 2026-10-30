quantra-ramfs
=============

The initramfs of Zainium OS. It is one static Rust program that runs as
/init before the system starts. It:

  - mounts the root: a disk partition, or the live medium's EROFS image
  - unlocks LUKS (TPM2, keyfile or passphrase) and assembles RAID and LVM
  - loads the display driver from the disk and starts the Plymouth splash
  - applies a staged base upgrade, before anything uses the base
  - seals the base with fs-verity on the first boot of an installed system
  - joins syshub and zexlib with OverlayFS at /overlayer/syshub and starts
    systemd


Rescue
------

Add zainium.overlay=off to the boot entry to start the base on its own,
without the software you installed. rd.break=<phase> stops at that phase
with an emergency shell whose commands need no external programs.


Building
--------

  cargo build --release --target x86_64-unknown-linux-musl
  ./pack-initramfs.sh [RAMFS_BIN] [SYSHUB_DIR] [OUT_IMG]

pack-initramfs.sh writes the image, with Plymouth taken from a syshub tree.


License
-------

MIT.
