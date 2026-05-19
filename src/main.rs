//! tiny-wallet — toy hardware wallet for the XIAO RP2040.
//!
//! Microkernel demo: one privileged kernel, four unprivileged user tasks.
//! MPU enforces task isolation; SVC is the only path back into the
//! kernel after the privilege drop. The vault task holds an ed25519
//! keypair that the host-facing `host_io` task cannot read directly —
//! it can only ask the vault to sign over IPC.
//!
//! Boot path (after the rp2040 mask ROM has loaded boot2 from flash):
//!   reset → cortex-m-rt → main()  [privileged thread mode, MSP]
//!     → init LEDs, USB-CDC, task table
//!     → configure MPU for task 0
//!     → enable SysTick (kernel heartbeat in handler mode)
//!     → set PSP to top of TASK0 RAM
//!     → write CONTROL = (SPSEL=1, nPRIV=1) → drop privilege
//!     → bx into the user task
//!
//! After the drop, the only path back into kernel code is via SVC
//! (syscalls), the USBCTRL_IRQ, or an exception (SysTick, HardFault).
//!
//! Module layout:
//!   syscall  — user-side ABI (constants, sys_print, sys_send, …)
//!   task     — Task struct, scheduler, region validation
//!   gpio     — on-board LED driver
//!   mpu      — MPU configuration + per-task reprogramming
//!   usb      — USB-CDC + RX ring + USBCTRL_IRQ
//!   kernel   — kernel-side syscall handlers (called from SVCall below)
//!   user     — the four task entry functions

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m::peripheral::{Peripherals, SCB, syst::SystClkSource};
use cortex_m_rt::{ExceptionFrame, entry, exception};
use {defmt_rtt as _, panic_probe as _};

mod gpio;
mod kernel;
mod mpu;
mod syscall;
mod task;
mod usb;
mod user;

use syscall::{
    SYSCALL_LED, SYSCALL_PRINT, SYSCALL_RECV, SYSCALL_SEND, SYSCALL_USB_READ, SYSCALL_USB_WRITE,
    SYSCALL_YIELD,
};

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
#[repr(C, align(8192))]
struct TaskRam(#[allow(dead_code)] [u8; 8192]);

static mut TASK0_RAM: TaskRam = TaskRam([0; 8192]);
static mut TASK1_RAM: TaskRam = TaskRam([0; 8192]);
static mut TASK2_RAM: TaskRam = TaskRam([0; 8192]);
static mut TASK3_RAM: TaskRam = TaskRam([0; 8192]);

// =============================================================================
// SVCall dispatcher
// =============================================================================
//
// User tasks reach the kernel by issuing `svc #0` with the syscall number
// in r0 and arguments in r1..r3 (see `syscall.rs`). The hardware vectors
// here; we read the saved frame off PSP, dispatch to the matching kernel
// handler in `kernel.rs`, and write the return value back into r0's slot.
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
    let a3 = frame[3];

    let ret = match num {
        SYSCALL_PRINT => kernel::syscall_print(a1, a2),
        SYSCALL_LED => kernel::syscall_led(a1, a2),
        SYSCALL_YIELD => {
            // Pend PendSV. Because PendSV is configured to lowest priority,
            // it fires *after* this SVC's exception return — at which point
            // PSP is back to the user task's pre-SVC stack, which is what
            // we want PendSV to save.
            SCB::set_pendsv();
            0
        }
        SYSCALL_SEND => kernel::syscall_send(a1, a2, a3),
        SYSCALL_RECV => kernel::syscall_recv(a1, a2),
        SYSCALL_USB_READ => kernel::syscall_usb_read(a1, a2),
        SYSCALL_USB_WRITE => kernel::syscall_usb_write(a1, a2),
        _ => {
            defmt::warn!("kernel: unknown syscall {}", num);
            u32::MAX
        }
    };

    // Caller will see this in r0 after exception return.
    frame[0] = ret;
}

// =============================================================================
// SysTick — kernel heartbeat
// =============================================================================
//
// Liveness signal so the operator can see the kernel is alive even if the
// user tasks are silent. Phase 2D could pend PendSV here for round-robin
// preemption.
static KERNEL_TICKS: AtomicU32 = AtomicU32::new(0);

#[exception]
fn SysTick() {
    // Plain load+store on AtomicU32 — M0+ lacks CAS, but Relaxed load/store
    // compile to LDR/STR, which is sufficient since SysTick is the only
    // writer.
    let n = KERNEL_TICKS.load(Ordering::Relaxed).wrapping_add(1);
    KERNEL_TICKS.store(n, Ordering::Relaxed);
    if n % 5 == 0 {
        defmt::info!("kernel: heartbeat tick={}", n);
    }
}

