/// OverlayFS — Zainium "Immutable Merge"
///
/// # Purpose
///
/// After the physical disk is mounted at `/zairoot`, this module constructs
/// the live root filesystem that PID 1 (systemd, or `quantra` as a fallback
/// — see `switch.rs`'s `INIT_FALLBACKS`) will see by merging three layers
/// via the Linux OverlayFS kernel driver.
///
/// # Architecture — "Option B" (no flattened FHS at root)
///
/// Unlike a traditional initramfs `switch_root`, Zainium does **not** flatten
/// syshub's own top level (bin/, lib/, etc/, engine/, …) onto `/`. Packages
/// (e.g. GCC) are built with `--prefix=/overlayer/syshub` — see
/// `zex-env/src/paths.rs` — so that prefix must be a real, live path after
/// pivot, not just before it on the physical disk.
///
/// `/new_root` (tmpfs, becomes `/` after `pivot_root`) holds
/// `overlayer/{syshub,zaisys,zexlib}` (merge target + bind mounts),
/// `home`/`root` (bind-mounted from `/zairoot`), `dev`/`proc`/`sys`/`run`
/// (MS_MOVE'd in `switch.rs`), and `tmp` (mounted later by PID 1).
///
/// `/new_root/overlayer/syshub` is the merged view: lowerdir[0] =
/// `/zairoot/overlayer/syshub` (immutable base), lowerdir[1] =
/// `/zairoot/overlayer/zaisys` (kernel/early-boot), upperdir =
/// `.../zexlib/union` (writable), workdir = `.../zexlib/work`.
///
/// There is deliberately **no** `/bin`, `/sbin`, `/usr`, `/lib`, `/etc` or
/// `/var` at the root of `/new_root` — `zex-env/src/paths.rs::FORBIDDEN_FHS`
/// enforces this same rule for the toolchain side. `ls /` after boot shows
/// only `overlayer`, `home`, `root`, `dev`, `proc`, `sys`, `run`, `tmp`.
///
/// `ls /overlayer` shows all three siblings — `syshub`, `zaisys`, `zexlib` —
/// matching the real on-disk layout. `zaisys` and `zexlib` are *also* bind
/// mounted at their own paths (in addition to feeding the merge below), so
/// they're never hidden — only their **content** gets linked into the
/// unified `syshub` view via OverlayFS.
///
/// **No compatibility symlink for `/etc` (or anything else).** An earlier
/// version of this module recreated `/etc -> /overlayer/syshub/etc` in
/// `/new_root` on every boot; that has been deliberately removed. Every
/// piece of Zainium-owned code (`quantra`, `quantra-logind`, …) references
/// `/overlayer/syshub/etc/...`, `/overlayer/syshub/var/...` etc. by its full
/// explicit path — there is no root-level shim of any kind. `/var` merges
/// into the overlay the same way `/etc` does (COW into `zexlib/union`); it
/// is **not** a third bind mount alongside `/home` and `/root`.
///
/// # OverlayFS Semantics
///
/// Reads fall through to lowerdir if not in upperdir. Adds/modifies
/// (copy-on-write) go to upperdir (`zexlib/union`). Deletes leave a
/// whiteout in upperdir; lowerdir is never touched.
///
/// The syshub (lowerdir[0]) is listed first → it takes priority over zaisys.
/// This means any file present in both syshub and zaisys is served from syshub.
///
/// Note: because upperdir/workdir (`zexlib/union`, `zexlib/work`) cannot live
/// inside the merge they contribute to, they stay physically under
/// `/zairoot/overlayer/zexlib/...` — a package installed by `zex` therefore
/// appears at `/overlayer/syshub/bin/foo` in the live view while physically
/// landing at `/zairoot/overlayer/zexlib/union/bin/foo` on disk. This is
/// expected OverlayFS behaviour, not a bug.
///
/// # /home Bind Mount
///
/// `/zairoot/home` is bind-mounted to `/new_root/home` separately.
/// This ensures user data survives a full zexlib rollback (resetting upperdir)
/// without being affected by the overlay layer mechanism.
///
/// # Rescue Mode
///
/// If the kernel cmdline contains `zainium.overlay=off`, `overlay_disabled_by_cmdline()`
/// returns `true` and main.rs skips this module entirely, booting the physical
/// /zairoot mount point directly (syshub read-only, no zexlib packages). In
/// that case `new_root` becomes `/zairoot` itself, which already has
/// `overlayer/syshub` as a real subdirectory — `switch.rs`'s init-fallback
/// paths are prefixed with `/overlayer/syshub` precisely so this case keeps
/// working without a separate code path.
use nix::mount::{MsFlags, mount};
use std::fs;

