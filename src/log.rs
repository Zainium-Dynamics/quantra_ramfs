//! All initramfs output goes through here: held until `quiet` is known, then the console or kmsg.
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

const HOLD: u8 = 0;
const CONSOLE: u8 = 1;
const KMSG: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(HOLD);
static HELD: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Backend of `klog!`.
pub fn line(msg: String) {
    match MODE.load(Ordering::SeqCst) {
        HOLD => {
            if let Ok(mut held) = HELD.lock() {
                held.push(msg);
            }
        }
        KMSG => write_kmsg(&msg),
        _ => {
            let _ = std::io::stderr().write_all(format!("{msg}\n").as_bytes());
        }
    }
}

// fresh open per line: /dev/kmsg rate-limits each open file to 10 lines per 5s
fn write_kmsg(msg: &str) {
    for text in msg.lines().map(str::trim).filter(|t| !t.is_empty()) {
        if let Ok(mut kmsg) = OpenOptions::new().write(true).open("/dev/kmsg") {
            let _ = kmsg.write_all(format!("<6>quantra-ramfs: {}\n", plain(text)).as_bytes());
        }
    }
}

// dmesg escapes non-ASCII, so status glyphs become words
fn plain(text: &str) -> String {
    let text = if let Some(rest) = text.strip_prefix("✓ ") {
        rest.to_string()
    } else if let Some(rest) = text.strip_prefix("⚠ ") {
        format!("warning: {rest}")
    } else if let Some(rest) = text.strip_prefix("✗ ") {
        format!("error: {rest}")
    } else if let Some(rest) = text.strip_prefix("→ ") {
        format!("phase: {rest}")
    } else {
        text.to_string()
    };
    text.replace(" ✓ ", " ").replace('→', "->").replace('←', "<-").replace('—', "-")
}

/// Quiet boot: our lines and every child's stdout/stderr go to the kernel log (dmesg, journal).
pub fn to_kmsg() {
    point_stdio_at("/dev/kmsg");
    set_mode(KMSG);
}

/// Real console: verbose boot, fatal errors, emergency shell, handoff to init.
pub fn to_console() {
    point_stdio_at("/dev/console");
    set_mode(CONSOLE);
}

fn point_stdio_at(path: &str) {
    if let Ok(f) = OpenOptions::new().write(true).open(path) {
        unsafe {
            libc::dup2(f.as_raw_fd(), 1);
            libc::dup2(f.as_raw_fd(), 2);
        }
    }
}

fn set_mode(mode: u8) {
    MODE.store(mode, Ordering::SeqCst);
    let held = HELD
        .lock()
        .map(|mut h| std::mem::take(&mut *h))
        .unwrap_or_default();
    for msg in held {
        line(msg);
    }
}
