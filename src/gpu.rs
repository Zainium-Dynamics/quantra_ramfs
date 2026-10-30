//! Display driver and firmware from the root disk, loaded before the splash starts.

use nix::mount::{MntFlags, MsFlags, mount, umount2};
use std::collections::HashMap;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

/// Where the kernel's firmware loader and the module tree live at runtime.
const DRIVERS: &str = "/overlayer/syshub/drivers";

/// `finit_module` flag for a compressed module file.
const MODULE_INIT_COMPRESSED_FILE: libc::c_uint = 4;

/// Loads the drivers of the PCI display devices from the disk; returns how many got one.
pub fn load_from_disk(zairoot: &str) -> usize {
    let release = match fs::read_to_string("/proc/sys/kernel/osrelease") {
        Ok(r) => r.trim().to_string(),
        Err(_) => return 0,
    };
    let src = format!("{zairoot}{DRIVERS}");
    if !Path::new(&src).is_dir() || bind_ro(&src, DRIVERS).is_err() {
        klog!("  gpu: no drivers dir on the root disk");
        return 0;
    }
    // a kernel just installed by zex kernel boots once before its modules leave the staged upgrade
    let mut moddir = format!("{DRIVERS}/modules/{release}");
    let staged = format!("{zairoot}/overlayer/.syshub-pending/root/drivers/modules/{release}");
    if !Path::new(&moddir).is_dir() && Path::new(&staged).is_dir() {
        klog!("  gpu: modules for {release} come from the staged upgrade");
        moddir = staged;
    }
    let aliases = read_aliases(&format!("{moddir}/modules.alias"));
    let deps = read_deps(&format!("{moddir}/modules.dep"));

    let mut devices = 0;
    for modalias in display_modaliases() {
        let mut loaded = false;
        for module in aliases
            .iter()
            .filter(|(p, _)| glob(p, &modalias))
            .map(|(_, m)| m)
        {
            match load_with_deps(&moddir, module, &deps) {
                Ok(()) => loaded = true,
                Err(e) => klog!("  gpu: {module}: {e}"),
            }
        }
        if loaded {
            devices += 1;
        }
    }
    if devices > 0 && wait_for_drm(Duration::from_secs(3)) {
        klog!("  gpu: display driver ready");
    }
    devices
}

/// Takes down the drivers bind before the switch to the real root.
pub fn release() {
    let _ = umount2(DRIVERS, MntFlags::MNT_DETACH);
}

fn bind_ro(src: &str, dest: &str) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    mount(
        Some(src),
        dest,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| e.to_string())?;
    mount(
        None::<&str>,
        dest,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .map_err(|e| e.to_string())
}

/// modalias of every PCI display device and of its direct children (virtio GPUs match the child).
fn display_modaliases() -> Vec<String> {
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else {
        return Vec::new();
    };
    let mut aliases = Vec::new();
    for dev in entries.flatten().map(|e| e.path()) {
        if !fs::read_to_string(dev.join("class")).is_ok_and(|c| c.trim().starts_with("0x03")) {
            continue;
        }
        let children = fs::read_dir(&dev)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path());
        // real subdirectories only: symlinks (driver, firmware_node, ...) lead elsewhere
        let children = children.filter(|c| fs::symlink_metadata(c).is_ok_and(|m| m.is_dir()));
        for d in std::iter::once(dev.clone()).chain(children) {
            if let Ok(m) = fs::read_to_string(d.join("modalias")) {
                aliases.push(m.trim().to_string());
            }
        }
    }
    aliases
}

/// `alias <pattern> <module>` lines of modules.alias, in file order.
fn read_aliases(path: &str) -> Vec<(String, String)> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            match (f.next(), f.next(), f.next()) {
                (Some("alias"), Some(p), Some(m)) => Some((p.to_string(), m.to_string())),
                _ => None,
            }
        })
        .collect()
}

/// modules.dep: module name -> (its path, the paths it depends on).
fn read_deps(path: &str) -> HashMap<String, (String, Vec<String>)> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|l| {
            let (file, deps) = l.split_once(':')?;
            let deps = deps.split_whitespace().map(str::to_string).collect();
            Some((module_name(file), (file.to_string(), deps)))
        })
        .collect()
}

