//! tiny-wallet — toy hardware wallet for the XIAO RP2040.
//!
//! Phase 1 PoC: minimal MPU-enforced microkernel. One privileged kernel,
//! one unprivileged user task, one syscall. The point of this phase is to
//! prove the *isolation mechanism* end-to-end before any wallet logic is
//! written: a buggy or compromised user task cannot reach kernel RAM or
//! peripherals — the MPU traps it.
//!
//! Boot path (after the rp2040 mask ROM has loaded boot2 from flash):
//!   reset → cortex-m-rt → main()  [privileged thread mode, MSP]
//!     → configure MPU
//!     → enable SysTick (kernel heartbeat in handler mode)
//!     → set PSP to top of TASK0 RAM
//!     → write CONTROL = (SPSEL=1, nPRIV=1) → drop privilege
//!     → bx into the user task
//!
//! After the drop, the only path back into kernel code is via SVC (syscalls)
//! or via an exception (SysTick, HardFault). The user task cannot directly
//! call kernel functions because its MPU view excludes everything it wasn't
//! explicitly granted.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m::peripheral::{Peripherals, syst::SystClkSource};
use cortex_m_rt::{ExceptionFrame, entry, exception};
use {defmt_rtt as _, panic_probe as _};

// =============================================================================
// Stage 2 bootloader
// =============================================================================
//
// The RP2040 mask ROM copies the first 256 bytes of flash into RAM and
// executes them. That blob — provided here by the `rp2040-boot2` crate for
// the W25Q080 QSPI flash on the XIAO RP2040 — sets up XIP so the rest of the
// firmware can execute directly from flash.
#[unsafe(link_section = ".boot2")]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;

// =============================================================================
// User task RAM
// =============================================================================
//
// 8 KiB block, 8 KiB-aligned, so the MPU can cover it as a single region.
// MPU regions on Armv6-M must be a power-of-two size and naturally aligned.
// Phase 2 will introduce a task table; for now, hardcode one task.
#[repr(C, align(8192))]
struct TaskRam(#[allow(dead_code)] [u8; 8192]);

static mut TASK0_RAM: TaskRam = TaskRam([0; 8192]);

// =============================================================================
// Syscall ABI
// =============================================================================
//
// User side: place the syscall number in r0 and args in r1..r3, then `svc #0`.
// Kernel SVCall handler reads the saved frame from PSP, dispatches, and
// writes the return value back into the saved r0 slot so the caller sees it.

const SYSCALL_PRINT: u32 = 1;
const SYSCALL_LED: u32 = 2;

#[inline(always)]
fn syscall2(num: u32, a1: u32, a2: u32) -> u32 {
    let ret: u32;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("r0") num,
            in("r1") a1,
            in("r2") a2,
            lateout("r0") ret,
        );
    }
    ret
}

fn sys_print(buf: &[u8]) {
    syscall2(SYSCALL_PRINT, buf.as_ptr() as u32, buf.len() as u32);
}

/// Ask the kernel to drive an on-board LED. `which`: 0=red, 1=green, 2=blue.
fn sys_set_led(which: u32, on: bool) {
    syscall2(SYSCALL_LED, which, on as u32);
}

