//! Boot-time syshub upgrade apply: `zex syshub -u` only stages into `.syshub-pending`;
//! the base is changed here, before the overlay is mounted, so the base never moves under a live overlay.
//!
//! On the installed disk (`<zairoot>/overlayer/`):
//!   `.syshub-pending/`   root 0700, immutable while waiting
//!       `root/<path>`    staged files
//!       `manifest`       `f\tmode\tsha256\tpath` | `l\ttarget\tpath` | `d\tmode\tpath`
//!       `READY`          txn id, written last by zex
//!       `ROLLBACK`       request to undo the last applied upgrade
//!   `.syshub-rollback/<txn>/`  root 0700, never visible at runtime
//!       `journal`        `B|N|D|U\tpath` lines, `COMMIT` last
//!       `files/<path>`   old base files, `union/<path>` union copies moved out of the way

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

const PENDING: &str = "overlayer/.syshub-pending";
const ROLLBACK: &str = "overlayer/.syshub-rollback";
const BASE: &str = "overlayer/syshub";
const UNION: &str = "overlayer/zexlib/union";

/// Shown on the splash while a staged upgrade goes into the base.
const UPDATING: &str = "Installing system updates…";

const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;
const FS_IMMUTABLE_FL: libc::c_long = 0x10;

enum Entry {
    File { mode: u32, sha: String, rel: PathBuf },
    Link { target: PathBuf, rel: PathBuf },
    Dir { mode: u32, rel: PathBuf },
}

impl Entry {
    fn rel(&self) -> &Path {
        match self {
            Entry::File { rel, .. } | Entry::Link { rel, .. } | Entry::Dir { rel, .. } => rel,
        }
    }
}

/// Installed disk only: lock down the private dirs, finish/undo interrupted work, then apply or roll back.
pub fn run(zairoot: &str) {
    let z = Path::new(zairoot);
    let pending = z.join(PENDING);
    let rollback = z.join(ROLLBACK);
    for dir in [&pending, &rollback] {
        if let Err(e) = private_dir(dir) {
            klog!("  ⚠ syshub: cannot prepare {}: {e}", dir.display());
            return;
        }
    }
    recover_incomplete(z, &rollback);

    if pending.join("ROLLBACK").exists() {
        set_immutable(&pending.join("ROLLBACK"), false);
        match rollback_last(z, &rollback) {
            Ok(Some(txn)) => klog!("  ✓ syshub: rolled back upgrade {txn}"),
            Ok(None) => klog!("  ⚠ syshub: rollback requested but nothing to roll back"),
            Err(e) => klog!("  ✗ syshub: rollback failed: {e}"),
        }
        let _ = fs::remove_file(pending.join("ROLLBACK"));
    }

    if pending.join("READY").exists() {
        crate::plymouth::show_message(UPDATING);
        clear_tree_immutable(&pending);
        match apply(z, &pending, &rollback) {
            Ok(txn) => klog!("  ✓ syshub: upgrade {txn} applied to base"),
            Err(e) => klog!("  ✗ syshub: upgrade not applied, base unchanged: {e}"),
        }
        empty_dir(&pending);
        crate::plymouth::hide_message(UPDATING);
    }
}

fn apply(z: &Path, pending: &Path, rollback: &Path) -> Result<String, String> {
    let txn = fs::read_to_string(pending.join("READY")).map_err(|e| format!("READY: {e}"))?;
    let txn = txn.trim().to_string();
    if txn.is_empty() || txn.contains('/') || txn.starts_with('.') {
        return Err(format!("bad txn id '{txn}'"));
    }
    let entries = read_manifest(&pending.join("manifest"))?;
    let staged = pending.join("root");
    verify(&staged, &entries)?;

    let base = z.join(BASE);
    let union = z.join(UNION);
    let tdir = rollback.join(&txn);
    if tdir.exists() {
        return Err(format!("txn {txn} already exists in rollback"));
    }
    fs::create_dir_all(tdir.join("files")).map_err(|e| e.to_string())?;
    let mut journal = OpenOptions::new()
        .create(true)
        .append(true)
        .open(tdir.join("journal"))
        .map_err(|e| format!("journal: {e}"))?;

    for e in entries.iter().filter(|e| matches!(e, Entry::Dir { .. })) {
        let Entry::Dir { mode, rel } = e else { continue };
        let dst = base.join(rel);
        if !dst.is_dir() {
            log(&mut journal, 'D', rel)?;
            fs::create_dir_all(&dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
            let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(*mode));
        }
    }

    for e in entries.iter().filter(|e| !matches!(e, Entry::Dir { .. })) {
        let rel = e.rel();
        let dst = base.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        if fs::symlink_metadata(&dst).is_ok() {
            log(&mut journal, 'B', rel)?;
            let bak = tdir.join("files").join(rel);
            fs::create_dir_all(bak.parent().unwrap()).map_err(|e| e.to_string())?;
            set_immutable(&dst, false);
            fs::rename(&dst, &bak).map_err(|e| format!("backup {}: {e}", dst.display()))?;
        } else {
            log(&mut journal, 'N', rel)?;
        }
        place(&staged, e, &dst)?;

        // an old copy of this base file in the userland union would hide the new one; etc stays the user's
        if !rel.starts_with("etc") {
            let u = union.join(rel);
            if fs::symlink_metadata(&u).map(|m| !m.is_dir()).unwrap_or(false) {
                log(&mut journal, 'U', rel)?;
                let keep = tdir.join("union").join(rel);
                fs::create_dir_all(keep.parent().unwrap()).map_err(|e| e.to_string())?;
                set_immutable(&u, false);
                fs::rename(&u, &keep).map_err(|e| format!("move union copy {}: {e}", u.display()))?;
            }
        }
    }

    journal.write_all(b"COMMIT\n").and_then(|_| journal.sync_all()).map_err(|e| e.to_string())?;
    drop(journal);
    set_tree_immutable(&tdir);
    Ok(txn)
}

