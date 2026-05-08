//! MPU configuration and per-task reprogramming.
//!
//! Each Task carries generic R/W/X permission bits in its TaskRegion list.
//! On context switch, those are translated to the RP2040's RASR encoding
//! (AP field, XN bit) and written to MPU regions 0..N. PRIVDEFENA stays
//! on so kernel-mode access is unaffected by MPU misses.

use cortex_m::peripheral::MPU;

use crate::task;

// RASR field encodings — see Armv6-M ARM § B3.5.
pub const RASR_ENABLE: u32 = 1 << 0;
pub const RASR_XN: u32 = 1 << 28;
// AP[2:0] in bits 26..24. Only the variants we actually use are defined;
// add more (e.g. PRIV_RW_UNPRIV_NONE = 0b001) when a future region needs
// them.
pub const RASR_AP_PRIV_RW_UNPRIV_RW: u32 = 0b011 << 24;
pub const RASR_AP_PRIV_RO_UNPRIV_RO: u32 = 0b110 << 24;
// S=1, C=1, B=0, TEX=0 → "Outer & inner write-through, shareable".
// Adequate default for SRAM and flash on RP2040.
pub const RASR_MEM_NORMAL: u32 = (1 << 18) | (1 << 17);

pub struct Region {
    pub number: u8,
    pub base: u32,
    pub size_bytes: u32,
    pub attrs: u32, // OR of RASR_* flags above (without ENABLE/SIZE)
}

fn size_field(size_bytes: u32) -> u32 {
    // SIZE encoding = log2(size_bytes) - 1, in bits 5..1.
    let log2 = 31 - size_bytes.leading_zeros();
    (log2 - 1) << 1
}

impl Region {
    fn rbar(&self) -> u32 {
        // VALID=1 (bit 4) + REGION (bits 3..0) lets us write rbar and
        // implicitly select the region number — saves a write to RNR.
        (self.base & !0x1F) | (1 << 4) | (self.number as u32 & 0xF)
    }

    fn rasr(&self) -> u32 {
        self.attrs | size_field(self.size_bytes) | RASR_ENABLE
    }
}

pub fn configure(regions: &[Region]) {
    // Use the static MPU pointer rather than a borrowed `&MPU` handle.
    // Callers like the PendSV context-switch path don't have access to
    // the cortex-m Peripherals struct (it was consumed at boot), and
    // there's only ever one MPU on the chip.
    // SAFETY: kernel-only, single-threaded with respect to MPU register
    // writes (PendSV is the only runtime caller; boot is single-threaded).
    let mpu = unsafe { &*MPU::PTR };
    unsafe {
        // Disable while we reconfigure.
        mpu.ctrl.write(0);

        for r in regions {
            mpu.rbar.write(r.rbar());
            mpu.rasr.write(r.rasr());
        }

        // ENABLE=1 (bit 0), PRIVDEFENA=1 (bit 2).
        // PRIVDEFENA=1 means privileged code falls back to the default
        // memory map for addresses not covered by any region — so the
        // kernel keeps full access without us having to enumerate every
        // peripheral. Unprivileged code only gets what regions grant.
        mpu.ctrl.write((1 << 0) | (1 << 2));

        cortex_m::asm::dsb();
        cortex_m::asm::isb();
    }
}

fn perms_to_rasr_attrs(perms: u8) -> u32 {
    let mut attrs = RASR_MEM_NORMAL;
    if perms & task::PERM_X == 0 {
        attrs |= RASR_XN;
    }
    let r = perms & task::PERM_R != 0;
    let w = perms & task::PERM_W != 0;
    attrs |= match (r, w) {
        (true, true) => RASR_AP_PRIV_RW_UNPRIV_RW,
        (true, false) => RASR_AP_PRIV_RO_UNPRIV_RO,
        _ => 0, // no-access (shouldn't happen for any granted region)
    };
    attrs
}

pub fn reconfigure_for_task(t: &task::Task) {
    // All current tasks use exactly two regions (RAM + flash). When
    // future tasks need more regions, generalize this.
    let regions = [
        Region {
            number: 0,
            base: t.regions[0].base,
            size_bytes: t.regions[0].size,
            attrs: perms_to_rasr_attrs(t.regions[0].perms),
        },
        Region {
            number: 1,
            base: t.regions[1].base,
            size_bytes: t.regions[1].size,
            attrs: perms_to_rasr_attrs(t.regions[1].perms),
        },
    ];
    configure(&regions);
}