/// `kernel/drivers/gpu/drm/i915/i915.ko.zst` -> `i915`; '-' and '_' are the same in names.
fn module_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let base = base.split(".ko").next().unwrap_or(base);
    base.replace('-', "_")
}

/// Loads `module` after its dependencies from modules.dep; built-in or missing modules are skipped.
fn load_with_deps(
    moddir: &str,
    module: &str,
    deps: &HashMap<String, (String, Vec<String>)>,
) -> Result<(), String> {
    let Some((file, needs)) = deps.get(&module.replace('-', "_")) else {
        return Ok(());
    };
    for dep in needs.iter().rev() {
        insmod(&format!("{moddir}/{dep}"))?;
    }
    insmod(&format!("{moddir}/{file}"))
}

fn insmod(path: &str) -> Result<(), String> {
    if Path::new("/sys/module").join(module_name(path)).exists() {
        return Ok(());
    }
    let f = fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let flags = if path.ends_with(".ko") {
        0
    } else {
        MODULE_INIT_COMPRESSED_FILE
    };
    let ret = unsafe { libc::syscall(libc::SYS_finit_module, f.as_raw_fd(), c"".as_ptr(), flags) };
    let err = std::io::Error::last_os_error();
    if ret != 0 && err.raw_os_error() != Some(libc::EEXIST) {
        return Err(format!("{path}: {err}"));
    }
    klog!("  gpu: loaded {}", module_name(path));
    Ok(())
}

/// Waits for a DRM card that belongs to a real device, not the firmware framebuffer.
fn wait_for_drm(timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(entries) = fs::read_dir("/sys/class/drm") {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.starts_with("card") || name.contains('-') {
                    continue;
                }
                if fs::read_link(e.path().join("device"))
                    .is_ok_and(|t| !t.to_string_lossy().contains("simple-framebuffer"))
                {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// fnmatch(3) for modalias patterns: '*', '?' and '[...]' classes.
fn glob(pattern: &str, text: &str) -> bool {
    fn class(p: &[u8], c: u8) -> Option<(bool, usize)> {
        let mut i = 1;
        let negate = p.get(1) == Some(&b'!');
        if negate {
            i += 1;
        }
        let mut hit = false;
        while i < p.len() && (p[i] != b']' || i == 1 + negate as usize) {
            if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
                hit |= p[i] <= c && c <= p[i + 2];
                i += 3;
            } else {
                hit |= p[i] == c;
                i += 1;
            }
        }
        (i < p.len()).then_some((hit != negate, i + 1))
    }
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => (0..=t.len()).any(|k| go(&p[1..], &t[k..])),
            Some(b'?') => !t.is_empty() && go(&p[1..], &t[1..]),
            Some(b'[') => match (t.first(), class(p, *t.first().unwrap_or(&0))) {
                (Some(_), Some((true, n))) => go(&p[n..], &t[1..]),
                _ => false,
            },
            Some(&c) => t.first() == Some(&c) && go(&p[1..], &t[1..]),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_pci_aliases() {
        let dev = "pci:v00008086d00001916sv000017AAsd00002233bc03sc00i00";
        assert!(glob("pci:v00008086d00001916sv*sd*bc03sc*i*", dev));
        assert!(glob("pci:v00008086d*sv*sd*bc03sc*i*", dev));
        assert!(!glob("pci:v00001002d*sv*sd*bc03sc*i*", dev));
        assert!(glob("pci:v00008086d0000191[0-9A-F]sv*", &dev[..32]));
        assert!(!glob("pci:v00008086d0000191[!0-9]sv*", &dev[..32]));
    }

    #[test]
    fn module_names_from_paths() {
        assert_eq!(module_name("kernel/drivers/gpu/drm/i915/i915.ko"), "i915");
        assert_eq!(
            module_name("kernel/drivers/gpu/drm/drm-kms.ko.zst"),
            "drm_kms"
        );
    }
}
