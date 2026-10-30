/// Joins the layers with OverlayFS into the view the system runs on (/overlayer/syshub).
use nix::mount::{MsFlags, mount};
use std::fs;

/// Mounts the overlay at /new_root/overlayer/syshub over the disk mounted at `zairoot`.
pub fn mount_overlay(zairoot: &str) -> Result<String, String> {
    // leftmost lower wins: syshub over zaisys
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

    // the live image is read-only, so its zexlib lives on a tmpfs for the session
    let phys_upper = format!("{}/overlayer/zexlib/union", zairoot);
    let phys_work = format!("{}/overlayer/zexlib/work", zairoot);
    let phys_zexlib = format!("{}/overlayer/zexlib", zairoot);
    let (upper, work, zexlib_src) = match fs::create_dir_all(&phys_upper)
        .and_then(|_| fs::create_dir_all(&phys_work))
    {
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

    // the root holds only overlayer, home, root and the virtual filesystems; switch.rs adds run
    fs::create_dir_all(&merged).map_err(|e| format!("create {}: {}", merged, e))?;
    for name in ["home", "root", "dev", "proc", "sys", "tmp"] {
        let dir = format!("{}/{}", root, name);
        fs::create_dir_all(&dir).map_err(|e| format!("create {}: {}", dir, e))?;
    }

    // systemd's StateDirectory= and friends need a real /var: link it to the base's var
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

    // home and root are bind mounts, so a zexlib rollback never touches user data
    if zexlib_src == "/run/zexlib" {
        // Live boot: zairoot is the read-only image, so give home/root a tmpfs upper layer.
        overlay_live_dir(zairoot, "home")?;
        overlay_live_dir(zairoot, "root")?;
    } else {
        bind_persistent_dir(zairoot, "home")?;
        bind_persistent_dir(zairoot, "root")?;
    }

    // zaisys and zexlib also show at their own paths under /overlayer, beside the merged syshub
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
    // every other overlayer entry on the disk (agents, the docs, links) shows at its own path too
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

/// Bind mounts `src` on `dest` read-only (a bind mount needs a second, read-only remount).
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

/// Bind mounts /zairoot/<name> on /new_root/<name>, outside the overlay, so a zexlib rollback never touches it.
fn bind_persistent_dir(zairoot: &str, name: &str) -> Result<(), String> {
    let src = format!("{}/{}", zairoot, name);
    let dest = format!("/new_root/{}", name);

    if !std::path::Path::new(&src).exists() {
        klog!("  /{}: '{}' not found — skipping bind mount", name, src);
        return Ok(());
    }

    // mount_overlay made it already
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

/// True when the cmdline has zainium.overlay=off: the rescue boot straight from syshub.
pub fn overlay_disabled_by_cmdline() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|s| s.split_whitespace().any(|tok| tok == "zainium.overlay=off"))
        .unwrap_or(false)
}