fn place(staged: &Path, e: &Entry, dst: &Path) -> Result<(), String> {
    let tmp = dst.with_file_name(format!(
        ".{}.zexnew",
        dst.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = fs::remove_file(&tmp);
    match e {
        Entry::File { mode, rel, .. } => {
            let src = staged.join(rel);
            fs::copy(&src, &tmp).map_err(|e| format!("copy {}: {e}", src.display()))?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(*mode)).map_err(|e| e.to_string())?;
            File::open(&tmp).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
            // sealed before it takes the real name: once in the base, its content cannot be rewritten
            if let Err(e) = crate::baseverity::enable(&tmp) {
                klog!("  ⚠ fs-verity: {}: {e}", dst.display());
            }
        }
        Entry::Link { target, .. } => {
            symlink(target, &tmp).map_err(|e| format!("symlink {}: {e}", tmp.display()))?;
        }
        Entry::Dir { .. } => return Ok(()),
    }
    fs::rename(&tmp, dst).map_err(|e| format!("place {}: {e}", dst.display()))
}

fn verify(staged: &Path, entries: &[Entry]) -> Result<(), String> {
    for e in entries {
        let p = staged.join(e.rel());
        match e {
            Entry::File { sha, .. } => {
                let meta = fs::symlink_metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                if !meta.is_file() {
                    return Err(format!("{} is not a regular file", p.display()));
                }
                let mut data = Vec::with_capacity(meta.len() as usize);
                File::open(&p)
                    .and_then(|mut f| f.read_to_end(&mut data))
                    .map_err(|e| format!("{}: {e}", p.display()))?;
                if hex(&crate::measured_boot::sha256(&data)) != *sha {
                    return Err(format!("checksum mismatch: {}", e.rel().display()));
                }
            }
            Entry::Link { target, .. } => {
                if fs::read_link(&p).ok().as_deref() != Some(target.as_path()) {
                    return Err(format!("symlink mismatch: {}", e.rel().display()));
                }
            }
            Entry::Dir { .. } => {}
        }
    }
    Ok(())
}

/// Undo the newest committed upgrade.
fn rollback_last(z: &Path, rollback: &Path) -> Result<Option<String>, String> {
    let mut txns: Vec<String> = fs::read_dir(rollback)
        .map_err(|e| e.to_string())?
        .filter_map(|d| d.ok())
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .filter(|n| committed(&rollback.join(n)))
        .collect();
    txns.sort();
    let Some(txn) = txns.pop() else { return Ok(None) };
    undo(z, &rollback.join(&txn))?;
    Ok(Some(txn))
}

/// A txn without COMMIT means power was lost mid-apply: put the base back exactly as it was.
fn recover_incomplete(z: &Path, rollback: &Path) {
    let Ok(dir) = fs::read_dir(rollback) else { return };
    for d in dir.filter_map(|d| d.ok()) {
        let tdir = d.path();
        if tdir.is_dir() && !committed(&tdir) {
            match undo(z, &tdir) {
                Ok(()) => klog!("  ⚠ syshub: interrupted upgrade {} undone", d.file_name().to_string_lossy()),
                Err(e) => klog!("  ✗ syshub: cannot undo interrupted upgrade: {e}"),
            }
        }
    }
}

fn committed(tdir: &Path) -> bool {
    fs::read_to_string(tdir.join("journal"))
        .map(|j| j.lines().any(|l| l == "COMMIT"))
        .unwrap_or(false)
}

fn undo(z: &Path, tdir: &Path) -> Result<(), String> {
    clear_tree_immutable(tdir);
    let base = z.join(BASE);
    let union = z.join(UNION);
    let journal = fs::read_to_string(tdir.join("journal")).unwrap_or_default();
    let ops: Vec<(char, PathBuf)> = journal
        .lines()
        .filter_map(|l| {
            let (k, p) = l.split_once('\t')?;
            Some((k.chars().next()?, PathBuf::from(p)))
        })
        .collect();
    for (k, rel) in ops.iter().rev() {
        if !safe_rel(rel) {
            continue;
        }
        let dst = base.join(rel);
        match k {
            'U' => {
                let keep = tdir.join("union").join(rel);
                if fs::symlink_metadata(&keep).is_ok() {
                    let u = union.join(rel);
                    let _ = fs::create_dir_all(u.parent().unwrap());
                    let _ = fs::rename(&keep, &u);
                }
            }
            'B' => {
                let bak = tdir.join("files").join(rel);
                if fs::symlink_metadata(&bak).is_ok() {
                    set_immutable(&dst, false);
                    let _ = fs::remove_file(&dst);
                    fs::rename(&bak, &dst).map_err(|e| format!("restore {}: {e}", dst.display()))?;
                }
            }
            'N' => {
                set_immutable(&dst, false);
                let _ = fs::remove_file(&dst);
            }
            'D' => {
                let _ = fs::remove_dir(&dst);
            }
            _ => {}
        }
    }
    fs::remove_dir_all(tdir).map_err(|e| format!("remove {}: {e}", tdir.display()))
}

fn read_manifest(path: &Path) -> Result<Vec<Entry>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("manifest: {e}"))?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        let bad = || format!("manifest line {}: '{line}'", n + 1);
        let entry = match f.as_slice() {
            ["f", mode, sha, rel] => Entry::File {
                mode: u32::from_str_radix(mode, 8).map_err(|_| bad())?,
                sha: sha.to_string(),
                rel: PathBuf::from(rel),
            },
            ["l", target, rel] => Entry::Link { target: PathBuf::from(target), rel: PathBuf::from(rel) },
            ["d", mode, rel] => Entry::Dir {
                mode: u32::from_str_radix(mode, 8).map_err(|_| bad())?,
                rel: PathBuf::from(rel),
            },
            _ => return Err(bad()),
        };
        if !safe_rel(entry.rel()) {
            return Err(format!("unsafe path in manifest: {}", entry.rel().display()));
        }
        out.push(entry);
    }
    if out.is_empty() {
        return Err("empty manifest".into());
    }
    Ok(out)
}

