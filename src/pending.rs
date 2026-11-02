//! Applies a staged base upgrade at boot, before the overlay is mounted, so the base never moves under a running system.

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

#[derive(Debug)]
enum Entry {
    File {
        mode: u32,
        sha: String,
        rel: PathBuf,
    },
    Link {
        target: PathBuf,
        rel: PathBuf,
    },
    Dir {
        mode: u32,
        rel: PathBuf,
    },
}

impl Entry {
    fn rel(&self) -> &Path {
        match self {
            Entry::File { rel, .. } | Entry::Link { rel, .. } | Entry::Dir { rel, .. } => rel,
        }
    }
}

/// Installed disk only: lock down the private dirs, finish/undo interrupted work, then apply or roll back.
/// An error means the base may not be in a known state, and normal boot must stop.
pub fn run(zairoot: &str) -> Result<(), String> {
    let z = Path::new(zairoot);
    let pending = z.join(PENDING);
    let rollback = z.join(ROLLBACK);
    for dir in [&pending, &rollback] {
        private_dir(dir).map_err(|e| format!("cannot prepare {}: {e}", dir.display()))?;
    }
    recover_incomplete(z, &rollback)?;

    if pending.join("ROLLBACK").exists() {
        set_immutable(&pending.join("ROLLBACK"), false);
        match rollback_last(z, &rollback) {
            Ok(Some(txn)) => klog!("  ✓ syshub: rolled back upgrade {txn}"),
            Ok(None) => klog!("  ⚠ syshub: rollback requested but nothing to roll back"),
            // the request stays until a rollback succeeds
            Err(e) => return Err(format!("rollback failed: {e}")),
        }
        fs::remove_file(pending.join("ROLLBACK")).map_err(|e| format!("ROLLBACK: {e}"))?;
    }

    if pending.join("READY").exists() {
        crate::plymouth::show_message(UPDATING);
        clear_tree_immutable(&pending);
        let applied = apply(z, &pending, &rollback);
        crate::plymouth::hide_message(UPDATING);
        match applied {
            Ok(txn) => klog!("  ✓ syshub: upgrade {txn} applied to base"),
            Err(e) => {
                // undo whatever part of it reached the base before the upgrade is dropped
                recover_incomplete(z, &rollback)
                    .map_err(|u| format!("upgrade failed ({e}) and could not be undone: {u}"))?;
                klog!("  ✗ syshub: upgrade not applied, base unchanged: {e}");
            }
        }
        empty_dir(&pending);
    }
    Ok(())
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
    for e in &entries {
        no_symlink_parent(&base, e.rel())?;
    }
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
        let Entry::Dir { mode, rel } = e else {
            continue;
        };
        let dst = base.join(rel);
        if !dst.is_dir() {
            log(&mut journal, 'D', rel)?;
            fs::create_dir_all(&dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
            let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(*mode));
        }
    }

    for e in entries.iter().filter(|e| !matches!(e, Entry::Dir { .. })) {
        let rel = e.rel();
        no_symlink_parent(&base, rel)?;
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
            if fs::symlink_metadata(&u)
                .map(|m| !m.is_dir())
                .unwrap_or(false)
            {
                log(&mut journal, 'U', rel)?;
                let keep = tdir.join("union").join(rel);
                fs::create_dir_all(keep.parent().unwrap()).map_err(|e| e.to_string())?;
                set_immutable(&u, false);
                fs::rename(&u, &keep)
                    .map_err(|e| format!("move union copy {}: {e}", u.display()))?;
            }
        }
    }

    // every rename above reaches the disk before the journal says the upgrade happened
    sync_fs(&base)?;
    journal
        .write_all(b"COMMIT\n")
        .and_then(|_| journal.sync_all())
        .map_err(|e| e.to_string())?;
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
            fs::set_permissions(&tmp, fs::Permissions::from_mode(*mode))
                .map_err(|e| e.to_string())?;
            File::open(&tmp)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
            // sealed before it takes the real name: once in the base, its content cannot be rewritten
            match crate::baseverity::enable(&tmp) {
                Ok(_) => {}
                Err(e) if crate::baseverity::unsupported(&e) => {}
                Err(e) => return Err(format!("fs-verity {}: {e}", dst.display())),
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
        no_symlink_parent(staged, e.rel())?;
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
    let Some(txn) = txns.pop() else {
        return Ok(None);
    };
    undo(z, &rollback.join(&txn))?;
    Ok(Some(txn))
}

