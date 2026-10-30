//! Mounts /proc, /sys, /dev, /run (required) and /dev/pts, /dev/shm (optional), in that order.

use nix::mount::{MsFlags, mount};
use std::fs;

// /proc and /sys: no flags — MS_NOEXEC on /proc breaks elevate/elevate-pam and /proc/self/fd
const PROC_FLAGS: MsFlags = MsFlags::empty();
const SYS_FLAGS: MsFlags = MsFlags::empty();

// no nosuid on devtmpfs here: it stops loop node creation on some kernels
const DEV_FLAGS: MsFlags = MsFlags::empty();

// /run: nosuid + nodev, allow exec (needed for lock files, pipes)
const RUN_FLAGS: MsFlags = MsFlags::MS_NOSUID.union(MsFlags::MS_NODEV);

// /dev/shm: full security flags (no exec, no setuid, no devices)
const SHM_FLAGS: MsFlags = MsFlags::MS_NOSUID
    .union(MsFlags::MS_NODEV)
    .union(MsFlags::MS_NOEXEC);

// Directories to create before mounting
const EARLY_DIRS: &[&str] = &["/proc", "/sys", "/dev", "/run"];

/// Mounts the early filesystems; fails only when a required one cannot be mounted.
#[inline]
pub fn mount_early() -> Result<(), String> {
    klog!("Mounting early filesystems...");

    // Batch-create all mount points
    for dir in EARLY_DIRS {
        fs::create_dir_all(dir).ok();
    }

    // /proc — critical
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        PROC_FLAGS,
        None::<&str>,
    )
    .map_err(|e| format!("CRITICAL: /proc mount failed: {} (errno: {})", e, e as i32))?;
    klog!("  ✓ /proc");

    // /sys — critical
    mount(
        Some("sysfs"),
        "/sys",
        Some("sysfs"),
        SYS_FLAGS,
        None::<&str>,
    )
    .map_err(|e| format!("CRITICAL: /sys mount failed: {} (errno: {})", e, e as i32))?;
    klog!("  ✓ /sys");

    // /dev — critical — NO MS_NOSUID so loop nodes can be created
    mount(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        DEV_FLAGS,
        Some("mode=0755"),
    )
    .map_err(|e| format!("CRITICAL: /dev mount failed: {} (errno: {})", e, e as i32))?;
    klog!("  ✓ /dev");

    // devtmpfs may not create the loop nodes without udev
    create_essential_devices();

    // made after devtmpfs is on /dev, else they are hidden; plymouth needs devpts for /dev/ptmx
    fs::create_dir_all("/dev/pts").ok();
    fs::create_dir_all("/dev/shm").ok();

    // /run — critical
    mount(
        Some("tmpfs"),
        "/run",
        Some("tmpfs"),
        RUN_FLAGS,
        Some("mode=0755"),
    )
    .map_err(|e| format!("CRITICAL: /run mount failed: {} (errno: {})", e, e as i32))?;
    klog!("  ✓ /run");

    // /dev/pts — OPTIONAL
    if let Err(e) = mount(
        Some("devpts"),
        "/dev/pts",
        Some("devpts"),
        MsFlags::empty(),
        Some("mode=0620,ptmxmode=0666"),
    ) {
        klog!("  ⚠ /dev/pts: {} (non-critical)", e);
    }

    // /dev/shm — OPTIONAL
    if let Err(e) = mount(
        Some("tmpfs"),
        "/dev/shm",
        Some("tmpfs"),
        SHM_FLAGS,
        Some("mode=1777"),
    ) {
        klog!("  ⚠ /dev/shm: {} (non-critical)", e);
    }

    klog!("✓ Early mounts complete\n");
    Ok(())
}

/// Creates /dev/loop-control and /dev/loop0..7; existing nodes are left alone.
fn create_essential_devices() {
    // /dev/loop-control: misc major 10, minor 237
    unsafe {
        let path = b"/dev/loop-control\0";
        libc::mknod(
            path.as_ptr() as *const libc::c_char,
            libc::S_IFCHR | 0o600,
            libc::makedev(10, 237),
        );
    }

    // /dev/loop0..7: block major 7
    for n in 0u32..8 {
        unsafe {
            // Build "/dev/loopN\0" on stack — no heap allocation
            let mut path = [0u8; 16];
            path[0..9].copy_from_slice(b"/dev/loop");
            path[9] = b'0' + n as u8;
            // path[10] = 0  (already zeroed)

            libc::mknod(
                path.as_ptr() as *const libc::c_char,
                libc::S_IFBLK | 0o660,
                libc::makedev(7, n),
            );
        }
    }

    klog!("  ✓ Loop devices: /dev/loop-control + /dev/loop0-7");
}