// =============================================================================
// User task
// =============================================================================
//
// Runs in unprivileged thread mode on PSP. MPU-allowed memory:
//   - read+execute  flash             (so the task can run its own code)
//   - read+write    its own 8 KiB RAM (TASK0_RAM)
// Anything else — kernel RAM, peripherals — should fault.
extern "C" fn task0_main() -> ! {
    sys_print(b"hello from user task");

    // Phase 2A demo: ask the kernel to print a buffer that points into
    // *kernel* RAM (not in any of this task's allowed regions). The kernel
    // must reject this — otherwise we have a confused-deputy bug.
    //
    // Visible signal on success (rejection): five fast blue pulses before
    // the normal slow toggle starts. Distinguishable from:
    //   - solid red       (HardFault — MPU caught a direct access)
    //   - slow blue blink (normal user-task heartbeat)
    //
    // If the burst is missing and we go straight to the slow blink, the
    // kernel accepted a pointer it should have rejected — a real bug.
    let kernel_ram_addr: u32 = 0x2000_0000;
    let result = syscall2(SYSCALL_PRINT, kernel_ram_addr, 16);
    if result == u32::MAX {
        for _ in 0..5 {
            sys_set_led(2, true);
            for _ in 0..50_000 {
                unsafe { core::arch::asm!("nop") };
            }
            sys_set_led(2, false);
            for _ in 0..50_000 {
                unsafe { core::arch::asm!("nop") };
            }
        }
    }

    let mut counter: u32 = 0;
    loop {
        // Visible heartbeat from the user side: toggle the blue LED via
        // syscall. If the SVC round-trip works, blue blinks; if it doesn't,
        // blue stays whatever it was. Either way the green kernel-heartbeat
        // is independent.
        sys_set_led(2, counter & 1 == 0);
        counter = counter.wrapping_add(1);

        // Crude busy-wait between toggles.
        for _ in 0..400_000 {
            unsafe { core::arch::asm!("nop") };
        }

        // Phase-1 isolation demo: after a few iterations, deliberately try
        // to drive the blue LED *directly* by writing to the SIO peripheral
        // — bypassing the syscall. The MPU should deny this (peripherals
        // aren't in the user task's MPU view), the HardFault handler should
        // turn the red LED on solid, and the board should hang.
        if counter == 5 {
            sys_print(b"about to bypass the syscall and write SIO directly");
            unsafe {
                let sio_gpio_out_clr = 0xD000_0018 as *mut u32;
                sio_gpio_out_clr.write_volatile(1 << 25); // would turn blue on
            }
        }
    }
}

// =============================================================================
// Kernel: GPIO (XIAO RP2040 on-board LEDs)
// =============================================================================
//
// The XIAO RP2040 has three simple-GPIO LEDs (active LOW): R=GPIO17,
// G=GPIO16, B=GPIO25. The kernel owns all three; the user task can only
// affect them via SYSCALL_LED.
mod gpio {
    // RP2040 peripheral addresses (datasheet § 2.14, 2.19, 2.20).
    const RESETS_BASE: u32 = 0x4000_C000;
    const RESETS_RESET_CLR: u32 = RESETS_BASE | 0x3000; // atomic-clear alias
    const RESETS_RESET_DONE: u32 = RESETS_BASE + 0x008;
    const RESET_IO_BANK0: u32 = 1 << 5;
    const RESET_PADS_BANK0: u32 = 1 << 8;

    const IO_BANK0_BASE: u32 = 0x4001_4000;
    const PADS_BANK0_BASE: u32 = 0x4001_C000;
    const PADS_BANK0_CLR: u32 = PADS_BANK0_BASE | 0x3000;

    const SIO_BASE: u32 = 0xD000_0000;
    const SIO_GPIO_OUT_SET: u32 = SIO_BASE + 0x014;
    const SIO_GPIO_OUT_CLR: u32 = SIO_BASE + 0x018;
    const SIO_GPIO_OUT_XOR: u32 = SIO_BASE + 0x01C;
    const SIO_GPIO_OE_SET: u32 = SIO_BASE + 0x024;

    pub const LED_RED: u32 = 17;
    pub const LED_GREEN: u32 = 16;
    pub const LED_BLUE: u32 = 25;

    fn write(addr: u32, val: u32) {
        unsafe {
            (addr as *mut u32).write_volatile(val);
        }
    }
    fn read(addr: u32) -> u32 {
        unsafe { (addr as *const u32).read_volatile() }
    }

    pub fn init_leds() {
        // Release IO_BANK0 + PADS_BANK0 from reset (they boot held in reset).
        let mask = RESET_IO_BANK0 | RESET_PADS_BANK0;
        write(RESETS_RESET_CLR, mask);
        while (read(RESETS_RESET_DONE) & mask) != mask {}

        for &pin in &[LED_RED, LED_GREEN, LED_BLUE] {
            // GPIOn_CTRL.FUNCSEL = 5 (SIO). Other CTRL fields = 0.
            write(IO_BANK0_BASE + 4 + pin * 8, 5);
            // PADS_BANK0_GPIOn: clear OD (output-disable, bit 7) so the pad drives.
            write(PADS_BANK0_CLR + 4 + pin * 4, 1 << 7);
            // Drive HIGH first (LED off — active low) so we don't flash on boot.
            write(SIO_GPIO_OUT_SET, 1 << pin);
            // Then enable output.
            write(SIO_GPIO_OE_SET, 1 << pin);
        }
    }

