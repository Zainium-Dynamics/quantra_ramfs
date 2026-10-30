//! Switches to the new root: moves the virtual filesystems, pivot_root, then execs init.
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::unistd::{chdir, chroot, execv, pivot_root};
use std::ffi::CString;
use std::fs;
use std::path::Path;

use crate::cmdline::Cmdline;

/// Where the physical disk is mounted, first usable one wins.
const MOUNT_TARGETS: &[&str] = &[
    "/zairoot",  // Zainium OS preferred
    "/mnt/root", // Standard Linux initramfs (Arch, Gentoo, …)
    "/sysroot",  // systemd initrd compatibility (Fedora, RHEL, …)
    "/newroot",  // BusyBox-style initramfs
];

/// Init binaries looked for in the merged root: systemd, a foreign system's init, a rescue shell.
const INIT_FALLBACKS: &[&str] = &[
    "/overlayer/syshub/lib/systemd/systemd",
    "/sbin/init",
    "/usr/lib/systemd/systemd",
    "/lib/systemd/systemd",
    "/usr/sbin/init",
    "/overlayer/syshub/bin/fish",
    "/overlayer/syshub/bin/bash",
    "/overlayer/syshub/bin/zsh",
];

/// Moved into the new root before pivot_root; /dev goes first.
const MOUNT_MOVE_ORDER: &[&str] = &["/dev", "/proc", "/sys", "/run"];

/// The mount point and the init that pivot_to_root boots.
#[derive(Debug)]
pub struct BootTarget {
    /// Where the physical disk is mounted; the pivot itself always goes to /new_root.
    pub mount_point: &'static str,
    /// Init binary path relative to the root, e.g. /overlayer/syshub/lib/systemd/systemd.
    pub init_path: &'static str,
}

/// First MOUNT_TARGETS entry that exists or can be created.
pub fn find_mount_target(_cmdline: &Cmdline) -> Option<&'static str> {
    for &target in MOUNT_TARGETS {
        if fs::create_dir_all(target).is_ok() {
            klog!("  ✓ Mount target: {target}");
            return Some(target);
        }
    }
    // none could be created: take one that already exists
    MOUNT_TARGETS
        .iter()
        .find(|&&t| Path::new(t).exists())
        .copied()
}

/// First INIT_FALLBACKS entry that exists under `new_root`.
pub fn find_init_binary(new_root: &str) -> Option<&'static str> {
    for &candidate in INIT_FALLBACKS {
        let full = format!("{}{}", new_root, candidate);
        if Path::new(&full).exists() {
            klog!("  ✓ Init binary: {new_root}{candidate}");
            return Some(candidate);
        }
    }
    None
}

/// Picks the init to boot: init= when it exists, otherwise the first one found.
pub fn discover_boot_target(cmdline: &Cmdline, new_root: &str) -> BootTarget {
    // init= is only used when it was given and the file exists
    if let Some(cmdline_init) = cmdline.init.as_deref() {
        let full = format!("{}{}", new_root, cmdline_init);
        if Path::new(&full).exists() {
            klog!("  ✓ Using cmdline init={cmdline_init}");
            let mount = MOUNT_TARGETS
                .iter()
                .find(|&&t| Path::new(t).exists())
                .copied()
                .unwrap_or(MOUNT_TARGETS[0]);
            // keep a &'static str: use the matching INIT_FALLBACKS entry, or the first one
            let init = INIT_FALLBACKS
                .iter()
                .find(|&&i| i == cmdline_init)
                .copied()
                .unwrap_or(INIT_FALLBACKS[0]);
            return BootTarget {
                mount_point: mount,
                init_path: init,
            };
        }
        klog!("  ⚠ cmdline init={cmdline_init} not found — falling back to auto-discovery");
    }

    // Auto-discover from priority list
    let init_path = find_init_binary(new_root).unwrap_or_else(|| {
        klog!("  ⚠ No init binary found — falling back to rescue shell");
        "/overlayer/syshub/bin/bash"
    });

    let mount_point = MOUNT_TARGETS
        .iter()
        .find(|&&t| Path::new(t).exists())
        .copied()
        .unwrap_or(MOUNT_TARGETS[0]);

    BootTarget {
        mount_point,
        init_path,
    }
}