/// A txn without COMMIT means power was lost mid-apply: put the base back exactly as it was.
fn recover_incomplete(z: &Path, rollback: &Path) -> Result<(), String> {
    let Ok(dir) = fs::read_dir(rollback) else {
        return Ok(());
    };
    for d in dir.filter_map(|d| d.ok()) {
        let tdir = d.path();
        if tdir.is_dir() && !committed(&tdir) {
            let name = d.file_name().to_string_lossy().into_owned();
            undo(z, &tdir).map_err(|e| format!("cannot undo interrupted upgrade {name}: {e}"))?;
            klog!("  ⚠ syshub: interrupted upgrade {name} undone");
        }
    }
    Ok(())
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
        // a copy that power loss left half placed
        let tmp = dst.with_file_name(format!(
            ".{}.zexnew",
            dst.file_name().unwrap_or_default().to_string_lossy()
        ));
        gone(fs::remove_file(&tmp), &tmp)?;
        match k {
            'U' => {
                let keep = tdir.join("union").join(rel);
                if fs::symlink_metadata(&keep).is_ok() {
                    let u = union.join(rel);
                    if let Some(parent) = u.parent() {
                        fs::create_dir_all(parent)
                            .map_err(|e| format!("{}: {e}", parent.display()))?;
                    }
                    fs::rename(&keep, &u)
                        .map_err(|e| format!("restore union copy {}: {e}", u.display()))?;
                }
            }
            'B' => {
                let bak = tdir.join("files").join(rel);
                if fs::symlink_metadata(&bak).is_ok() {
                    set_immutable(&dst, false);
                    gone(fs::remove_file(&dst), &dst)?;
                    fs::rename(&bak, &dst)
                        .map_err(|e| format!("restore {}: {e}", dst.display()))?;
                }
            }
            'N' => {
                set_immutable(&dst, false);
                gone(fs::remove_file(&dst), &dst)?;
            }
            'D' => {
                gone(fs::remove_dir(&dst), &dst)?;
            }
            _ => {}
        }
    }
    // the restored base reaches the disk before the journal that could redo this undo is gone
    sync_fs(&base)?;
    fs::remove_dir_all(tdir).map_err(|e| format!("remove {}: {e}", tdir.display()))
}