/// Mount the Zainium OverlayFS at `/new_root/overlayer/syshub`.
///
/// Caller must have already mounted the physical disk partition at `zairoot`
/// (e.g. `/zairoot`). After this function returns `Ok`, the process should
/// search for and exec the init binary from `/new_root/overlayer/syshub/...`
/// (see `switch.rs`, whose `INIT_FALLBACKS` already carry that prefix).
///
/// # Arguments
/// * `zairoot` — Path where the physical disk is mounted (e.g. `"/zairoot"`)
///
/// # Returns
/// The path of the tmpfs root (`"/new_root"`) on success — i.e. the
/// mountpoint that will become `/` after `pivot_root`, **not** the overlay
/// merge subdirectory itself.
pub fn mount_overlay(zairoot: &str) -> Result<String, String> {
    // OverlayFS lowerdir: colon-separated, leftmost = highest priority
    // syshub (base OS) overrides zaisys (kernel/early-boot) when files collide.
    let lower = format!("{z}/overlayer/syshub:{z}/overlayer/zaisys", z = zairoot);
    let root = "/new_root".to_string();
    let merged = format!("{}/overlayer/syshub", root);

    // Sanity check — ensure the Zainium disk layout is present
    if !std::path::Path::new(&format!("{}/overlayer/syshub", zairoot)).exists() {
        return Err(format!(
            "syshub not found in '{}' — not a Zainium disk (overlayer/syshub missing)",
            zairoot
        ));
    }

    // upperdir + workdir normally live physically under zairoot's own
    // overlayer/zexlib (writable installed-disk case). On a live boot,
    // zairoot itself is the read-only loop-mounted image, so that path
    // can't be created — fall back to a tmpfs-backed zexlib instead
    // (ephemeral, matches how any live distro handles this: writable for
    // the session, gone on reboot). Only syshub/zaisys are meant to be
    // read-only; zexlib is always meant to be writable.
    let phys_upper = format!("{}/overlayer/zexlib/union", zairoot);
    let phys_work = format!("{}/overlayer/zexlib/work", zairoot);
    let phys_zexlib = format!("{}/overlayer/zexlib", zairoot);
    let (upper, work, zexlib_src) = match fs::create_dir_all(&phys_upper).and_then(|_| fs::create_dir_all(&phys_work)) {
        Ok(()) => {
            // staged `zex syshub -u` upgrades go into the base here, while nothing has it mounted
            crate::pending::run(zairoot);
            crate::baseverity::ensure_base(zairoot);
            (phys_upper, phys_work, phys_zexlib)
        }
        Err(e) => {
            klog!(
                "  zexlib not writable on {} ({e}) — using tmpfs zexlib (live session, not persistent)",
                zairoot
            );
            let (u, w) = tmpfs_zexlib()?;
            (u, w, "/run/zexlib".to_string())
        }
    };

    // Base tmpfs root layout: only overlayer/syshub, home, root, and the
    // standard virtual filesystem mountpoints live at the true root.
    // No bin/sbin/usr/lib/etc. `run` is deliberately not created here
    // — switch.rs's MS_MOVE step creates it when it moves the initramfs's
    // own /run into place, right before pivot_root.
    fs::create_dir_all(&merged).map_err(|e| format!("create {}: {}", merged, e))?;
    for name in ["home", "root", "dev", "proc", "sys", "tmp"] {
        let dir = format!("{}/{}", root, name);
        fs::create_dir_all(&dir).map_err(|e| format!("create {}: {}", dir, e))?;
    }

    // systemd's StateDirectory=/CacheDirectory=/LogsDirectory= (used by
    // logind.service and plenty of other units) always resolve against
    // the real root's /var/lib etc, completely independent of any
    // --localstatedir build option -- confirmed live: systemd-logind
    // crash-looped with "Failed to set up special execution directory
    // in /var/lib: No such file or directory" because there's no real
    // /var at all at Zainium's true root. A plain symlink is enough for
    // every unit that will ever hit this, not just logind.
    let var_link = format!("{}/var", root);
    if !std::path::Path::new(&var_link).exists() {
        std::os::unix::fs::symlink("/overlayer/syshub/var", &var_link)
            .map_err(|e| format!("symlink {} -> /overlayer/syshub/var: {}", var_link, e))?;
    }

    let opts = format!("lowerdir={},upperdir={},workdir={}", lower, upper, work);

    klog!("  OverlayFS (Immutable Merge):");
    klog!("    lower[0] = {}/overlayer/syshub  (priority)", zairoot);
    klog!("    lower[1] = {}/overlayer/zaisys", zairoot);
    klog!(
        "    upper    = {}/overlayer/zexlib/union  (writable)",
        zairoot
    );
    klog!("    merged   = {}", merged);

    mount(
        Some("overlay"),
        merged.as_str(),
        Some("overlay"),
        MsFlags::empty(),
        Some(opts.as_str()),
    )
    .map_err(|e| {
        format!(
            "OverlayFS mount failed: {} — is the 'overlay' kernel module loaded?",
            e
        )
    })?;

    klog!("  ✓ OverlayFS → {}", merged);

    // Bind mount /home and /root separately so user data is not part of the
    // writable overlay — both must survive a zexlib rollback untouched.
    if zexlib_src == "/run/zexlib" {
        // Live boot: zairoot is the read-only image, so give home/root a tmpfs upper layer.
        overlay_live_dir(zairoot, "home")?;
        overlay_live_dir(zairoot, "root")?;
    } else {
        bind_persistent_dir(zairoot, "home")?;
        bind_persistent_dir(zairoot, "root")?;
    }

    // Also expose zaisys and zexlib as their own visible directories under
    // /overlayer, alongside the merged syshub — matching the real Zainium
    // layout (overlayer/{syshub,zaisys,zexlib}, three siblings). The merge
    // above already links zexlib/union's content into the syshub view; this
    // additionally makes zaisys/zexlib reachable at their own paths too,
    // instead of only existing invisibly as merge inputs.
    bind_ro_dir(
        &format!("{}/overlayer/zaisys", zairoot),
        &format!("{}/overlayer/zaisys", root),
    )?;
    bind_dir(&zexlib_src, &format!("{}/overlayer/zexlib", root))?;
    // root-only staging area for `zex syshub -u` (installed disk only, created by pending::run)
    bind_dir(
        &format!("{}/overlayer/.syshub-pending", zairoot),
        &format!("{}/overlayer/.syshub-pending", root),
    )?;
    // every other overlayer entry on the disk shows at its own path too: directories
    // (agents, ...), files (README.zainium, architecture.md, zex.md, ...) and symlinks
    if let Ok(entries) = fs::read_dir(format!("{}/overlayer", zairoot)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || matches!(name.as_str(), "syshub" | "zaisys" | "zexlib") {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let src = format!("{}/overlayer/{}", zairoot, name);
            let dest = format!("{}/overlayer/{}", root, name);
            if ft.is_dir() {
                bind_dir(&src, &dest)?;
            } else if ft.is_file() {
                // the docs are read-only at runtime; a missing doc file must never stop the boot
                if let Err(e) = bind_ro_file(&src, &dest) {
                    klog!("  warning: {}", e);
                }
            } else if ft.is_symlink() {
                if let Ok(target) = fs::read_link(&src) {
                    let _ = std::os::unix::fs::symlink(target, &dest);
                }
            }
        }
    }

    Ok(root)
}