/// Moves /dev, /proc, /sys and /run in, pivots to /new_root, detaches the old root and execs init.
pub fn pivot_to_root(target: &BootTarget) -> Result<(), String> {
    // the overlay is always at /new_root; without it, boot the physical mount point
    let new_root = if Path::new("/new_root").exists() {
        "/new_root"
    } else {
        target.mount_point
    };
    let init_rel = target.init_path;

    // pivot_root needs a mount point: bind /new_root onto itself
    mount(
        Some(new_root),
        new_root,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| format!("self-bind-mount {}: {}", new_root, e))?;

    // pivot_root also fails with EBUSY under shared mount propagation
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| format!("make-rprivate /: {}", e))?;

    // Step 1: MS_MOVE pseudo-filesystems into new root
    for &mp in MOUNT_MOVE_ORDER {
        let dest = format!("{}{}", new_root, mp);
        // Directory may already exist inside the OverlayFS; ok if create fails.
        fs::create_dir_all(&dest).ok();

        mount(
            Some(mp),
            dest.as_str(),
            None::<&str>,
            MsFlags::MS_MOVE,
            None::<&str>,
        )
        .map_err(|e| format!("MS_MOVE {} → {}: {}", mp, dest, e))?;
    }

    // Step 2: chdir into new root
    chdir(new_root).map_err(|e| format!("chdir {}: {}", new_root, e))?;

    // Step 3: pivot_root(2)
    let old_root = format!("{}/.old_root", new_root);
    fs::create_dir_all(&old_root).ok();

    let pivot_ok = match pivot_root(new_root, old_root.as_str()) {
        Ok(()) => {
            chdir("/").map_err(|e| format!("chdir / post-pivot: {}", e))?;
            // Detach the physical disk — /zairoot is now invisible
            let _ = umount2("/.old_root", MntFlags::MNT_DETACH);
            let _ = fs::remove_dir("/.old_root");
            klog!("  ✓ pivot_root complete — physical disk detached");
            true
        }
        Err(e) => {
            klog!("  ⚠ pivot_root failed ({e}) — chroot fallback");
            false
        }
    };

    // Step 3 fallback: chroot (live ISO / overlay-off boot on squashfs)
    if !pivot_ok {
        chroot(new_root).map_err(|e| format!("chroot {}: {}", new_root, e))?;
        chdir("/").map_err(|e| format!("chdir / post-chroot: {}", e))?;
        klog!("  ✓ chroot fallback complete");
    }

    // programs under /overlayer/syshub need sys_resolver from their first instruction
    if init_rel.starts_with("/overlayer/syshub/")
        && init_rel != "/overlayer/syshub/bin/bash"
        && Path::new("/overlayer/syshub/lib/libsys_resolver.so").exists()
    {
        // SAFETY: still single-threaded, nothing else reads the environment
        unsafe {
            std::env::set_var("LD_PRELOAD", "/overlayer/syshub/lib/libsys_resolver.so");
        }
        klog!("  ✓ sys_resolver: LD_PRELOAD armed for {init_rel}");
    }

    // init gets the console on fds 1/2, as the kernel would hand it
    crate::log::to_console();

    // Step 4: execv — never returns on success
    let cs = CString::new(init_rel).map_err(|e| format!("CString '{}': {}", init_rel, e))?;
    let args = vec![cs.clone()];
    execv(&cs, &args).map_err(|e| format!("execv '{}': {}", init_rel, e))?;

    unreachable!("execv returned — kernel bug");
}

/// Boot target for rd.rescue and single: a bash shell.
pub fn rescue_boot_target() -> BootTarget {
    let mount_point = MOUNT_TARGETS
        .iter()
        .find(|&&t| Path::new(t).exists())
        .copied()
        .unwrap_or(MOUNT_TARGETS[0]);

    // Pick the best available rescue shell (Zainium's own, under syshub)
    let shells = [
        "/overlayer/syshub/bin/bash",
        "/overlayer/syshub/bin/fish",
        "/overlayer/syshub/bin/zsh",
    ];
    let init_path = INIT_FALLBACKS
        .iter()
        .filter(|&&p| shells.contains(&p))
        .find(|&&p| {
            let full = format!("{}{}", "/new_root", p);
            Path::new(&full).exists()
        })
        .copied()
        .unwrap_or("/overlayer/syshub/bin/bash");

    klog!("  ✓ Rescue mode: {init_path}");
    BootTarget {
        mount_point,
        init_path,
    }
}
