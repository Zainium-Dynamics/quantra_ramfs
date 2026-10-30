/// Runs fsck on the root partition before it is mounted.
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

const FSCK_PATHS: &[&str] = &[
    "/sbin/fsck",
    "/usr/sbin/fsck",
    "/overlayer/syshub/sbin/fsck",
    "/overlayer/syshub/bin/fsck",
];

// Filesystem types that must NOT be checked (read-only / network / pseudo)
const SKIP_FSTYPES: &[&str] = &[
    "squashfs", "iso9660", "tmpfs", "ramfs", "devtmpfs", "sysfs", "proc", "devpts", "nfs", "nfs4",
    "overlay",
];

/// Checks `device` (fstype from rootfstype= if given); Err when fsck cannot repair it or run.
pub fn check_root(device: &str, fstype: Option<&str>) -> Result<(), String> {
    // Skip filesystems that cannot be fscked
    if let Some(ft) = fstype {
        if SKIP_FSTYPES.contains(&ft) {
            klog!("  fsck: skipping {} ({})", device, ft);
            return Ok(());
        }
    }

    // Skip loop and ram devices
    if device.starts_with("/dev/loop") || device.starts_with("/dev/ram") {
        klog!("  fsck: skipping {} (loop/ram)", device);
        return Ok(());
    }

    // Skip NFS
    if device.contains(':') {
        klog!("  fsck: skipping {} (NFS)", device);
        return Ok(());
    }

    let fsck_bin = match find_fsck_binary(fstype) {
        Some(p) => p,
        None => {
            klog!("  fsck: no fsck binary found — skipping (device may be unclean)");
            return Ok(());
        }
    };

    klog!("  fsck: checking {} with {}", device, fsck_bin);
    let t = Instant::now();

    // -a repairs without asking, -T drops the title
    let mut args = vec!["-a", "-T"];
    if let Some(ft) = fstype {
        args.push("-t");
        args.push(ft);
    }
    args.push(device);

    let status = Command::new(&fsck_bin)
        .args(&args)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("fsck exec '{}': {}", fsck_bin, e))?;

    let code = status.code().unwrap_or(8);
    let elapsed = t.elapsed().as_millis();

    match code {
        0 => {
            klog!("  fsck: {} clean ({}ms)", device, elapsed);
            Ok(())
        }
        1 => {
            klog!("  fsck: {} repaired ({}ms) — continuing", device, elapsed);
            Ok(())
        }
        2 => Err(format!(
            "fsck: {} repaired but REBOOT REQUIRED (exit {})",
            device, code
        )),
        4 => Err(format!(
            "fsck: {} has UNCORRECTED ERRORS (exit {}) — manual intervention required",
            device, code
        )),
        _ => Err(format!(
            "fsck: {} returned unexpected exit code {} ({}ms)",
            device, code, elapsed
        )),
    }
}

/// Like check_root, but reboots when fsck says the repair needs it (exit code 2).
#[allow(dead_code)]
pub fn check_root_or_reboot(device: &str, fstype: Option<&str>) {
    match check_root(device, fstype) {
        Ok(()) => {}
        Err(e) if e.contains("REBOOT REQUIRED") => {
            klog!("  fsck: REBOOTING for clean filesystem...");
            std::thread::sleep(std::time::Duration::from_secs(2));
            unsafe {
                libc::sync();
                libc::reboot(libc::RB_AUTOBOOT);
            }
        }
        Err(e) => {
            klog!("  fsck ERROR: {}", e);
            // Caller decides — returns so caller can drop to emergency shell
        }
    }
}

/// Mounts a Btrfs root at `target` with the subvol= from rootflags=.
pub fn mount_btrfs_subvol(
    device: &str,
    target: &str,
    rootflags: Option<&str>,
) -> Result<(), String> {
    use nix::mount::{MsFlags, mount};

    let flags = MsFlags::empty();
    let opts = rootflags.unwrap_or("subvol=@");

    klog!("  btrfs: mounting {} → {} ({})", device, target, opts);

    mount(Some(device), target, Some("btrfs"), flags, Some(opts)).map_err(|e| {
        format!(
            "btrfs mount '{}' → '{}' (opts={}): {}",
            device, target, opts, e
        )
    })?;

    klog!("  ✓ btrfs subvolume mounted");
    Ok(())
}

fn find_fsck_binary(fstype: Option<&str>) -> Option<String> {
    // First try fsck.<type> (e.g. fsck.ext4, fsck.xfs)
    if let Some(ft) = fstype {
        for dir in &[
            "/sbin",
            "/usr/sbin",
            "/overlayer/syshub/sbin",
            "/overlayer/syshub/bin",
        ] {
            let specific = format!("{}/fsck.{}", dir, ft);
            if Path::new(&specific).exists() {
                return Some(specific);
            }
        }
    }

    // Then try generic fsck
    for &path in FSCK_PATHS {
        if Path::new(path).exists() {
            return Some(path.to_string());
        }
    }

    None
}
