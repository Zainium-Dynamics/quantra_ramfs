#!/bin/sh
# pack-initramfs.sh -- build quantra-ramfs.img (the initramfs cpio.gz).
#
# Bundles the quantra-ramfs init binary itself PLUS a real Plymouth splash
# (daemon, its full shared-library closure, the "script" module + drm/
# framebuffer renderers, and the zainium theme) directly inside the
# initramfs. This is required, not optional: Plymouth's own DRM/cmdline
# check runs right after early mounts, long before /overlayer/syshub is
# ever mounted (that happens in the later Overlay phase) -- so plymouthd
# at its real on-disk path is invisible at that point in boot no matter
# what env/config is set. Every real distro (dracut, initramfs-tools)
# solves this the same way: ship a private copy of plymouth inside the
# initramfs image itself.
#
# All paths below are placed at their exact real absolute paths
# (/overlayer/syshub/...) -- plymouthd's own ELF interpreter and RPATH
# are hardcoded absolute (not $ORIGIN-relative), so this is not
# optional structure, it's the only layout that will actually resolve.
# At the real pivot/switch_root, this initramfs root is discarded and
# replaced by the real OverlayFS-mounted syshub, so there's no conflict
# with the on-disk copy -- this is purely for the early-boot window.
#
# Usage:
#   ./pack-initramfs.sh [RAMFS_BIN] [SYSHUB_DIR] [OUT_IMG]
#
# Defaults match this repo's own release build and Zainium's live
# staging tree.

set -e

RAMFS_BIN="${1:-target/x86_64-unknown-linux-musl/release/quantra-ramfs}"
SYSHUB="${2:-/home/alizain/os/zairoot/overlayer/syshub}"
OUT="${3:-/home/alizain/os/zairoot/overlayer/zaisys/kernel/quantra-ramfs.img}"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# The init binary itself, at the top level (unchanged from the original
# working layout).
cp "$RAMFS_BIN" "$STAGE/quantra-ramfs"

# Every file plymouthd's module chain needs, relative to $SYSHUB, kept
# at the identical relative path under $STAGE/overlayer/syshub/...

NEEDED="
sbin/plymouthd
bin/plymouth
lib/libply.so.5
lib/libply-splash-core.so.5
lib/libply-splash-graphics.so.5
lib/libply-boot-client.so.5
lib/libevdev.so.2
lib/libxkbcommon.so.0
lib/libudev.so.1
lib/libucontext.so.1
lib/libdrm.so.2
lib/libpng16.so.16
lib/libz.so.1
lib/plymouth/script.so
lib/plymouth/renderers/drm.so
lib/plymouth/renderers/frame-buffer.so
x86_64-zainium-linux-musl/lib/ld-musl-x86_64.so.1
etc/plymouth/plymouthd.conf
share/plymouth/plymouthd.defaults
"

copy_one() {
    rel="$1"
    src="$SYSHUB/$rel"
    dst="$STAGE/overlayer/syshub/$rel"

    if [ ! -e "$src" ] && [ ! -L "$src" ]; then
        echo "WARNING: missing on syshub, skipped: $rel" >&2
        return 0
    fi
    [ -e "$dst" ] || [ -L "$dst" ] && return 0

    mkdir -p "$(dirname "$dst")"

    if [ -L "$src" ]; then
        # Targets are /overlayer/syshub paths, which don't exist on the build host, so follow them by name.
        target="$(readlink "$src")"
        ln -s "$target" "$dst"
        case "$target" in
            /overlayer/syshub/*) nextrel="${target#/overlayer/syshub/}" ;;
            /*) return 0 ;;
            *)  nextrel="$(dirname "$rel")/$target" ;;
        esac
        copy_one "$nextrel"
    else
        cp -a "$src" "$dst"
    fi
}

for rel in $NEEDED; do
    copy_one "$rel"
done

# The theme itself, wholesale.
mkdir -p "$STAGE/overlayer/syshub/share/plymouth/themes"
cp -a "$SYSHUB/share/plymouth/themes/zainium" \
      "$STAGE/overlayer/syshub/share/plymouth/themes/zainium"

# Build the archive.
( cd "$STAGE" && find . | cpio -o -H newc 2>/dev/null ) | gzip -9 > "$OUT"

echo "Wrote $OUT ($(du -h "$OUT" | cut -f1))"
