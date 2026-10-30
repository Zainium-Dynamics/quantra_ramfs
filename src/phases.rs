/// Boot phase tracking: each set_phase() stores the phase and extends TPM2 PCR 8 with it.
use std::sync::atomic::{AtomicU32, Ordering};

pub struct BootPhase;

impl BootPhase {
    pub const INIT: u32 = 0;
    pub const MOUNTS: u32 = 1;
    pub const CMDLINE: u32 = 2;
    pub const ROOTFS_DETECT: u32 = 3;
    pub const ROOTFS_MOUNT: u32 = 4;
    pub const OVERLAY: u32 = 5;
    pub const PIVOT: u32 = 6;
    pub const COMPLETE: u32 = 7;

    #[inline]
    pub fn phase_name(phase: u32) -> &'static str {
        match phase {
            0 => "Init",
            1 => "Mounts",
            2 => "Cmdline",
            3 => "RootfsDetect",
            4 => "RootfsMount",
            5 => "Overlay",
            6 => "Pivot",
            7 => "Complete",
            _ => "Unknown",
        }
    }
}

/// Global atomic boot phase — readable by watchdogs / monitoring tools.
pub static BOOT_PHASE: AtomicU32 = AtomicU32::new(0);

/// Moves to `phase`, logs it and measures it into PCR 8 when a TPM is present.
#[inline]
pub fn set_phase(phase: u32) {
    BOOT_PHASE.store(phase, Ordering::SeqCst);
    let name = BootPhase::phase_name(phase);
    klog!("→ {name}");
    // Measured boot: extend PCR[8] for every phase transition
    crate::measured_boot::measure_phase(phase, name);
}
