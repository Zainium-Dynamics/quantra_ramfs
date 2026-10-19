# quantra-ramfs

quantra-ramfs is the initramfs of Zainium OS: the first program the kernel runs. It is a single Rust binary that prepares the real root and hands over to the system's init. It finds the root device or the live image, applies a staged base upgrade, joins the Zainium layers with OverlayFS, switches root and starts init.

## What it does at boot

1. **Early mounts**: `/proc`, `/sys`, `/dev` (devtmpfs) and `/run`. The kernel command line is read; with `quiet` or `splash`, messages go to the kernel log instead of the screen.
2. **Splash**: starts Plymouth if `splash` is set. Display devices are registered the way udev would, so Plymouth draws from the start of the boot.
3. **Devices**: a small netlink uevent handler creates device nodes; optional MD RAID and LVM activation.
4. **Root**: finds the root by `root=` (UUID, LABEL or device), with optional fsck, LUKS (key file or TPM2 unseal), dm-verity and Btrfs subvolumes. Network roots are supported (DHCP, iSCSI, NBD, HTTP). For a live ISO, `loop=` and `loopfstype=` name the image to loop-mount (Zainium uses `zaisys/zairoot.img`, EROFS).
5. **Base upgrade**: if `zex syshub --u` staged an upgrade in `overlayer/.syshub-pending`, it is applied to `overlayer/syshub` now, before the overlay is mounted. A requested rollback undoes the last applied upgrade instead.
6. **Overlay**: mounts OverlayFS at `/overlayer/syshub`, with lower layers `overlayer/syshub` and `overlayer/zaisys` and the upper layer `overlayer/zexlib/union`. `zaisys` (read-only), `zexlib` and every other entry of `overlayer/` are mounted at their own paths; `/home` and `/root` come from the disk. On a read-only live image, `zexlib`, `/home` and `/root` get writable layers in memory.
7. **Switch**: `pivot_root` into the new root and `exec` the init: `/overlayer/syshub/lib/systemd/systemd`, falling back to `quantra`, then the usual init paths of other distributions.

Each phase extends TPM2 PCR 8 when a TPM is present (measured boot). If a step fails, quantra-ramfs drops to an emergency shell on the console.

## Kernel command line

| Option | Meaning |
| :--- | :--- |
| `root=` | Root device: `UUID=…`, `LABEL=…` or `/dev/…` |
| `rootfstype=`, `rootflags=` | Root file system type and mount options (e.g. `subvol=@`) |
| `rootwait`, `rootwait=N` | Wait for the root device (forever, or N × 50 ms) |
| `loop=`, `loopfstype=` | Image inside the root device to loop-mount, and its type |
| `init=` | Init to run instead of the default list |
| `luks=`, `luks_keyfile=` | LUKS mapping name and key file (`dev:path` or path) |
| `rd.verity=1`, `rd.verity.data=`, `rd.verity.hash=`, `rd.verity.roothash=`, `rd.verity.hashoffset=` | dm-verity for the root |
| `zainium.overlay=off` | Boot the base without the userland layer (rescue) |
| `rd.break=pre-mount\|pre-overlay\|pre-pivot` | Stop in a shell at that step |
| `rd.shell` | Always stop in a shell before booting |
| `rd.rescue` | Boot to the rescue target (same as `single`) |
| `rd.fsck=0`, `rd.tpm2=0`, `rd.udev=0` | Turn off fsck, TPM2 unseal or the uevent handler |
| `quiet`, `splash` | Clean screen; logs go to `dmesg` and the journal |

## Building

quantra-ramfs targets x86_64 musl:

```sh
cargo build --release --target x86_64-unknown-linux-musl
```

`pack-initramfs.sh` builds the initramfs image (`cpio`, gzip). Besides the binary it bundles Plymouth, its libraries and renderers, and the Zainium theme, taken from a Zainium base tree:

```sh
./pack-initramfs.sh [RAMFS_BIN] [SYSHUB_DIR] [OUT_IMG]
```

- `RAMFS_BIN`: the built binary (default `target/x86_64-unknown-linux-musl/release/quantra-ramfs`)
- `SYSHUB_DIR`: a Zainium `overlayer/syshub` tree to take Plymouth from
- `OUT_IMG`: the image to write; Zainium boots it from `overlayer/zaisys/kernel/quantra-ramfs.img`

The binary sits at `/quantra-ramfs` in the image. Zainium's kernel runs that path by default; with another kernel, add `rdinit=/quantra-ramfs` to the command line.

## Source layout

| File | Purpose |
| :--- | :--- |
| `src/main.rs` | Boot sequence |
| `src/cmdline.rs` | Kernel command line parsing |
| `src/mounts.rs` | Early mounts |
| `src/log.rs` | Output: console or kernel log |
| `src/plymouth.rs` | Splash, display device registration, KMS, PKCS#11 and LUKS helpers |
| `src/udev.rs` | Netlink uevent handler and device nodes |
| `src/raid.rs` | MD RAID and LVM |
| `src/rootfs.rs` | Root detection and mounting, loop images |
| `src/fsck.rs` | File system check |
| `src/verity.rs` | dm-verity |
| `src/tpm2.rs` | TPM2 unseal of LUKS keys |
| `src/measured_boot.rs`, `src/phases.rs` | Boot phases and PCR measurements |
| `src/network_boot.rs` | DHCP, iSCSI, NBD and HTTP roots |
| `src/pending.rs` | Applying staged base upgrades and rollback |
| `src/overlay.rs` | The Zainium OverlayFS view and bind mounts |
| `src/switch.rs` | `pivot_root` and init discovery |
| `src/emergency.rs` | Emergency shell |

## License

MIT
