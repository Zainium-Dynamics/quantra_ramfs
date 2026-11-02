/// Joins the layers with OverlayFS into the view the system runs on (/overlayer/syshub).
use nix::mount::{MsFlags, mount};
use std::fs;

/// Mounts the system view at /new_root/overlayer/syshub over the disk mounted at `zairoot`.
///
/// syshub is a sealed, read-only overlay of the base with the userland (zexlib/union) on top:
/// nothing writes through it, root included, while zex installs straight into the union.
/// etc and var are overlays of their own whose writes go to union/etc and union/var.
/// `live` keeps the userland on a tmpfs; `rescue` leaves it out (base only).
pub fn mount_overlay(zairoot: &str, live: bool, rescue: bool) -> Result<String, String> {
    let base = format!("{zairoot}/overlayer/syshub");
    let root = "/new_root".to_string();
    let merged = format!("{root}/overlayer/syshub");
    if !std::path::Path::new(&base).is_dir() {
        return Err(format!("{base} missing: not a Zainium disk"));
    }

    let zexlib = if live || rescue {
        tmpfs_zexlib()?
    } else {
        let zexlib = format!("{zairoot}/overlayer/zexlib");
        writable(&zexlib)?;
        // staged `zex syshub -u` upgrades go into the base here, while nothing has it mounted
        crate::pending::run(zairoot).map_err(|e| format!("base upgrade: {e}"))?;
        crate::baseverity::ensure_base(zairoot).map_err(|e| format!("fs-verity: {e}"))?;
        zexlib
    };
    for d in ["union/etc", "union/var", "work", "work-etc", "work-var"] {
        let dir = format!("{zexlib}/{d}");
        fs::create_dir_all(&dir).map_err(|e| format!("create {dir}: {e}"))?;
    }

    // the root holds only overlayer, home, root and the virtual filesystems; switch.rs adds run
    fs::create_dir_all(&merged).map_err(|e| format!("create {merged}: {e}"))?;
    for name in ["home", "root", "dev", "proc", "sys", "tmp"] {
        let dir = format!("{root}/{name}");
        fs::create_dir_all(&dir).map_err(|e| format!("create {dir}: {e}"))?;
    }
    // services expect /var: it is the base's var, written to the union
    let var_link = format!("{root}/var");
    if !std::path::Path::new(&var_link).exists() {
        std::os::unix::fs::symlink("/overlayer/syshub/var", &var_link)
            .map_err(|e| format!("symlink {var_link}: {e}"))?;
    }

    let opts = if rescue {
        format!("lowerdir={base}")
    } else {
        format!("lowerdir={base},upperdir={zexlib}/union,workdir={zexlib}/work,sealed")
    };
    mount_sealed(&merged, &opts)?;
    for d in ["etc", "var"] {
        // union/{d} is inside the syshub overlay's upper dir, whose view of it this mount covers:
        // only the exclusive-upper check (index=on) stands in the way
        let opts = format!(
            "lowerdir={base}/{d},upperdir={zexlib}/union/{d},workdir={zexlib}/work-{d},index=off"
        );
        let target = format!("{merged}/{d}");
        mount(
            Some("overlay"),
            target.as_str(),
            Some("overlay"),
            MsFlags::empty(),
            Some(opts.as_str()),
        )
        .map_err(|e| format!("overlay {target}: {e}"))?;
    }
    klog!("  ✓ /overlayer/syshub read-only, etc and var written to {zexlib}/union");

    let zexlib_src = zexlib;
    // home and root are bind mounts, so a zexlib rollback never touches user data
    if live {
        // Live boot: zairoot is the read-only image, so give home/root a tmpfs upper layer.
        overlay_live_dir(zairoot, "home")?;
        overlay_live_dir(zairoot, "root")?;
    } else {
        bind_persistent_dir(zairoot, "home")?;
        bind_persistent_dir(zairoot, "root")?;
    }

    // zaisys stays out of the syshub view: its own read-only mount, the only one left after the pivot
    bind_ro_dir(
        &format!("{}/overlayer/zaisys", zairoot),
        &format!("{}/overlayer/zaisys", root),
    )?;
    bind_dir(&zexlib_src, &format!("{}/overlayer/zexlib", root))?;
    // root-only staging area for `zex syshub -u` (installed disk only, created by pending::run)
    if !live && !rescue {
        bind_dir(
            &format!("{}/overlayer/.syshub-pending", zairoot),
            &format!("{}/overlayer/.syshub-pending", root),
        )?;
    }
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

/// Mounts the read-only system view; a kernel without sealed overlays gets a plain read-only one.
fn mount_sealed(target: &str, opts: &str) -> Result<(), String> {
    let ro = MsFlags::MS_RDONLY;
    match mount(Some("overlay"), target, Some("overlay"), ro, Some(opts)) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::EINVAL) if opts.ends_with(",sealed") => {
            let plain = opts.trim_end_matches(",sealed");
            mount(Some("overlay"), target, Some("overlay"), ro, Some(plain))
                .map_err(|e| format!("overlay {target}: {e}"))?;
            klog!(
                "  ⚠ kernel has no sealed overlays: /overlayer/syshub is read-only but can be remounted"
            );
            Ok(())
        }
        Err(e) => Err(format!("overlay {target}: {e}")),
    }
}

/// The installed disk's zexlib must take writes: a missing or read-only store stops the boot.
fn writable(dir: &str) -> Result<(), String> {
    let probe = format!("{dir}/.write-test");
    fs::write(&probe, b"").map_err(|e| format!("{dir} is not writable: {e}"))?;
    fs::remove_file(&probe).map_err(|e| format!("{probe}: {e}"))
}

/// Bind mount `src` → `dest` (writable), creating `dest` if needed.
fn bind_dir(src: &str, dest: &str) -> Result<(), String> {
    if !std::path::Path::new(src).is_dir() {
        return Err(format!("{src} missing: not a Zainium disk"));
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
        MsFlags::MS_BIND
            | MsFlags::MS_REMOUNT
            | MsFlags::MS_RDONLY
            | MsFlags::MS_NOSUID
            | MsFlags::MS_NODEV,
        None::<&str>,
    )
    .map_err(|e| format!("remount ro '{}': {}", dest, e))?;
    klog!("  ✓ {} bind mounted ← {} (read-only)", dest, src);
    Ok(())
}

/// Bind mounts `src` on `dest` read-only (a bind mount needs a second, read-only remount).
fn bind_ro_dir(src: &str, dest: &str) -> Result<(), String> {
    if !std::path::Path::new(src).is_dir() {
        return Err(format!("{src} missing: not a Zainium disk"));
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
        MsFlags::MS_BIND
            | MsFlags::MS_REMOUNT
            | MsFlags::MS_RDONLY
            | MsFlags::MS_NOSUID
            | MsFlags::MS_NODEV,
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

/// tmpfs-backed zexlib for a live session or a rescue boot (not persistent).
fn tmpfs_zexlib() -> Result<String, String> {
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
    Ok(base.to_string())
}

/// True when the cmdline has zainium.overlay=off: the rescue boot straight from syshub.
pub fn overlay_disabled_by_cmdline() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|s| s.split_whitespace().any(|tok| tok == "zainium.overlay=off"))
        .unwrap_or(false)
}