/// Bind mount `src` → `dest` (writable), creating `dest` if needed.
fn bind_dir(src: &str, dest: &str) -> Result<(), String> {
    if !std::path::Path::new(src).exists() {
        klog!("  '{}' not found — skipping bind mount", src);
        return Ok(());
    }
    fs::create_dir_all(dest).map_err(|e| format!("create {}: {}", dest, e))?;
    mount(
        Some(src),
        dest,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| format!("bind mount '{}' → '{}': {}", src, dest, e))?;
    klog!("  ✓ {} bind mounted ← {}", dest, src);
    Ok(())
}

/// Bind mount the file `src` onto an empty file created at `dest`.
fn bind_ro_file(src: &str, dest: &str) -> Result<(), String> {
    fs::File::create(dest).map_err(|e| format!("create {}: {}", dest, e))?;
    mount(
        Some(src),
        dest,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| format!("bind mount '{}' → '{}': {}", src, dest, e))?;
    mount(
        None::<&str>,
        dest,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .map_err(|e| format!("remount ro '{}': {}", dest, e))?;
    klog!("  ✓ {} bind mounted ← {} (read-only)", dest, src);
    Ok(())
}

/// Bind mount `src` → `dest` read-only (two-step: bind, then remount ro —
/// required by Linux, a plain bind mount inherits write access otherwise).
fn bind_ro_dir(src: &str, dest: &str) -> Result<(), String> {
    if !std::path::Path::new(src).exists() {
        klog!("  '{}' not found — skipping bind mount", src);
        return Ok(());
    }
    fs::create_dir_all(dest).map_err(|e| format!("create {}: {}", dest, e))?;
    mount(
        Some(src),
        dest,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| format!("bind mount '{}' → '{}': {}", src, dest, e))?;
    mount(
        None::<&str>,
        dest,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .map_err(|e| format!("remount ro '{}': {}", dest, e))?;
    klog!("  ✓ {} bind mounted ← {} (read-only)", dest, src);
    Ok(())
}

/// Bind mount `/zairoot/{name}` → `/new_root/{name}` (used for `home` and `root`).
///
/// These directories must survive a zexlib rollback (which resets upperdir).
/// Keeping them outside the overlay layer guarantees that: a rollback wipes
/// installed packages but never touches user data.
fn bind_persistent_dir(zairoot: &str, name: &str) -> Result<(), String> {
    let src = format!("{}/{}", zairoot, name);
    let dest = format!("/new_root/{}", name);

    if !std::path::Path::new(&src).exists() {
        klog!("  /{}: '{}' not found — skipping bind mount", name, src);
        return Ok(());
    }

    // Destination directory must already exist inside /new_root (created by
    // mount_overlay's base-dirs pass above); create_dir_all is idempotent.
    fs::create_dir_all(&dest).ok();

    mount(
        Some(src.as_str()),
        dest.as_str(),
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| format!("bind mount /{}: '{}' → '{}': {}", name, src, dest, e))?;

    klog!("  ✓ /{} bind mounted (persistent, survives rollback)", name);
    Ok(())
}

/// Writable overlay of `/zairoot/{name}` over a tmpfs upper layer (live boot, not persistent).
fn overlay_live_dir(zairoot: &str, name: &str) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let src = format!("{}/{}", zairoot, name);
    let dest = format!("/new_root/{}", name);
    let meta = match fs::metadata(&src) {
        Ok(m) => m,
        Err(_) => {
            klog!("  /{}: '{}' not found — skipping", name, src);
            return Ok(());
        }
    };

    let upper = format!("/run/zexlib/{}-upper", name);
    let work = format!("/run/zexlib/{}-work", name);
    for dir in [&upper, &work] {
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {}", dir, e))?;
    }
    // The merged root takes the upper dir's owner and mode, so copy them from the image.
    fs::set_permissions(&upper, fs::Permissions::from_mode(meta.mode() & 0o7777))
        .map_err(|e| format!("chmod {}: {}", upper, e))?;
    std::os::unix::fs::chown(&upper, Some(meta.uid()), Some(meta.gid()))
        .map_err(|e| format!("chown {}: {}", upper, e))?;

    fs::create_dir_all(&dest).ok();
    let opts = format!("lowerdir={},upperdir={},workdir={}", src, upper, work);
    mount(
        Some("overlay"),
        dest.as_str(),
        Some("overlay"),
        MsFlags::empty(),
        Some(opts.as_str()),
    )
    .map_err(|e| format!("overlay /{}: {}", name, e))?;

    klog!("  ✓ /{} overlay (live, writable, not persistent)", name);
    Ok(())
}

/// tmpfs-backed zexlib fallback for a read-only zairoot (live boot).
fn tmpfs_zexlib() -> Result<(String, String), String> {
    let base = "/run/zexlib";
    fs::create_dir_all(base).map_err(|e| format!("create {}: {}", base, e))?;
    mount(
        Some("tmpfs"),
        base,
        Some("tmpfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .map_err(|e| format!("mount tmpfs at {}: {}", base, e))?;
    let upper = format!("{}/union", base);
    let work = format!("{}/work", base);
    for dir in [&upper, &work] {
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {}", dir, e))?;
    }
    Ok((upper, work))
}

/// Check whether the kernel cmdline contains `zainium.overlay=off`.
///
/// This is the rescue boot mechanism: add `zainium.overlay=off` to the
/// bootloader entry (Limine, GRUB) to skip the OverlayFS layer and boot
/// directly from the immutable syshub inside /zairoot.
///
/// Returns `true` if overlay should be skipped, `false` otherwise.
/// If `/proc/cmdline` cannot be read (pre-Phase1 call), returns `false`.
pub fn overlay_disabled_by_cmdline() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|s| s.split_whitespace().any(|tok| tok == "zainium.overlay=off"))
        .unwrap_or(false)
}
