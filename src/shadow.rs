//! Stops the userland from hiding programs and libraries of the base.
//!
//! A file in the upper layer wins over the base's, so root could replace `bin/login` for
//! every later boot without touching the sealed base. Before the overlay is mounted, a
//! userland entry over a base entry in these trees is moved out of the union and kept in
//! `zexlib/shadowed`. Data in `share` stays: zex's generated caches cover the base's.

use std::fs;
use std::path::Path;

const BASE: &str = "overlayer/syshub";
const UNION: &str = "overlayer/zexlib/union";
const KEPT: &str = "overlayer/zexlib/shadowed";
const PROTECTED: [&str; 6] = [
    "bin",
    "sbin",
    "libexec",
    "lib",
    "drivers",
    "x86_64-zainium-linux-musl",
];

/// Moves what hides the base out of the union; the number moved.
pub fn clear(zairoot: &Path) -> usize {
    let (base, union, kept) = (zairoot.join(BASE), zairoot.join(UNION), zairoot.join(KEPT));
    let mut moved = 0;
    for top in PROTECTED {
        walk(
            &base.join(top),
            &union.join(top),
            &kept.join(top),
            &mut moved,
        );
    }
    moved
}

fn walk(base: &Path, union: &Path, kept: &Path, moved: &mut usize) {
    remove_opaque(union);
    let Ok(entries) = fs::read_dir(union) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let (b, u, k) = (base.join(&name), union.join(&name), kept.join(&name));
        let Ok(in_base) = fs::symlink_metadata(&b) else {
            continue;
        };
        let Ok(in_union) = fs::symlink_metadata(&u) else {
            continue;
        };
        if in_base.is_dir() && in_union.is_dir() {
            walk(&b, &u, &k, moved);
        } else if move_out(&u, &k) {
            klog!("  ⚠ {} hid the base: moved to {}", u.display(), k.display());
            *moved += 1;
        }
    }
}

fn move_out(from: &Path, to: &Path) -> bool {
    crate::pending::set_immutable(from, false);
    if let Some(dir) = to.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::remove_file(to);
    fs::rename(from, to).is_ok()
}

/// An opaque directory hides everything of the base under it.
fn remove_opaque(dir: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return;
    };
    for name in [c"trusted.overlay.opaque", c"user.overlay.opaque"] {
        // SAFETY: both strings are valid and NUL-terminated for the call.
        unsafe { libc::lremovexattr(path.as_ptr(), name.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tree(name: &str) -> PathBuf {
        let z = std::env::temp_dir().join(format!("shadow-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&z);
        for d in ["bin", "lib/security", "share/mime"] {
            fs::create_dir_all(z.join(BASE).join(d)).unwrap();
            fs::create_dir_all(z.join(UNION).join(d)).unwrap();
        }
        fs::write(z.join(BASE).join("bin/login"), "base").unwrap();
        fs::write(z.join(BASE).join("lib/security/pam_unix.so"), "base").unwrap();
        fs::write(z.join(BASE).join("share/mime/mime.cache"), "base").unwrap();
        z
    }

    #[test]
    fn a_file_over_the_base_is_moved_and_kept() {
        let z = tree("file");
        fs::write(z.join(UNION).join("bin/login"), "fake").unwrap();
        fs::write(z.join(UNION).join("lib/security/pam_unix.so"), "fake").unwrap();
        assert_eq!(clear(&z), 2);
        assert!(!z.join(UNION).join("bin/login").exists());
        assert_eq!(fs::read(z.join(KEPT).join("bin/login")).unwrap(), b"fake");
        assert_eq!(fs::read(z.join(BASE).join("bin/login")).unwrap(), b"base");
        let _ = fs::remove_dir_all(&z);
    }

    #[test]
    fn a_link_over_the_base_is_moved() {
        let z = tree("link");
        std::os::unix::fs::symlink("/tmp/fake", z.join(UNION).join("bin/login")).unwrap();
        assert_eq!(clear(&z), 1);
        assert!(fs::symlink_metadata(z.join(UNION).join("bin/login")).is_err());
        let _ = fs::remove_dir_all(&z);
    }

    #[test]
    fn new_software_and_generated_data_stay() {
        let z = tree("keep");
        fs::write(z.join(UNION).join("bin/htop"), "new").unwrap();
        fs::write(z.join(UNION).join("share/mime/mime.cache"), "cache").unwrap();
        assert_eq!(clear(&z), 0);
        assert!(z.join(UNION).join("bin/htop").exists());
        assert!(z.join(UNION).join("share/mime/mime.cache").exists());
        let _ = fs::remove_dir_all(&z);
    }
}
