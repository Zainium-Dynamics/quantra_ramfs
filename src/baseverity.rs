//! fs-verity for the base: once sealed, the kernel refuses any write to a base file, root included.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
const FS_IOC_ENABLE_VERITY: libc::c_ulong = 0x4080_6685; // _IOW('f', 133, struct fsverity_enable_arg)
const FS_VERITY_FL: libc::c_long = 0x0010_0000;
const FS_VERITY_HASH_ALG_SHA256: u32 = 1;

const MARKER: &str = "overlayer/syshub/engine/.fsverity";
const TREES: [&str; 2] = ["overlayer/syshub", "overlayer/zaisys/kernel"];

/// Shown on the splash while the base is sealed for the first time.
const SEALING: &str = "Securing the system…";

#[repr(C)]
struct EnableArg {
    version: u32,
    hash_algorithm: u32,
    block_size: u32,
    salt_size: u32,
    salt_ptr: u64,
    sig_size: u32,
    reserved1: u32,
    sig_ptr: u64,
    reserved2: [u64; 11],
}

/// Turns on fs-verity for one file: Ok(true) if done now, Ok(false) if it already had it.
pub fn enable(path: &Path) -> io::Result<bool> {
    let f = File::open(path)?;
    let fd = f.as_raw_fd();
    let mut flags: libc::c_long = 0;
    // SAFETY: fd is open for the lifetime of `f`; FS_IOC_GETFLAGS writes one long into `flags`.
    if unsafe { libc::ioctl(fd, FS_IOC_GETFLAGS as _, &mut flags) } == 0 && flags & FS_VERITY_FL != 0 {
        return Ok(false);
    }
    let arg = EnableArg {
        version: 1,
        hash_algorithm: FS_VERITY_HASH_ALG_SHA256,
        block_size: 4096,
        salt_size: 0,
        salt_ptr: 0,
        sig_size: 0,
        reserved1: 0,
        sig_ptr: 0,
        reserved2: [0; 11],
    };
    // SAFETY: `arg` matches struct fsverity_enable_arg (128 bytes) and outlives the call.
    if unsafe { libc::ioctl(fd, FS_IOC_ENABLE_VERITY as _, &arg) } == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EEXIST) {
        return Ok(false);
    }
    Err(e)
}

/// The filesystem cannot do fs-verity at all (no `verity` feature, or not ext4/btrfs/f2fs).
fn unsupported(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EOPNOTSUPP) | Some(libc::ENOTTY) | Some(libc::EINVAL))
}

/// Installed disk only: seal the whole base once. Later upgrades seal their own files.
pub fn ensure_base(zairoot: &str) {
    let z = Path::new(zairoot);
    if z.join(MARKER).exists() {
        return;
    }
    let mut files = Vec::new();
    for t in TREES {
        walk(&z.join(t), &mut files);
    }
    crate::plymouth::show_message(SEALING);
    let (mut sealed, mut failed) = (0usize, 0usize);
    for f in &files {
        match enable(f) {
            Ok(true) => sealed += 1,
            Ok(false) => {}
            Err(e) if unsupported(&e) => {
                klog!("  ⚠ fs-verity: not available on this filesystem ({e}); base files stay writable by root");
                crate::plymouth::hide_message(SEALING);
                return;
            }
            Err(e) => {
                failed += 1;
                if failed <= 5 {
                    klog!("  ⚠ fs-verity: {}: {e}", f.display());
                }
            }
        }
    }
    crate::plymouth::hide_message(SEALING);
    if failed == 0 {
        let _ = fs::write(z.join(MARKER), format!("{}\n", files.len()));
    }
    klog!("  ✓ fs-verity: {sealed} base files sealed ({} total, {failed} failed)", files.len());
}

/// Regular files under `dir`, without following symlinks.
fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), out);
        } else if ft.is_file() {
            out.push(e.path());
        }
    }
}
