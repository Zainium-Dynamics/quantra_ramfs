//! fs-verity for the base: once sealed, the kernel refuses any write to a base file, root included.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;

use fs_verity::InnerHashAlgorithm;
use fs_verity::linux::{fsverity_enable, fsverity_is_enabled};

/// Sealed on every boot; a file that cannot be sealed stops the boot.
const BASE: [&str; 2] = ["overlayer/syshub", "overlayer/zaisys"];
/// zex-installed software: sealed on every boot, except its etc and var, which stay writable.
const UNION: &str = "overlayer/zexlib/union";
const UNION_WRITABLE: [&str; 2] = ["etc", "var"];

/// What the last full check saw, kept on each tree's top directory (root-only, no extra file).
const STAMP: &str = "trusted.zainium.verity";


/// Sealing the userland is the user's choice: `elevate zex verity --union on` creates this
/// file (and seals the union at once), `--union off` removes it; zainium.verity=union on the
/// kernel command line turns it on for one boot.
const UNION_SWITCH: &str = "overlayer/zexlib/seal-union";

/// The userland is sealed only when asked: the switch file, or zainium.verity=union on the
/// kernel command line. Sealed files can't be written, only replaced, which software that
/// updates itself in place doesn't expect.
fn union_wanted(zairoot: &Path) -> bool {
    zairoot.join(UNION_SWITCH).exists()
        || fs::read_to_string("/proc/cmdline")
            .is_ok_and(|c| c.split_whitespace().any(|a| a == "zainium.verity=union"))
}

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

/// Installed disk only: seals every base and zaisys file that is not sealed yet, on all CPUs,
/// and the userland's (union) files too when the user turned that on (`union_wanted`).
///
/// A tree whose directories look exactly as at the last full check is skipped: adding, removing
/// or renaming a file changes its directory's ctime, and a sealed file's content cannot change.
/// zainium.verity=full on the kernel command line checks every file anyway. No option turns
/// sealing the base and zaisys off.
pub fn ensure_base(zairoot: &str) -> Result<(), String> {
    let z = Path::new(zairoot);
    let full = fs::read_to_string("/proc/cmdline")
        .is_ok_and(|c| c.split_whitespace().any(|a| a == "zainium.verity=full"));

    let mut base = Vec::new();
    let mut base_fp = Fingerprint::default();
    for t in BASE {
        walk(&z.join(t), &[], &mut base, &mut base_fp).map_err(|e| format!("scan {t}: {e}"))?;
    }
    let mut union = Vec::new();
    let mut union_fp = Fingerprint::default();
    let union_top = z.join(UNION);
    let seal_union = union_wanted(z);
    if seal_union && union_top.is_dir() {
        walk(&union_top, &UNION_WRITABLE, &mut union, &mut union_fp)
            .map_err(|e| format!("scan {UNION}: {e}"))?;
    }
    let (base_fp, union_fp) = (base_fp.to_string(), union_fp.to_string());
    let base_top = z.join(BASE[0]);

    if full || get_stamp(&base_top).as_deref() != Some(base_fp.as_str()) {
        let (sealed, failed) = seal(&base)?;
        klog!(
            "  ✓ fs-verity: base and zaisys {} files, {sealed} sealed now",
            base.len()
        );
        if failed > 0 {
            return Err(format!("{failed} base files could not be sealed"));
        }
        set_stamp(&base_top, &base_fp)?;
    } else {
        klog!(
            "  ✓ fs-verity: base and zaisys unchanged since the last check ({} files)",
            base.len()
        );
    }
    if !seal_union {
        klog!("  ✓ fs-verity: userland not sealed (turn on with: elevate zex verity --union on)");
    } else if union_top.is_dir()
        && (full || get_stamp(&union_top).as_deref() != Some(union_fp.as_str()))
    {
        let (sealed, failed) = seal(&union)?;
        klog!(
            "  ✓ fs-verity: userland {} files, {sealed} sealed now, {failed} failed",
            union.len()
        );
        // a userland file that failed is tried again next boot
        if failed == 0 {
            set_stamp(&union_top, &union_fp)?;
        }
    } else {
        klog!(
            "  ✓ fs-verity: userland unchanged since the last check ({} files)",
            union.len()
        );
    }
    Ok(())
}

/// Seals `files` in parallel: (sealed now, failed). A filesystem without fs-verity is an error.
fn seal(files: &[PathBuf]) -> Result<(usize, usize), String> {
    let sealed = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let unsupported_fs = AtomicUsize::new(0);
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
                            sealed.fetch_add(1, Ordering::Relaxed);
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
    if unsupported_fs.into_inner() > 0 {
        return Err("the root filesystem has no fs-verity (tune2fs -O verity)".into());
    }
    Ok((sealed.into_inner(), failed.into_inner()))
}

