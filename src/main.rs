/// eprintln! routed through `log`: held until `quiet` is known, then console or kmsg.
macro_rules! klog {
    () => { $crate::log::line(String::new()) };
    ($($arg:tt)*) => { $crate::log::line(format!($($arg)*)) };
}

mod cmdline;
mod emergency;
mod fsck;
mod log;
mod measured_boot;
mod mounts;
mod network_boot;
mod overlay;
mod pending;
mod phases;
mod plymouth;
mod raid;
mod rootfs;
mod switch;
mod tpm2;
mod udev;
mod verity;

use cmdline::RdBreak;
use phases::{BootPhase, set_phase};
use std::time::{Duration, Instant};

/// Status line; on a quiet boot it lands in dmesg/journal instead of the screen.
fn ok(msg: impl std::fmt::Display) {
    klog!("  ✓ {msg}");
}

/// Non-fatal warning line — boot continues.
fn warn(msg: impl std::fmt::Display) {
    klog!("  ⚠ {msg}");
}

/// Failure that is about to drop to the emergency shell: always on the real console.
fn fail(msg: impl std::fmt::Display) {
    restore_console_stderr();
    klog!("  ✗ {msg}");
}

/// Send all output (ours and children's) to the kernel log.
fn redirect_stderr_to_kmsg() {
    log::to_kmsg();
}

/// Put output back on the real console (fatal errors, emergency shell).
fn restore_console_stderr() {
    log::to_console();
}

/// `quiet` or `splash` keeps the screen clean like Fedora/Ubuntu; logs stay in dmesg/journal.
fn quiet_boot() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|c| c.split_whitespace().any(|t| t == "quiet" || t == "splash"))
        .unwrap_or(false)
}