// =============================================================================
// HardFault — visible diagnostic
// =============================================================================
//
// On Cortex-M0+ the MPU fault path escalates to HardFault — there's no
// MemManage exception or fault-status register, so we can't *prove* it was
// the MPU. The diagnostic blink encodes the PC's region so the operator can
// distinguish "user MPU violation" from "executing data" from "SCS region
// corruption" without attaching a probe.
#[exception]
unsafe fn HardFault(ef: &ExceptionFrame) -> ! {
    let pc = ef.pc();
    defmt::error!(
        "HardFault! pc=0x{:08x} lr=0x{:08x} xpsr=0x{:08x}",
        pc, ef.lr(), ef.xpsr(),
    );
    gpio::set(gpio::LED_GREEN, false);
    gpio::set(gpio::LED_BLUE, false);
    gpio::set(gpio::LED_RED, true);

    //   1 = PC in flash (0x10000000..)  → fault inside our code
    //   2 = PC in RAM   (0x20000000..)  → executing data (bad jump)
    //   3 = PC in SCS   (0xE0000000..)  → exception inside an exception
    //   4 = other                       → jumped to garbage (e.g. addr 0)
    let n = if (0x1000_0000..0x1020_0000).contains(&pc) {
        1
    } else if (0x2000_0000..0x2010_0000).contains(&pc) {
        2
    } else if pc >= 0xE000_0000 {
        3
    } else {
        4
    };

    fn long_delay() {
        for _ in 0..20_000_000 {
            cortex_m::asm::nop();
        }
    }
    fn short_delay() {
        for _ in 0..3_000_000 {
            cortex_m::asm::nop();
        }
    }
    long_delay(); // pause so the burst is visually distinct
    for _ in 0..n {
        gpio::set(gpio::LED_BLUE, true);
        short_delay();
        gpio::set(gpio::LED_BLUE, false);
        short_delay();
    }

    loop {
        cortex_m::asm::wfi();
    }
}

// =============================================================================
// PendSV — cooperative context switch
// =============================================================================
//
// Triggered indirectly by SYSCALL_YIELD (which sets PENDSVSET). Because
// PendSV is configured to the lowest priority, it runs after the
// triggering SVC has fully exited — so when PendSV fires, PSP holds the
// outgoing user task's true SP (no nested-exception frame on top).
//
// We can't write the body in plain Rust, because the cortex-m-rt
// `#[exception]` macro generates a wrapper with a function preamble that
// would clobber r4-r11 before we can save them. So PendSV is a fully
// naked function that:
//   1. Saves r4-r11 below the current PSP (Cortex-M0+ doesn't auto-save
//      callee-saved registers on exception entry — only r0-r3, r12, lr,
//      pc, xpsr).
//   2. Calls a Rust helper (`pendsv_switch`) with the new low-water PSP
//      as its argument; it returns the incoming task's saved PSP.
//   3. Restores r4-r11 from the incoming task's PSP.
//   4. Updates the PSP register and exception-returns. The hardware then
//      pops the standard 8-word frame from the new PSP, jumping into
//      the new task at its saved PC (or its entry point on first run).