    /// Active-low: `on=true` clears the pin (drives low).
    pub fn set(pin: u32, on: bool) {
        if on {
            write(SIO_GPIO_OUT_CLR, 1 << pin);
        } else {
            write(SIO_GPIO_OUT_SET, 1 << pin);
        }
    }

    pub fn toggle(pin: u32) {
        write(SIO_GPIO_OUT_XOR, 1 << pin);
    }
}

// =============================================================================
// Kernel: tasks
// =============================================================================
//
// A task carries the same (base, size, perms) region list that's programmed
// into the MPU for it. The kernel uses this list to validate user-supplied
// pointers in syscalls — without it, a malicious task could pass a kernel
// pointer to SYSCALL_PRINT and have the kernel exfiltrate kernel memory on
// the task's behalf (the "confused deputy" pattern).
//
// Phase 2A: one hardcoded task. Phase 2B will introduce a real table.
mod task {
    pub const PERM_R: u8 = 1 << 0;
    pub const PERM_W: u8 = 1 << 1;
    pub const PERM_X: u8 = 1 << 2;
    pub const PERM_RW: u8 = PERM_R | PERM_W;
    pub const PERM_RX: u8 = PERM_R | PERM_X;

    #[derive(Clone, Copy)]
    pub struct TaskRegion {
        pub base: u32,
        pub size: u32,
        pub perms: u8,
    }

    pub struct Task {
        /// Function pointer the kernel jumps to when first running this task.
        pub entry_pc: u32,
        /// Initial PSP value (top of the task's stack, 8-byte aligned).
        /// Phase 2C will replace this with a saved PSP across context switches.
        pub initial_psp: u32,
        /// MPU regions granted to this task, mirrored for syscall pointer
        /// validation. Empty entries (size == 0) are skipped.
        pub regions: [TaskRegion; 4],
    }

    const EMPTY: TaskRegion = TaskRegion { base: 0, size: 0, perms: 0 };
    const EMPTY_TASK: Task = Task {
        entry_pc: 0,
        initial_psp: 0,
        regions: [EMPTY; 4],
    };

    pub static mut TASKS: [Task; 1] = [EMPTY_TASK];

    /// Populate task 0. Called once from kernel `main()` before privilege drop.
    /// `ram_base` must be aligned to `ram_size` (MPU requirement); `ram_size`
    /// must be a power of two ≥ 32.
    pub fn init_task0(entry_pc: u32, ram_base: u32, ram_size: u32) {
        // SAFETY: called once at boot before any code can read TASKS.
        unsafe {
            let t = &raw mut TASKS[0];
            (*t).entry_pc = entry_pc;
            (*t).initial_psp = (ram_base + ram_size) & !7;
            (*t).regions[0] = TaskRegion {
                base: ram_base,
                size: ram_size,
                perms: PERM_RW,
            };
            (*t).regions[1] = TaskRegion {
                base: 0x1000_0000,
                size: 16 * 1024 * 1024,
                perms: PERM_RX,
            };
        }
    }

    /// PoC: only one task exists. Phase 2C will replace this with a
    /// scheduler-tracked index.
    pub fn current() -> &'static Task {
        // SAFETY: TASKS is only mutated by init_task0 at boot. After that
        // it's read-only for the lifetime of the program (Phase 2A scope).
        unsafe { &*(&raw const TASKS[0]) }
    }

    /// Returns true iff `[ptr, ptr+len)` is fully inside one of the task's
    /// regions and that region grants the access in `need`.
    pub fn validate_buf(task: &Task, ptr: u32, len: u32, need: u8) -> bool {
        let end = match ptr.checked_add(len) {
            Some(e) => e,
            None => return false,
        };
        for r in &task.regions {
            if r.size == 0 {
                continue;
            }
            let r_end = match r.base.checked_add(r.size) {
                Some(e) => e,
                None => continue,
            };
            if ptr >= r.base && end <= r_end && (r.perms & need) == need {
                return true;
            }
        }
        false
    }
}

// =============================================================================
// Kernel: MPU
// =============================================================================
mod mpu_cfg {
    use cortex_m::peripheral::MPU;

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