/// Zainium Initramfs — 8 phases: INIT, MOUNTS, CMDLINE, ROOTFS_DETECT, ROOTFS_MOUNT, OVERLAY, PIVOT, COMPLETE.
fn main() -> ! {
    let boot_start = Instant::now();

    set_phase(BootPhase::INIT);

    // Phase 1: Early mounts
    set_phase(BootPhase::MOUNTS);
    if let Err(e) = mounts::mount_early() {
        fail(format!("Early mounts failed: {e}"));
        emergency::shell("early mount failed");
    }
    // /proc is up: everything held so far goes to kmsg on a quiet boot, else to the console
    if quiet_boot() {
        redirect_stderr_to_kmsg();
    } else {
        restore_console_stderr();
    }
    ok("Mounted early filesystems");

    if plymouth::is_enabled() {
        plymouth::setup_kms_framebuffer();
        if plymouth::start(None) {
            // Plymouth owns the display now; emergency::shell() brings the console back
            redirect_stderr_to_kmsg();
        }
    }

    // vconsole keymap in initrd
    plymouth::setup_initrd_vconsole();

    // Secure Boot check
    let _sb_status = plymouth::check_secure_boot();

    // Measure cmdline now that /proc is available
    measured_boot::measure_cmdline();

    // udev: get /dev nodes for devices present at boot time
    let udev_pre_count = udev::settle(Duration::from_millis(500));
    if udev_pre_count > 0 {
        ok(format!("udev: {udev_pre_count} device(s) detected"));
    }

    // Live medium probe, only when asked for: the live ISO's entry passes loop=, an installed
    // system passes root= and must never be swapped for a live USB that is still plugged in.
    // Retry with fresh udev settles, USB storage is slow to enumerate.
    let raw_cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let has = |key: &str| raw_cmdline.split_whitespace().any(|t| t.starts_with(key));
    let wants_live = has("loop=") || !has("root=");
    let mut live_result = if wants_live {
        rootfs::prepare_live_medium_bridge()
    } else {
        Ok(None)
    };
    if wants_live && matches!(live_result, Ok(None)) {
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(300));
            udev::settle(Duration::from_millis(300));
            live_result = rootfs::prepare_live_medium_bridge();
            if !matches!(live_result, Ok(None)) {
                break;
            }
        }
    }
    let live_image: Option<std::path::PathBuf> = match live_result {
        Ok(Some(path)) => {
            ok(format!("Live medium: {}", path.display()));
            Some(path)
        }
        Ok(None) => None,
        Err(e) => {
            warn(format!("Live medium probe: {e}"));
            None
        }
    };

    // Phase 2: Parse cmdline
    set_phase(BootPhase::CMDLINE);
    let cmdline = match cmdline::parse() {
        Ok(c) => c,
        Err(e) => {
            fail(format!("Cmdline parse failed: {e}"));
            emergency::shell("cmdline parse");
        }
    };

    // rd.shell — unconditional emergency shell (useful for debugging)
    if cmdline.rd_shell {
        ok("rd.shell: dropping to emergency shell (cmdline request)");
        emergency::shell("rd.shell");
    }

    // rd.rescue / single — set init to rescue target
    if cmdline.rd_rescue {
        ok("Rescue mode — will boot to an emergency shell");
    }

    // Phase 3: Detect root device
    set_phase(BootPhase::ROOTFS_DETECT);

    // rd.break=pre-mount — drop to shell before we touch the disk
    if cmdline.rd_break == Some(RdBreak::PreMount) {
        ok("rd.break=pre-mount: dropping to emergency shell");
        emergency::shell("rd.break=pre-mount");
    }

    // Extra udev settle if udev is enabled — wait for slow devices
    if cmdline.udev_enabled {
        let rootwait_ms = match cmdline.rootwait {
            Some(0) => 10_000, // bare rootwait — give udev 10s
            Some(n) => n as u64 * 50,
            None => 1_000, // default 1s
        };
        let extra = udev::settle(Duration::from_millis(rootwait_ms.min(3_000)));
        if extra > 0 {
            ok(format!("udev: +{extra} device(s) after settle"));
        }
        // Build /dev/disk/by-uuid and /dev/disk/by-label symlinks
        udev::create_disk_symlinks();
    }

    // DHCP in initrd (for NFS root, iSCSI, NBD, HTTP boot)
    if cmdline.ip_dhcp {
        let iface = cmdline.ip_iface.as_deref();
        let lease = if let Some(iface) = iface {
            network_boot::dhcp_acquire(iface, std::time::Duration::from_secs(10)).ok()
        } else {
            network_boot::dhcp_any_interface(std::time::Duration::from_secs(10))
        };
        if lease.is_some() {
            ok("Network: DHCP lease acquired");
        } else {
            warn("Network: DHCP failed");
        }
    }

    // IPv6 SLAAC
    if cmdline.ipv6_slaac {
        for iface in network_boot::list_interfaces() {
            network_boot::enable_ipv6_slaac(&iface).ok();
        }
    }

    // MD RAID assembly
    if cmdline.rd_md {
        match raid::assemble_raid(cmdline.rd_md_uuid.as_deref()) {
            Ok(n) => ok(format!("MD RAID: {n} array(s)")),
            Err(e) => warn(format!("MD RAID: {e}")),
        }
        raid::wait_for_raid_sync(std::time::Duration::from_secs(30));
    }

    // LVM activation
    if cmdline.rd_lvm {
        match raid::activate_lvm(cmdline.rd_lvm_vg.as_deref()) {
            Ok(n) => ok(format!("LVM: {n} volume(s)")),
            Err(e) => warn(format!("LVM: {e}")),
        }
    }

    // Device multipath
    if cmdline.rd_multipath {
        match raid::activate_multipath() {
            Ok(()) => ok("Multipath configured"),
            Err(e) => warn(format!("Multipath: {e}")),
        }
    }

    // Stratis pool unlock
    if let Some(ref uuid) = cmdline.stratis_pool_uuid {
        raid::unlock_stratis_pool(Some(uuid.as_str())).ok();
    }

    // iSCSI root
    if let Some(ref target_name) = cmdline.iscsi_target_name {
        if let Some(ref target_ip) = cmdline.iscsi_target_ip {
            let iscsi = network_boot::IscsiTarget {
                initiator_iqn: cmdline
                    .iscsi_initiator
                    .clone()
                    .unwrap_or_else(|| "iqn.2026-01.os.zainium:initrd".to_string()),
                target_iqn: target_name.clone(),
                target_ip: target_ip.clone(),
                target_port: cmdline.iscsi_target_port,
            };
            match network_boot::connect_iscsi(&iscsi) {
                Ok(dev) => ok(format!("iSCSI: {dev}")),
                Err(e) => warn(format!("iSCSI: {e}")),
            }
        }
    }

    // NBD root
    if let Some(ref nbd_spec) = cmdline.nbd {
        let parts: Vec<&str> = nbd_spec.splitn(2, ':').collect();
        if parts.len() == 2 {
            if let Ok(port) = parts[1].parse::<u16>() {
                match network_boot::connect_nbd(parts[0], port, None) {
                    Ok(dev) => ok(format!("NBD: {dev}")),
                    Err(e) => warn(format!("NBD: {e}")),
                }
            }
        }
    }

    // Phase 4: Mount root filesystem
    set_phase(BootPhase::ROOTFS_MOUNT);

    let zairoot = switch::find_mount_target(&cmdline).unwrap_or_else(|| {
        fail("No usable mount target found");
        emergency::shell("no mount target");
    });

    if let Some(image) = &live_image {
        if let Err(e) = rootfs::mount_live_root(image, &cmdline, zairoot) {
            fail(format!("Live root mount failed: {e}"));
            emergency::shell("live root mount");
        }
    } else {
    let root_device = match rootfs::find_root(&cmdline) {
        Ok(dev) => {
            ok(format!("Root device: {dev:?}"));
            dev
        }
        Err(e) => {
            fail(format!("Root device not found: {e}"));
            emergency::shell("root detect");
        }
    };

    // fsck before mounting (skipped for squashfs/iso/loop/NFS)
    if cmdline.fsck_enabled {
        let fstype_ref = cmdline.root_fstype.as_deref();
        if let Err(e) = fsck::check_root(&root_device, fstype_ref) {
            if e.contains("REBOOT REQUIRED") {
                warn("Filesystem check failed — rebooting in 3s");
                std::thread::sleep(Duration::from_secs(3));
                unsafe {
                    libc::sync();
                    libc::reboot(libc::RB_AUTOBOOT);
                }
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                }
            } else {
                warn(format!("Filesystem check: {e} — continuing"));
            }
        }
    }

    // dm-verity: set up verified device before mounting
    let actual_root_device = if cmdline.verity_enabled {
        match setup_verity_device(&cmdline, &root_device) {
            Ok(verified_dev) => {
                ok(format!("dm-verity: {verified_dev}"));
                verified_dev
            }
            Err(e) => {
                fail(format!("dm-verity setup failed: {e}"));
                emergency::shell("dm-verity failed");
            }
        }
    } else {
        root_device.clone()
    };

    // Mount root
    if let Err(e) = rootfs::mount_root_at(&actual_root_device, &cmdline, zairoot) {
        fail(format!("Root mount failed: {e}"));
        emergency::shell("root mount");
    }
    }
    ok(format!("Mounted root filesystem → {zairoot}"));

    // rd.break=pre-overlay — shell after disk mount, before OverlayFS
    if cmdline.rd_break == Some(RdBreak::PreOverlay) {
        ok("rd.break=pre-overlay: dropping to emergency shell");
        emergency::shell("rd.break=pre-overlay");
    }

    // Phase 4.5: OverlayFS → /new_root
    set_phase(BootPhase::OVERLAY);

    let overlay_enabled = !overlay::overlay_disabled_by_cmdline();
    measured_boot::measure_overlay_mode(overlay_enabled);

    let new_root = if !overlay_enabled {
        ok("OverlayFS disabled (zainium.overlay=off) — syshub only");
        zairoot.to_string()
    } else {
        match overlay::mount_overlay(zairoot) {
            Ok(r) => {
                ok(format!("OverlayFS mounted → {r}"));
                r
            }
            Err(e) => {
                warn(format!("OverlayFS failed: {e} — read-only fallback to {zairoot}"));
                zairoot.to_string()
            }
        }
    };

    // Phase 5: Discover init binary (rescue mode overrides to bash, Zainium has no /bin/sh)
    let boot_target = if cmdline.rd_rescue {
        switch::rescue_boot_target()
    } else {
        switch::discover_boot_target(&cmdline, &new_root)
    };

    ok(format!("Starting {}", boot_target.init_path));

    // rd.break=pre-pivot — last chance before we hand over to PID 1
    if cmdline.rd_break == Some(RdBreak::PrePivot) {
        ok("rd.break=pre-pivot: dropping to emergency shell");
        emergency::shell("rd.break=pre-pivot");
    }

    // Measured boot: record the exact init binary
    measured_boot::measure_init_binary(&new_root, boot_target.init_path);

    // Plymouth: signal root mounted before pivot
    plymouth::root_mounted();
    plymouth::update_status("initrd-done");

    // Phase 6: pivot_root → execv init
    set_phase(BootPhase::PIVOT);
    if let Err(e) = switch::pivot_to_root(&boot_target) {
        fail(format!("switch_root failed: {e}"));
        emergency::shell("switch_root");
    }
    plymouth::handoff_to_init();

    // Complete
    set_phase(BootPhase::COMPLETE);
    ok(format!("Boot complete ({}ms)", boot_start.elapsed().as_millis()));

    emergency::shell("execv returned unexpectedly");
}

// dm-verity setup helper

/// Parse verity config from cmdline, set up dm-verity, return `/dev/mapper/zainium-verity`.
fn setup_verity_device(cmdline: &cmdline::Cmdline, root_device: &str) -> Result<String, String> {
    match verity::parse_verity_cmdline(&cmdline.raw) {
        None => Err("rd.verity=1 set but no verity params found in cmdline".to_string()),
        Some(Err(e)) => Err(format!("verity cmdline parse: {}", e)),
        Some(Ok(mut cfg)) => {
            // If data device not specified in verity params, use root device
            if cfg.data_device.is_empty() {
                cfg.data_device = root_device.to_string();
            }
            let dev = verity::setup_verity(&cfg)?;
            Ok(dev.path)
        }
    }
}