fn safe_rel(rel: &Path) -> bool {
    !rel.as_os_str().is_empty() && rel.components().all(|c| matches!(c, Component::Normal(_)))
}

fn log(journal: &mut File, kind: char, rel: &Path) -> Result<(), String> {
    writeln!(journal, "{kind}\t{}", rel.display())
        .and_then(|_| journal.sync_all())
        .map_err(|e| format!("journal: {e}"))
}

/// root-owned 0700, so nothing but root can see or touch it.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    set_immutable(dir, false);
    std::os::unix::fs::chown(dir, Some(0), Some(0))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Remove everything inside `dir` but keep `dir` itself.
fn empty_dir(dir: &Path) {
    clear_tree_immutable(dir);
    if let Ok(rd) = fs::read_dir(dir) {
        for d in rd.filter_map(|d| d.ok()) {
            let p = d.path();
            let _ = if p.is_dir() && !p.is_symlink() { fs::remove_dir_all(&p) } else { fs::remove_file(&p) };
        }
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for d in rd.filter_map(|d| d.ok()) {
            let p = d.path();
            if fs::symlink_metadata(&p).map(|m| m.is_dir()).unwrap_or(false) {
                walk(&p, out);
            }
            out.push(p);
        }
    }
}

fn clear_tree_immutable(dir: &Path) {
    set_immutable(dir, false);
    let mut all = Vec::new();
    walk(dir, &mut all);
    for p in all.iter().rev() {
        set_immutable(p, false);
    }
}

fn set_tree_immutable(dir: &Path) {
    let mut all = Vec::new();
    walk(dir, &mut all);
    for p in &all {
        set_immutable(p, true);
    }
    set_immutable(dir, true);
}

/// chattr ±i; symlinks and filesystems without inode flags are skipped silently.
fn set_immutable(path: &Path, on: bool) {
    let Ok(meta) = fs::symlink_metadata(path) else { return };
    if meta.file_type().is_symlink() || meta.mode() & libc::S_IFMT == libc::S_IFSOCK {
        return;
    }
    let Ok(f) = File::open(path) else { return };
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);
    let mut flags: libc::c_long = 0;
    // SAFETY: fd is open for the whole call and flags is a valid long for the kernel to fill.
    unsafe {
        if libc::ioctl(fd, FS_IOC_GETFLAGS as _, &mut flags) != 0 {
            return;
        }
        let new = if on { flags | FS_IMMUTABLE_FL } else { flags & !FS_IMMUTABLE_FL };
        if new != flags {
            libc::ioctl(fd, FS_IOC_SETFLAGS as _, &new);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
