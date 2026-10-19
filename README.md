# quantra-ramfs

The initramfs of Zainium OS. It mounts the root (disk or live image), applies a staged base upgrade, sets up the `/overlayer` OverlayFS view and starts systemd.

## Build

```sh
cargo build --release --target x86_64-unknown-linux-musl
./pack-initramfs.sh [RAMFS_BIN] [SYSHUB_DIR] [OUT_IMG]
```

`pack-initramfs.sh` writes the initramfs image with Plymouth taken from a Zainium `syshub` tree.

## License

MIT