    pub fn configure(mpu: &MPU, regions: &[Region]) {
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
}

use mpu_cfg::{
    RASR_AP_PRIV_RO_UNPRIV_RO, RASR_AP_PRIV_RW_UNPRIV_RW, RASR_MEM_NORMAL, RASR_XN, Region,
    configure,
};

// =============================================================================
// Kernel: SVC dispatch
// =============================================================================

fn syscall_print(ptr: u32, len: u32) -> u32 {
    if len > 256 {
        return u32::MAX;
    }
    // Validate the user-supplied buffer is inside the calling task's regions
    // before dereferencing — closes the confused-deputy hole. Without this
    // check, the user task could pass a kernel pointer and have us read
    // kernel memory on its behalf.
    if !task::validate_buf(task::current(), ptr, len, task::PERM_R) {
        defmt::warn!(
            "kernel: rejected SYSCALL_PRINT — buf not in caller's regions (ptr=0x{:08x} len={})",
            ptr,
            len,
        );
        return u32::MAX;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    match core::str::from_utf8(bytes) {
        Ok(s) => defmt::info!("[user→kernel print] {}", s),
        Err(_) => defmt::info!("[user→kernel print] (non-utf8, {} bytes)", bytes.len()),
    }
    0
}

fn syscall_led(which: u32, value: u32) -> u32 {
    let pin = match which {
        0 => gpio::LED_RED,
        1 => gpio::LED_GREEN,
        2 => gpio::LED_BLUE,
        _ => return u32::MAX,
    };
    gpio::set(pin, value != 0);
    0
}

#[exception]
unsafe fn SVCall() {
    // The exception was taken from unprivileged thread mode (the user task
    // is the only thing running on PSP), so r0..r3,r12,lr,pc,xpsr are saved
    // on PSP — not MSP, which is now the active stack for the handler.
    let psp: *mut u32;
    unsafe { core::arch::asm!("mrs {}, psp", out(reg) psp) };
    let frame = unsafe { core::slice::from_raw_parts_mut(psp, 8) };

    let num = frame[0];
    let a1 = frame[1];
    let a2 = frame[2];

    let ret = match num {
        SYSCALL_PRINT => syscall_print(a1, a2),
        SYSCALL_LED => syscall_led(a1, a2),
        _ => {
            defmt::warn!("kernel: unknown syscall {}", num);
            u32::MAX
        }
    };

    // Caller will see this in r0 after exception return.
    frame[0] = ret;
}

// =============================================================================
// Kernel: heartbeat (SysTick)
// =============================================================================
//
// SysTick fires roughly once a second off the boot ROSC (~6.5 MHz core clock
// before any clock tree configuration). Used here purely as a liveness signal
// so the operator can see the kernel is alive even if the user task is
// silent or stuck.
static KERNEL_TICKS: AtomicU32 = AtomicU32::new(0);

#[exception]
fn SysTick() {
    // Plain load+store on AtomicU32 — M0+ lacks CAS, but Relaxed load/store
    // compile to LDR/STR, which is sufficient since SysTick is the only
    // writer.
    let n = KERNEL_TICKS.load(Ordering::Relaxed).wrapping_add(1);
    KERNEL_TICKS.store(n, Ordering::Relaxed);
    // Visible heartbeat: green LED toggles every tick (~0.5 Hz blink).
    gpio::toggle(gpio::LED_GREEN);
    if n % 5 == 0 {
        defmt::info!("kernel: heartbeat tick={}", n);
    }
}

// =============================================================================
// Kernel: HardFault
// =============================================================================
//
// On Cortex-M0+ the MPU fault path escalates to HardFault — there's no
// MemManage exception or fault-status register, so we can't *prove* it was
// the MPU. In this PoC the only realistic source of a HardFault is an MPU
// violation from the user task, which is exactly what we want to demonstrate.
#[exception]
unsafe fn HardFault(ef: &ExceptionFrame) -> ! {
    defmt::error!(
        "HardFault! pc=0x{:08x} lr=0x{:08x} xpsr=0x{:08x} — likely MPU violation from user task",
        ef.pc(),
        ef.lr(),
        ef.xpsr(),
    );
    // Visible signal: red LED solid, green + blue off. Without a debug
    // probe attached, `bkpt` would itself escalate, so just halt cleanly
    // with WFI.
    gpio::set(gpio::LED_GREEN, false);
    gpio::set(gpio::LED_BLUE, false);
    gpio::set(gpio::LED_RED, true);
    loop {
        cortex_m::asm::wfi();
    }
}

// =============================================================================
// Kernel: privilege drop
// =============================================================================
//
// `bootstrap_user` is the one-way trapdoor from kernel to user mode: load
// the task's PSP, set CONTROL=(SPSEL=1, nPRIV=1), ISB, then bx into the
// task entry. After this point the only path back to kernel code is via
// SVC or an exception.
//
// In Phase 2C this same routine will be reused on the very first switch
// to *any* task, by feeding it different Task entries. The signature also
// makes it easier to extend later when each task needs MPU reprogramming.
unsafe fn bootstrap_user(task: &task::Task) -> ! {
    // All three values flow through compiler-picked input registers; the
    // asm body never names a hardcoded register. Earlier versions used
    // `movs r0, #3` for the CONTROL value, which silently broke whenever
    // the compiler happened to pick `r0` for `{entry}` — the `movs`
    // clobbered the entry address before the `bx`, jumping to address 3.
    // (`options(noreturn)` forbids declaring `r0` as a clobber explicitly,
    // so just don't touch r0 at all.)
    unsafe {
        core::arch::asm!(
            "msr psp, {psp}",        // PSP = top of user stack
            "msr control, {ctrl}",   // CONTROL = nPRIV(1) | SPSEL(1) = 3
            "isb",                   // commit privilege change before next insn
            "bx  {entry}",           // jump into user task (unprivileged, on PSP)
            psp   = in(reg) task.initial_psp,
            entry = in(reg) task.entry_pc,
            ctrl  = in(reg) 3u32,
            options(noreturn),
        );
    }
}

// =============================================================================
// Kernel entry
// =============================================================================
#[entry]
fn main() -> ! {
    defmt::info!("tiny-wallet kernel: boot");
    let mut cp = Peripherals::take().unwrap();

    // ---- 0. On-board LEDs (kernel-owned, used as the visible status display) ----
    gpio::init_leds();
    defmt::info!("kernel: LEDs initialized (R=17 G=16 B=25)");

    // ---- 1. Populate the task table ----
    //
    // The Task entry holds entry_pc / initial_psp / regions. Both MPU
    // configuration and privilege-drop will read from it below, so the
    // table is the single source of truth for "what is task 0".
    let task0_ram_base = &raw const TASK0_RAM as u32;
    let task0_ram_size: u32 = 8 * 1024;
    defmt::info!(
        "kernel: TASK0_RAM @ 0x{:08x} (size {} B)",
        task0_ram_base,
        task0_ram_size,
    );
    task::init_task0(
        task0_main as *const () as u32,
        task0_ram_base,
        task0_ram_size,
    );

    // ---- 2. MPU (regions sourced from TASKS[0]) ----
    //
    // For now the MPU's RASR encoding is set inline; Phase 2C will move
    // this into a `switch_to_task` helper that reprograms MPU on each
    // context switch.
    let t = task::current();
    let mpu_regions = [
        Region {
            number: 0,
            base: t.regions[0].base,
            size_bytes: t.regions[0].size,
            attrs: RASR_AP_PRIV_RW_UNPRIV_RW | RASR_MEM_NORMAL | RASR_XN,
        },
        Region {
            number: 1,
            base: t.regions[1].base,
            size_bytes: t.regions[1].size,
            // Read-only + executable for both privilege levels — user task
            // needs to run its own code, which lives in flash.
            attrs: RASR_AP_PRIV_RO_UNPRIV_RO | RASR_MEM_NORMAL,
        },
    ];
    configure(&cp.MPU, &mpu_regions);
    defmt::info!("kernel: MPU configured (2 regions: task0 RAM, flash RX)");

    // ---- 3. SysTick heartbeat (~1 Hz off uncalibrated boot ROSC) ----
    cp.SYST.set_clock_source(SystClkSource::Core);
    cp.SYST.set_reload(6_500_000 - 1);
    cp.SYST.clear_current();
    cp.SYST.enable_counter();
    cp.SYST.enable_interrupt();
    defmt::info!("kernel: SysTick armed");

    // ---- 4. Drop to unprivileged thread mode and jump to user task ----
    defmt::info!(
        "kernel: dropping to task0: entry=0x{:08x} PSP=0x{:08x}",
        t.entry_pc,
        t.initial_psp,
    );
    unsafe { bootstrap_user(t) }
}