fn get_stamp(dir: &Path) -> Option<String> {
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let name = std::ffi::CString::new(STAMP).ok()?;
    let mut buf = [0u8; 128];
    // SAFETY: both strings are NUL-terminated and buf is valid for its whole length
    let n = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

fn set_stamp(dir: &Path, value: &str) -> Result<(), String> {
    let path =
        std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    let name = std::ffi::CString::new(STAMP).map_err(|e| e.to_string())?;
    // SAFETY: both strings are NUL-terminated and value is valid for its whole length
    let r = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if r != 0 {
        return Err(format!(
            "{STAMP} on {}: {}",
            dir.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Directory count, file count and the newest change time of the directories below a tree's top
/// (the top carries the stamp, so its own ctime is left out).
#[derive(Default)]
struct Fingerprint {
    dirs: u64,
    files: u64,
    newest: i128,
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.dirs, self.files, self.newest)
    }
}

/// What the scan threads share.
#[derive(Default)]
struct Scan {
    todo: Vec<PathBuf>,
    busy: usize,
    out: Vec<PathBuf>,
    fp: Fingerprint,
    err: Option<io::Error>,
}

/// One directory: its subdirectories, its regular files and the newest ctime below it.
fn scan_dir(dir: &Path, skip: &[&str]) -> io::Result<(Vec<PathBuf>, Vec<PathBuf>, i128)> {
    let (mut dirs, mut files, mut newest) = (Vec::new(), Vec::new(), 0);
    for e in fs::read_dir(dir)? {
        let e = e?;
        if skip.iter().any(|s| e.file_name() == *s) {
            continue;
        }
        let ft = e.file_type()?;
        if ft.is_dir() {
            let m = e.metadata()?;
            newest = newest.max(m.ctime() as i128 * 1_000_000_000 + m.ctime_nsec() as i128);
            dirs.push(e.path());
        } else if ft.is_file() {
            files.push(e.path());
        }
    }
    Ok((dirs, files, newest))
}

fn scan_worker(state: &Mutex<Scan>, wake: &Condvar) {
    let mut s = state.lock().unwrap();
    loop {
        let Some(dir) = s.todo.pop() else {
            if s.busy == 0 {
                wake.notify_all();
                return;
            }
            s = wake.wait(s).unwrap();
            continue;
        };
        s.busy += 1;
        drop(s);
        let found = scan_dir(&dir, &[]);
        s = state.lock().unwrap();
        s.busy -= 1;
        match found {
            Ok((dirs, files, newest)) => {
                s.fp.dirs += 1;
                s.fp.files += files.len() as u64;
                s.fp.newest = s.fp.newest.max(newest);
                s.out.extend(files);
                s.todo.extend(dirs);
            }
            Err(e) => {
                s.todo.clear();
                s.err = Some(e);
            }
        }
        wake.notify_all();
    }
}

/// Regular files under `dir` minus the top-level `skip` names, without following symlinks,
/// read on several threads: most of a normal boot's wait here is the disk.
fn walk(dir: &Path, skip: &[&str], out: &mut Vec<PathBuf>, fp: &mut Fingerprint) -> io::Result<()> {
    let (dirs, files, newest) = scan_dir(dir, skip)?;
    fp.dirs += 1;
    fp.files += files.len() as u64;
    fp.newest = fp.newest.max(newest);
    out.extend(files);
    let state = Mutex::new(Scan {
        todo: dirs,
        ..Scan::default()
    });
    let wake = Condvar::new();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(4, 16));
    std::thread::scope(|t| {
        for _ in 0..threads {
            t.spawn(|| scan_worker(&state, &wake));
        }
    });
    let done = state.into_inner().unwrap();
    if let Some(e) = done.err {
        return Err(e);
    }
    fp.dirs += done.fp.dirs;
    fp.files += done.fp.files;
    fp.newest = fp.newest.max(done.fp.newest);
    out.extend(done.out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(dir: &Path) -> String {
        let mut files = Vec::new();
        let mut f = Fingerprint::default();
        walk(dir, &[], &mut files, &mut f).unwrap();
        f.to_string()
    }

    #[test]
    fn fingerprint_changes_only_when_the_tree_does() {
        let d = std::env::temp_dir().join(format!("verity-fp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/a"), "a").unwrap();
        let before = fp(&d);
        assert_eq!(before, fp(&d), "nothing changed");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(d.join("bin/new"), "n").unwrap();
        let added = fp(&d);
        assert_ne!(before, added, "a new file must force a full check");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::rename(d.join("bin/new"), d.join("bin/renamed")).unwrap();
        assert_ne!(added, fp(&d), "a rename must force a full check");
        let _ = fs::remove_dir_all(&d);
    }
}
