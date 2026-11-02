//! fs-verity for the base: once sealed, the kernel refuses any write to a base file, root included.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use fs_verity::InnerHashAlgorithm;
use fs_verity::linux::{fsverity_enable, fsverity_is_enabled};

/// Sealed on every boot; a file that cannot be sealed stops the boot.
const BASE: [&str; 2] = ["overlayer/syshub", "overlayer/zaisys"];
/// zex-installed software: sealed on every boot, except its etc and var, which stay writable.
const UNION: &str = "overlayer/zexlib/union";
const UNION_WRITABLE: [&str; 2] = ["etc", "var"];

/// Shown on the splash while files are sealed.
const SEALING: &str = "Securing the system…";

/// Turns on fs-verity for one file: Ok(true) if done now, Ok(false) if it already had it.
pub fn enable(path: &Path) -> io::Result<bool> {
    let f = File::open(path)?;
    if fsverity_is_enabled(f.as_fd())? {
        return Ok(false);
    }
    match fsverity_enable(f, 4096, InnerHashAlgorithm::Sha256, &[]) {
        Ok(()) => Ok(true),
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(false),
        Err(e) => Err(e),
    }
}

/// The filesystem cannot do fs-verity at all (no `verity` feature, or not ext4/btrfs/f2fs).
pub fn unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EOPNOTSUPP) | Some(libc::ENOTTY) | Some(libc::EINVAL)
    )
}

/// Installed disk only: seals every base, zaisys and union file that is not sealed yet, on all CPUs.
pub fn ensure_base(zairoot: &str) -> Result<(), String> {
    let z = Path::new(zairoot);
    let mut base = Vec::new();
    for t in BASE {
        walk(&z.join(t), &[], &mut base).map_err(|e| format!("scan {t}: {e}"))?;
    }
    let mut union = Vec::new();
    let u = z.join(UNION);
    if u.is_dir() {
        walk(&u, &UNION_WRITABLE, &mut union).map_err(|e| format!("scan {UNION}: {e}"))?;
    }

    let (sealed, failed) = seal(&base)?;
    klog!(
        "  ✓ fs-verity: base and zaisys {} files, {sealed} sealed now",
        base.len()
    );
    if failed > 0 {
        return Err(format!("{failed} base files could not be sealed"));
    }
    let (sealed, failed) = seal(&union)?;
    klog!(
        "  ✓ fs-verity: userland {} files, {sealed} sealed now, {failed} failed",
        union.len()
    );
    Ok(())
}

/// Seals `files` in parallel: (sealed now, failed). A filesystem without fs-verity is an error.
fn seal(files: &[PathBuf]) -> Result<(usize, usize), String> {
    let sealed = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let unsupported_fs = AtomicUsize::new(0);
    let shown = AtomicUsize::new(0);
    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16);
    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(f) = files.get(i) else { break };
                    match enable(f) {
                        Ok(true) => {
                            // the splash only appears when there is real work
                            if sealed.fetch_add(1, Ordering::Relaxed) == 0 {
                                crate::plymouth::show_message(SEALING);
                                shown.store(1, Ordering::Relaxed);
                            }
                        }
                        Ok(false) => {}
                        Err(e) if unsupported(&e) => {
                            unsupported_fs.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        Err(e) => {
                            if failed.fetch_add(1, Ordering::Relaxed) < 5 {
                                klog!("  ⚠ fs-verity: {}: {e}", f.display());
                            }
                        }
                    }
                }
            });
        }
    });
    if shown.into_inner() == 1 {
        crate::plymouth::hide_message(SEALING);
    }
    if unsupported_fs.into_inner() > 0 {
        return Err("the root filesystem has no fs-verity (tune2fs -O verity)".into());
    }
    Ok((sealed.into_inner(), failed.into_inner()))
}

/// Regular files under `dir` minus the top-level `skip` names, without following symlinks.
fn walk(dir: &Path, skip: &[&str], out: &mut Vec<PathBuf>) -> io::Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let ft = e.file_type()?;
        if skip.iter().any(|s| e.file_name() == *s) {
            continue;
        }
        if ft.is_dir() {
            walk(&e.path(), &[], out)?;
        } else if ft.is_file() {
            out.push(e.path());
        }
    }
    Ok(())
}