#[unsafe(no_mangle)]
extern "C" fn pendsv_switch(outgoing_psp: u32) -> u32 {
    // SAFETY: runs only inside the PendSV handler, which is the unique
    // writer of CURRENT_TASK and the per-task saved_psp slots.
    unsafe {
        let cur = task::CURRENT_TASK;
        let tasks = &raw mut task::TASKS;
        (*tasks)[cur].saved_psp = outgoing_psp;

        // Round-robin pick that respects scheduling state — blocked tasks
        // (waiting on IPC) are skipped.
        let next = task::pick_next_ready(cur);
        task::CURRENT_TASK = next;

        let next_task = &(*tasks)[next];
        mpu::reconfigure_for_task(next_task);
        next_task.saved_psp
    }
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn PendSV() {
    core::arch::naked_asm!(
        // ---- save r4-r11 below the outgoing PSP ----
        "mrs   r0, psp",
        "subs  r0, #32",
        "stmia r0!, {{r4-r7}}",      // r4-r7 → [psp-32 .. psp-16]; r0 += 16
        "mov   r4, r8",
        "mov   r5, r9",
        "mov   r6, r10",
        "mov   r7, r11",
        "stmia r0!, {{r4-r7}}",      // r8-r11 → [psp-16 .. psp]; r0 += 16
        "subs  r0, #32",             // r0 = psp - 32 (the new low-water mark)

        // ---- call into Rust to update CURRENT_TASK + reprogram MPU ----
        "push  {{lr}}",              // preserve EXC_RETURN across the bl
        "bl    pendsv_switch",
        "pop   {{r1}}",
        "mov   lr, r1",
        // r0 = incoming task's saved_psp

        // ---- restore r4-r11 from the incoming PSP ----
        // CRITICAL: load the HIGH half first (saved r8-r11) using r4-r7 as
        // temps and move into r8-r11; THEN load the LOW half (saved r4-r7)
        // into r4-r7 directly. Doing it the other way around silently
        // corrupts r4-r7 because the second ldmia overwrites them with the
        // high-half values that should have gone to r8-r11.
        "adds  r0, #16",             // r0 = saved_psp + 16 (start of high half)
        "ldmia r0!, {{r4-r7}}",      // r4-r7 = saved r8,r9,r10,r11 (temps)
        "mov   r8, r4",
        "mov   r9, r5",
        "mov   r10, r6",
        "mov   r11, r7",
        "subs  r0, #32",             // r0 = saved_psp (start of low half)
        "ldmia r0!, {{r4-r7}}",      // r4-r7 = saved r4,r5,r6,r7 (final)
        "adds  r0, #16",             // r0 = saved_psp + 32 (start of exception frame)

        // ---- set PSP for the hardware exception return ----
        "msr   psp, r0",
        "bx    lr",                  // EXC_RETURN — HW pops the frame, runs new task
    );
}

// =============================================================================
// Privilege drop
// =============================================================================
//
// `bootstrap_user` is the one-way trapdoor from kernel to user mode: load
// the task's PSP, set CONTROL=(SPSEL=1, nPRIV=1), ISB, then bx into the
// task entry. After this point the only path back to kernel code is via
// SVC, an exception, or USBCTRL_IRQ.
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

    // Explicitly initialize task scheduling state — TASKS and CURRENT_TASK
    // landed at the .bss / .uninit boundary; cortex-m-rt only zero-inits
    // .bss, so anything in .uninit holds whatever was in RAM at reset.
    // Garbage TaskState discriminants make Rust's pattern-match jump to
    // arbitrary addresses (e.g. SCS region → HardFault).
    unsafe {
        core::ptr::write_volatile(&raw mut task::CURRENT_TASK, 0);
    }

    // ---- 0. On-board LEDs (kernel-owned, used as the visible status display) ----
    gpio::init_leds();
    defmt::info!("kernel: LEDs initialized (R=17 G=16 B=25)");

    // ---- 0.5. System clocks + USB-CDC ----
    //
    // After this call: core runs at 125 MHz (was ~6.5 MHz on boot ROSC),
    // PLL_USB gives 48 MHz to the USB block, and the device enumerates as
    // a CDC-ACM serial port. Must run before SysTick is configured because
    // SysTick reload depends on the core clock.
    usb::init();
    defmt::info!("kernel: USB-CDC up (VID=0x16c0 PID=0x27dd)");

    // ---- 1. Populate the task table ----
    //
    // Each Task entry holds entry_pc / initial_psp / saved_psp / regions.
    // Task 0 boots via `bootstrap_user` (uses initial_psp). Tasks 1..3 boot
    // via PendSV's restore path on the first switch (use saved_psp,
    // which init_task_with_frame seeds with a fake exception frame).
    let task0_ram_base = &raw const TASK0_RAM as u32;
    let task1_ram_base = &raw const TASK1_RAM as u32;
    let task2_ram_base = &raw const TASK2_RAM as u32;
    let task3_ram_base = &raw const TASK3_RAM as u32;
    let task_ram_size: u32 = 8 * 1024;
    defmt::info!(
        "kernel: TASK0_RAM=0x{:08x} TASK1_RAM=0x{:08x} TASK2_RAM=0x{:08x} TASK3_RAM=0x{:08x} (each {} B)",
        task0_ram_base,
        task1_ram_base,
        task2_ram_base,
        task3_ram_base,
        task_ram_size,
    );
    task::init_task0(
        user::client::task0_main as *const () as u32,
        task0_ram_base,
        task_ram_size,
    );
    task::init_task_with_frame(
        1,
        user::server::task1_main as *const () as u32,
        task1_ram_base,
        task_ram_size,
    );
    task::init_task_with_frame(
        2,
        user::host_io::host_io_main as *const () as u32,
        task2_ram_base,
        task_ram_size,
    );
    task::init_task_with_frame(
        3,
        user::vault::vault_main as *const () as u32,
        task3_ram_base,
        task_ram_size,
    );

    // ---- 2. MPU (start with task 0's view; PendSV reprograms on switch) ----
    mpu::reconfigure_for_task(task::current());
    defmt::info!("kernel: MPU configured for task 0");

    // ---- 3. PendSV must be lowest priority so it never preempts SVCall ----
    //
    // We pend PendSV from inside the SVCall handler (via SYSCALL_YIELD).
    // For PendSV to fire only *after* SVCall returns, its priority must be
    // strictly lower than SVCall's. Setting it to 0xFF gives the lowest
    // possible priority on M0+ (only the top 2 bits of priority are
    // implemented, so 0xFF == 0xC0 == priority 3).
    unsafe {
        cp.SCB
            .set_priority(cortex_m::peripheral::scb::SystemHandler::PendSV, 0xFF);
    }

    cp.SYST.set_clock_source(SystClkSource::Core);
    cp.SYST.set_reload(125_000_000 - 1);
    cp.SYST.clear_current();
    cp.SYST.enable_counter();
    cp.SYST.enable_interrupt();
    defmt::info!("kernel: SysTick armed");

    // ---- 5. Drop to unprivileged thread mode and jump to task 0 ----
    let t = task::current();
    defmt::info!(
        "kernel: dropping to task0: entry=0x{:08x} PSP=0x{:08x}",
        t.entry_pc,
        t.initial_psp,
    );
    unsafe { bootstrap_user(t) }
}