/// syncfs(2) on the filesystem holding `path`.
fn sync_fs(path: &Path) -> Result<(), String> {
    let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: the descriptor stays open for the call
    if unsafe { libc::syncfs(std::os::unix::io::AsRawFd::as_raw_fd(&f)) } != 0 {
        return Err(format!(
            "syncfs {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Ok when the thing is already gone: an undo may run again after another power loss.
fn gone(r: std::io::Result<()>, what: &Path) -> Result<(), String> {
    match r {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", what.display()))
        }
        _ => Ok(()),
    }
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
            ["l", target, rel] => Entry::Link {
                target: PathBuf::from(target),
                rel: PathBuf::from(rel),
            },
            ["d", mode, rel] => Entry::Dir {
                mode: u32::from_str_radix(mode, 8).map_err(|_| bad())?,
                rel: PathBuf::from(rel),
            },
            _ => return Err(bad()),
        };
        if !safe_rel(entry.rel()) {
            return Err(format!(
                "unsafe path in manifest: {}",
                entry.rel().display()
            ));
        }
        out.push(entry);
    }
    if out.is_empty() {
        return Err("empty manifest".into());
    }
    // one entry per path, and nothing below a path that is not a directory
    let mut kinds = std::collections::HashMap::new();
    for e in &out {
        if kinds
            .insert(e.rel().to_path_buf(), matches!(e, Entry::Dir { .. }))
            .is_some()
        {
            return Err(format!("manifest lists {} twice", e.rel().display()));
        }
    }
    for e in &out {
        for a in e
            .rel()
            .ancestors()
            .skip(1)
            .filter(|a| !a.as_os_str().is_empty())
        {
            if kinds.get(a) == Some(&false) {
                return Err(format!(
                    "manifest puts {} below the non-directory {}",
                    e.rel().display(),
                    a.display()
                ));
            }
        }
    }
    Ok(out)
}

/// No directory on the way from `root` to `rel` may be a symlink: it could lead out of `root`.
fn no_symlink_parent(root: &Path, rel: &Path) -> Result<(), String> {
    let mut p = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        p.push(c);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!(
                    "{} goes through the symlink {}",
                    rel.display(),
                    p.display()
                ));
            }
            Ok(m) if !m.is_dir() => {
                return Err(format!(
                    "{} goes through the non-directory {}",
                    rel.display(),
                    p.display()
                ));
            }
            Ok(_) => {}
            // nothing further down exists yet, so nothing further down can be a link
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("{}: {e}", p.display())),
        }
    }
    Ok(())
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
            let _ = if p.is_dir() && !p.is_symlink() {
                fs::remove_dir_all(&p)
            } else {
                fs::remove_file(&p)
            };
        }
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for d in rd.filter_map(|d| d.ok()) {
            let p = d.path();
            if fs::symlink_metadata(&p)
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
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
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
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
        let new = if on {
            flags | FS_IMMUTABLE_FL
        } else {
            flags & !FS_IMMUTABLE_FL
        };
        if new != flags {
            libc::ioctl(fd, FS_IOC_SETFLAGS as _, &new);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pending-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join(BASE)).unwrap();
        fs::create_dir_all(d.join(UNION)).unwrap();
        d
    }

    fn stage(z: &Path, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let pending = z.join(PENDING);
        let rollback = z.join(ROLLBACK);
        fs::create_dir_all(pending.join("root")).unwrap();
        fs::create_dir_all(&rollback).unwrap();
        let mut manifest = String::new();
        for (rel, data) in files {
            let p = pending.join("root").join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, data).unwrap();
            let sha = hex(&crate::measured_boot::sha256(data.as_bytes()));
            manifest.push_str(&format!("f\t644\t{sha}\t{rel}\n"));
        }
        fs::write(pending.join("manifest"), manifest).unwrap();
        fs::write(pending.join("READY"), "t1\n").unwrap();
        (pending, rollback)
    }

    #[test]
    fn apply_then_rollback_restores_the_base() {
        let z = tmp("apply");
        fs::create_dir_all(z.join(BASE).join("bin")).unwrap();
        fs::write(z.join(BASE).join("bin/old"), "v1").unwrap();
        fs::create_dir_all(z.join(UNION).join("bin")).unwrap();
        fs::write(z.join(UNION).join("bin/old"), "union").unwrap();
        let (pending, rollback) = stage(&z, &[("bin/old", "v2"), ("bin/new", "n")]);

        assert_eq!(apply(&z, &pending, &rollback).unwrap(), "t1");
        assert_eq!(
            fs::read_to_string(z.join(BASE).join("bin/old")).unwrap(),
            "v2"
        );
        assert_eq!(
            fs::read_to_string(z.join(BASE).join("bin/new")).unwrap(),
            "n"
        );
        assert!(
            !z.join(UNION).join("bin/old").exists(),
            "union copy must not hide the upgrade"
        );

        assert_eq!(rollback_last(&z, &rollback).unwrap().as_deref(), Some("t1"));
        assert_eq!(
            fs::read_to_string(z.join(BASE).join("bin/old")).unwrap(),
            "v1"
        );
        assert!(!z.join(BASE).join("bin/new").exists());
        assert_eq!(
            fs::read_to_string(z.join(UNION).join("bin/old")).unwrap(),
            "union"
        );
        let _ = fs::remove_dir_all(&z);
    }

    #[test]
    fn interrupted_apply_is_undone_at_every_stage() {
        // power lost after each journal line: the base must come back as it was
        for stop in 0..3 {
            let z = tmp(&format!("crash{stop}"));
            fs::create_dir_all(z.join(BASE).join("bin")).unwrap();
            fs::write(z.join(BASE).join("bin/a"), "old").unwrap();
            let rollback = z.join(ROLLBACK);
            let tdir = rollback.join("t9");
            fs::create_dir_all(tdir.join("files/bin")).unwrap();
            let mut journal = String::new();
            if stop >= 1 {
                // 'B' logged and the original moved aside
                journal.push_str("B\tbin/a\n");
                fs::rename(z.join(BASE).join("bin/a"), tdir.join("files/bin/a")).unwrap();
            }
            if stop >= 2 {
                // the new file half placed, then in place
                fs::write(z.join(BASE).join("bin/.a.zexnew"), "half").unwrap();
                fs::write(z.join(BASE).join("bin/a"), "new").unwrap();
                journal.push_str("N\tbin/b\n");
                fs::write(z.join(BASE).join("bin/b"), "new").unwrap();
            }
            fs::write(tdir.join("journal"), journal).unwrap();

            recover_incomplete(&z, &rollback).unwrap();
            assert_eq!(
                fs::read_to_string(z.join(BASE).join("bin/a")).unwrap(),
                "old",
                "stage {stop}"
            );
            assert!(!z.join(BASE).join("bin/b").exists(), "stage {stop}");
            assert!(!z.join(BASE).join("bin/.a.zexnew").exists(), "stage {stop}");
            assert!(!tdir.exists(), "stage {stop}");
            // a second recovery (another power loss) changes nothing
            recover_incomplete(&z, &rollback).unwrap();
            let _ = fs::remove_dir_all(&z);
        }
    }

    #[test]
    fn bad_checksum_leaves_the_base_alone() {
        let z = tmp("sha");
        fs::create_dir_all(z.join(BASE).join("bin")).unwrap();
        fs::write(z.join(BASE).join("bin/a"), "old").unwrap();
        let (pending, rollback) = stage(&z, &[("bin/a", "new")]);
        fs::write(pending.join("root/bin/a"), "tampered").unwrap();
        assert!(apply(&z, &pending, &rollback).is_err());
        assert_eq!(
            fs::read_to_string(z.join(BASE).join("bin/a")).unwrap(),
            "old"
        );
        let _ = fs::remove_dir_all(&z);
    }

    #[test]
    fn symlink_parent_is_refused() {
        let z = tmp("link");
        let outside = z.join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, z.join(BASE).join("lnk")).unwrap();
        let (pending, rollback) = stage(&z, &[("lnk/evil", "x")]);
        let err = apply(&z, &pending, &rollback).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert!(!outside.join("evil").exists());
        let _ = fs::remove_dir_all(&z);
    }

    #[test]
    fn manifest_rejects_duplicates_and_type_collisions() {
        let z = tmp("manifest");
        let m = z.join("m");
        let sha = "0".repeat(64);
        fs::write(&m, format!("f\t644\t{sha}\tbin/a\nf\t644\t{sha}\tbin/a\n")).unwrap();
        assert!(read_manifest(&m).unwrap_err().contains("twice"));
        fs::write(
            &m,
            format!("f\t644\t{sha}\tbin/a\nf\t644\t{sha}\tbin/a/b\n"),
        )
        .unwrap();
        assert!(read_manifest(&m).unwrap_err().contains("non-directory"));
        fs::write(&m, format!("f\t644\t{sha}\t../etc/passwd\n")).unwrap();
        assert!(read_manifest(&m).unwrap_err().contains("unsafe"));
        let _ = fs::remove_dir_all(&z);
    }
}
