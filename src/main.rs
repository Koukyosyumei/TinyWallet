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
static mut TASK1_RAM: TaskRam = TaskRam([0; 8192]);

// =============================================================================
// Syscall ABI
// =============================================================================
//
// User side: place the syscall number in r0 and args in r1..r3, then `svc #0`.
// Kernel SVCall handler reads the saved frame from PSP, dispatches, and
// writes the return value back into the saved r0 slot so the caller sees it.

const SYSCALL_PRINT: u32 = 1;
const SYSCALL_LED: u32 = 2;
const SYSCALL_YIELD: u32 = 3;
const SYSCALL_SEND: u32 = 4;
const SYSCALL_RECV: u32 = 5;

/// Maximum bytes we'll copy in a single SEND/RECV. Bounds the validation
/// surface and keeps the rendezvous logic simple.
const IPC_MAX_BYTES: u32 = 64;

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

/// Cooperative yield: kernel pends PendSV, which (after this SVC returns)
/// switches to the next ready task. Returns when this task is rescheduled.
#[allow(dead_code)]
fn sys_yield() {
    syscall2(SYSCALL_YIELD, 0, 0);
}

#[inline(always)]
fn syscall3(num: u32, a1: u32, a2: u32, a3: u32) -> u32 {
    let ret: u32;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("r0") num,
            in("r1") a1,
            in("r2") a2,
            in("r3") a3,
            lateout("r0") ret,
        );
    }
    ret
}

/// Send a message to another task. Blocks the caller until the target is
/// in `recv` to pick it up (synchronous rendezvous). Returns 0 on success.
fn sys_send(target: u32, msg: &[u8]) -> u32 {
    syscall3(SYSCALL_SEND, target, msg.as_ptr() as u32, msg.len() as u32)
}

/// Receive a message into `buf`. Blocks until some task sends to us.
/// Returns a packed u32: low 16 bits = message length, bits 16..23 =
/// sender task ID. `0xFFFFFFFF` indicates an error (e.g. invalid buffer).
fn sys_recv(buf: &mut [u8]) -> u32 {
    syscall2(SYSCALL_RECV, buf.as_mut_ptr() as u32, buf.len() as u32)
}

// =============================================================================
// User task
// =============================================================================
//
// Runs in unprivileged thread mode on PSP. MPU-allowed memory:
//   - read+execute  flash             (so the task can run its own code)
//   - read+write    its own 8 KiB RAM (TASK0_RAM)
// Anything else — kernel RAM, peripherals — should fault.
/// Task 0 — the **client**. Toggles the blue LED on its own loop, then
/// every 4th iteration sends "ping" to task 1 and waits for the reply.
extern "C" fn task0_main() -> ! {
    sys_print(b"hello task0 (client)");
    let mut counter: u32 = 0;
    let mut reply_buf = [0u8; 16];
    loop {
        sys_set_led(2, counter & 1 == 0); // blue toggle
        counter = counter.wrapping_add(1);
        for _ in 0..400_000 {
            unsafe { core::arch::asm!("nop") };
        }
        if counter.is_multiple_of(4) {
            sys_send(1, b"ping");
            let _ = sys_recv(&mut reply_buf);
        }
    }
}

/// Task 1 — the **server**. Sits in `recv` waiting for requests; on each
/// one, toggles the green LED and replies "pong". The fact that green
/// blinks at all proves IPC delivery is happening — task 0 doesn't yield
/// between pings, so without IPC task 1 would never run.
extern "C" fn task1_main() -> ! {
    sys_print(b"hello task1 (server)");
    let mut counter: u32 = 0;
    let mut req_buf = [0u8; 16];
    loop {
        let _ = sys_recv(&mut req_buf); // blocks until task 0 pings
        sys_set_led(1, counter & 1 == 0); // green toggle
        counter = counter.wrapping_add(1);
        for _ in 0..200_000 {
            unsafe { core::arch::asm!("nop") };
        }
        sys_send(0, b"pong");
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

    /// Scheduling + IPC state. Phase 2D introduces blocking states; the
    /// scheduler skips them when picking the next task to run.
    #[derive(Clone, Copy)]
    pub enum TaskState {
        Ready,
        /// Task called RECV and is parked waiting for some sender to target
        /// it. `out_ptr` / `max_len` describe the buffer where the message
        /// should be written when delivery happens.
        BlockedOnRecv { out_ptr: u32, max_len: u32 },
        /// Task called SEND and is parked because the target wasn't in
        /// RECV. Stays here until the target's RECV picks up the message.
        BlockedOnSend {
            target: u8,
            msg_ptr: u32,
            msg_len: u32,
        },
    }

    pub struct Task {
        /// Function pointer the kernel jumps to when first running this task.
        pub entry_pc: u32,
        /// Initial PSP value (top of the task's stack, 8-byte aligned).
        /// Used only by `bootstrap_user` for the first task; thereafter
        /// `saved_psp` carries the actual SP across context switches.
        pub initial_psp: u32,
        /// PSP value at the most recent suspension. For task 0, undefined
        /// until its first yield. For task 1+, init code seeds this to point
        /// at a fake exception frame so the first PendSV restore lands at
        /// `entry_pc` cleanly.
        pub saved_psp: u32,
        /// MPU regions granted to this task, mirrored for syscall pointer
        /// validation. Empty entries (size == 0) are skipped.
        pub regions: [TaskRegion; 4],
        /// Scheduler state; blocked tasks are skipped by `pick_next_ready`.
        pub state: TaskState,
    }

    pub const N_TASKS: usize = 2;

    const EMPTY: TaskRegion = TaskRegion { base: 0, size: 0, perms: 0 };
    const EMPTY_TASK: Task = Task {
        entry_pc: 0,
        initial_psp: 0,
        saved_psp: 0,
        regions: [EMPTY; 4],
        state: TaskState::Ready,
    };

    pub static mut TASKS: [Task; N_TASKS] = [EMPTY_TASK; N_TASKS];

    /// Index of the task that's currently executing in user mode. Updated
    /// by PendSV during a context switch. Read by syscall handlers (so
    /// `validate_buf` checks against the right task's regions) and by
    /// PendSV (to know which slot to save the outgoing PSP into).
    pub static mut CURRENT_TASK: usize = 0;

    /// Populate task 0. Bootstraps via `bootstrap_user` so it doesn't need
    /// a fake initial exception frame — kernel `main()` calls
    /// `bootstrap_user(task0)` directly which loads `initial_psp` and `bx`s
    /// to `entry_pc`. After task 0's first yield, `saved_psp` is set by
    /// PendSV and `initial_psp` is no longer read.
    pub fn init_task0(entry_pc: u32, ram_base: u32, ram_size: u32) {
        // SAFETY: called once at boot before any code can read TASKS.
        unsafe {
            let t = &raw mut TASKS[0];
            (*t).entry_pc = entry_pc;
            (*t).initial_psp = (ram_base + ram_size) & !7;
            (*t).saved_psp = 0; // unused until first yield
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

    /// Populate task 1+. Unlike task 0, this task's first run goes through
    /// PendSV's restore path — so we pre-seed the task's stack with a
    /// well-formed exception frame plus 32 bytes of zeros for r4-r11. When
    /// PendSV pops both, the hardware exception-return lands at `entry_pc`
    /// in unprivileged thread mode.
    ///
    /// Stack layout (grows down from `top`):
    ///
    /// ```text
    /// top         ──────────────  PSP after first run resumes
    ///   -4   xpsr  = 0x01000000
    ///   -8   pc    = entry_pc
    ///   -12  lr    = 0
    ///   -16  r12   = 0
    ///   -20  r3    = 0
    ///   -24  r2    = 0
    ///   -28  r1    = 0
    ///   -32  r0    = 0
    ///   -36..-64  r4-r11 = zeros   ── PendSV pops these first
    /// saved_psp  ──────────────  initial PSP value PendSV sees
    /// ```
    pub fn init_task1(slot: usize, entry_pc: u32, ram_base: u32, ram_size: u32) {
        // SAFETY: called once at boot, single-threaded. Writes into the
        // task's RAM via privileged kernel access (PRIVDEFENA=1 in MPU).
        unsafe {
            let t = &raw mut TASKS[slot];
            (*t).entry_pc = entry_pc;
            let top = (ram_base + ram_size) & !7;
            (*t).initial_psp = top;

            // Fake exception frame at top-32 .. top.
            let frame_base = top - 32;
            (frame_base as *mut u32).add(0).write_volatile(0); // r0
            (frame_base as *mut u32).add(1).write_volatile(0); // r1
            (frame_base as *mut u32).add(2).write_volatile(0); // r2
            (frame_base as *mut u32).add(3).write_volatile(0); // r3
            (frame_base as *mut u32).add(4).write_volatile(0); // r12
            (frame_base as *mut u32).add(5).write_volatile(0); // lr
            (frame_base as *mut u32).add(6).write_volatile(entry_pc); // pc
            (frame_base as *mut u32).add(7).write_volatile(0x0100_0000); // xpsr (T bit)

            // Zeros for r4-r11 at top-64 .. top-32.
            let saved_psp = top - 64;
            for i in 0..8 {
                (saved_psp as *mut u32).add(i).write_volatile(0);
            }
            (*t).saved_psp = saved_psp;

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

    /// Returns the task currently running in user mode (per CURRENT_TASK).
    /// Read from kernel context (syscall handlers, PendSV) — never from a
    /// user task directly.
    pub fn current() -> &'static Task {
        // SAFETY: CURRENT_TASK is written only by PendSV (one writer) and
        // read by handlers that run with PendSV preempted/disabled, so
        // there's no torn-read race in this PoC. TASKS is initialized once
        // at boot.
        unsafe {
            let idx = CURRENT_TASK;
            &*(&raw const TASKS[idx])
        }
    }

    /// Round-robin scheduler: starting after `current`, return the index
    /// of the next Ready task. Wraps around. If no task is Ready (true
    /// deadlock), panics.
    pub fn pick_next_ready(current: usize) -> usize {
        for i in 1..=N_TASKS {
            let idx = (current + i) % N_TASKS;
            if matches!(
                unsafe { (*(&raw const TASKS[idx])).state },
                TaskState::Ready
            ) {
                return idx;
            }
        }
        panic!("scheduler: no Ready tasks");
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

/// Pack (sender, len) for return from RECV: low 16 bits = len,
/// bits 16..23 = sender index.
fn encode_recv_result(sender: u8, len: u32) -> u32 {
    ((sender as u32) << 16) | (len & 0xFFFF)
}

/// Write the saved r0 slot of a *Blocked* task's exception frame so that
/// when the task is later resumed, its in-flight syscall sees `value` as
/// the return register.
///
/// The task must currently be Blocked (i.e. its full register state has
/// been saved by PendSV and hasn't been popped back). Layout under the
/// saved PSP: 32 bytes of r4-r11, then the 8-word HW exception frame.
/// r0 lives at offset 32 from saved_psp.
unsafe fn poke_blocked_task_r0(task_idx: usize, value: u32) {
    unsafe {
        let psp = (*(&raw const task::TASKS[task_idx])).saved_psp;
        let r0_slot = (psp + 32) as *mut u32;
        r0_slot.write_volatile(value);
    }
}

fn syscall_send(target: u32, msg_ptr: u32, msg_len: u32) -> u32 {
    let target = target as usize;
    if target >= task::N_TASKS || msg_len > IPC_MAX_BYTES {
        return u32::MAX;
    }
    let cur = unsafe { task::CURRENT_TASK };
    if target == cur {
        return u32::MAX; // self-send is meaningless and would deadlock
    }
    let cur_task = unsafe { &*(&raw const task::TASKS[cur]) };
    if !task::validate_buf(cur_task, msg_ptr, msg_len, task::PERM_R) {
        return u32::MAX;
    }

    // Snapshot the target's state once. If it's BlockedOnRecv, we deliver
    // the message right now and wake it; otherwise we block the caller
    // until the target eventually calls RECV.
    let target_state = unsafe { (*(&raw const task::TASKS[target])).state };
    match target_state {
        task::TaskState::BlockedOnRecv {
            out_ptr,
            max_len,
        } => {
            let copy_len = msg_len.min(max_len);
            // SAFETY: kernel is privileged (PRIVDEFENA=1), both buffers
            // were validated against their owning task's regions when the
            // tasks issued their respective syscalls.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    msg_ptr as *const u8,
                    out_ptr as *mut u8,
                    copy_len as usize,
                );
                let target_t = &raw mut task::TASKS[target];
                (*target_t).state = task::TaskState::Ready;
                // Deliver the recv result through the target's saved frame
                // so its sys_recv call returns with the right value.
                poke_blocked_task_r0(target, encode_recv_result(cur as u8, copy_len));
            }
            // Caller stays Ready; pend a switch so the freshly-Ready
            // target gets a chance to run.
            cortex_m::peripheral::SCB::set_pendsv();
            0
        }
        _ => {
            // Block caller. Its r4-r11 + exception frame will be saved by
            // the PendSV that fires next; when the target eventually
            // RECVs, that handler will copy from msg_ptr and wake us.
            unsafe {
                let cur_t = &raw mut task::TASKS[cur];
                (*cur_t).state = task::TaskState::BlockedOnSend {
                    target: target as u8,
                    msg_ptr,
                    msg_len,
                };
            }
            cortex_m::peripheral::SCB::set_pendsv();
            // The 0 we return here will be overwritten by the rendezvous
            // handler before we ever resume — see poke_blocked_task_r0.
            0
        }
    }
}

fn syscall_recv(out_ptr: u32, max_len: u32) -> u32 {
    if max_len > IPC_MAX_BYTES {
        return u32::MAX;
    }
    let cur = unsafe { task::CURRENT_TASK };
    let cur_task = unsafe { &*(&raw const task::TASKS[cur]) };
    if !task::validate_buf(cur_task, out_ptr, max_len, task::PERM_W) {
        return u32::MAX;
    }

    // Look for a task already blocked-on-send-to-us. If found, deliver
    // its message immediately and wake it.
    for sender in 0..task::N_TASKS {
        if sender == cur {
            continue;
        }
        let sender_state = unsafe { (*(&raw const task::TASKS[sender])).state };
        if let task::TaskState::BlockedOnSend {
            target,
            msg_ptr,
            msg_len,
        } = sender_state
        {
            if target as usize == cur {
                let copy_len = msg_len.min(max_len);
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        msg_ptr as *const u8,
                        out_ptr as *mut u8,
                        copy_len as usize,
                    );
                    let sender_t = &raw mut task::TASKS[sender];
                    (*sender_t).state = task::TaskState::Ready;
                    // Sender's sys_send will see 0 (success) when it resumes.
                    poke_blocked_task_r0(sender, 0);
                }
                return encode_recv_result(sender as u8, copy_len);
            }
        }
    }

    // No pending sender — block caller.
    unsafe {
        let cur_t = &raw mut task::TASKS[cur];
        (*cur_t).state = task::TaskState::BlockedOnRecv { out_ptr, max_len };
    }
    cortex_m::peripheral::SCB::set_pendsv();
    // Will be overwritten by the rendezvous-on-send handler.
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
    let a3 = frame[3];

    let ret = match num {
        SYSCALL_PRINT => syscall_print(a1, a2),
        SYSCALL_LED => syscall_led(a1, a2),
        SYSCALL_YIELD => {
            // Pend PendSV. Because PendSV is configured to lowest priority,
            // it fires *after* this SVC's exception return — at which point
            // PSP is back to the user task's pre-SVC stack, which is what
            // we want PendSV to save.
            cortex_m::peripheral::SCB::set_pendsv();
            0
        }
        SYSCALL_SEND => syscall_send(a1, a2, a3),
        SYSCALL_RECV => syscall_recv(a1, a2),
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
    // writer. Phase 2C: SysTick no longer drives any LED — task 1 owns
    // green via SYSCALL_LED. SysTick stays armed for timing reference and
    // as the future preemption hook (Phase 2D could pend PendSV here for
    // round-robin preemption).
    let n = KERNEL_TICKS.load(Ordering::Relaxed).wrapping_add(1);
    KERNEL_TICKS.store(n, Ordering::Relaxed);
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
// Kernel: per-task MPU reprogramming (used on context switch)
// =============================================================================
//
// Each Task carries generic R/W/X permission bits in its TaskRegion list.
// On switch, those need to be translated to the RP2040's RASR encoding
// (AP field, XN bit) and written to MPU regions 0..N. PRIVDEFENA stays
// on so kernel-mode access is unaffected.

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

fn reconfigure_mpu_for_task(t: &task::Task) {
    // Both tasks currently use exactly two regions (RAM + flash). When
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

// =============================================================================
// Kernel: PendSV — cooperative context switch
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
        reconfigure_mpu_for_task(next_task);
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
    // Each Task entry holds entry_pc / initial_psp / saved_psp / regions.
    // Task 0 boots via `bootstrap_user` (uses initial_psp). Task 1 boots
    // via PendSV's restore path on the first switch (uses saved_psp,
    // which init_task1 seeds with a fake exception frame).
    let task0_ram_base = &raw const TASK0_RAM as u32;
    let task1_ram_base = &raw const TASK1_RAM as u32;
    let task_ram_size: u32 = 8 * 1024;
    defmt::info!(
        "kernel: TASK0_RAM @ 0x{:08x}, TASK1_RAM @ 0x{:08x} (each {} B)",
        task0_ram_base,
        task1_ram_base,
        task_ram_size,
    );
    task::init_task0(
        task0_main as *const () as u32,
        task0_ram_base,
        task_ram_size,
    );
    task::init_task1(
        1,
        task1_main as *const () as u32,
        task1_ram_base,
        task_ram_size,
    );

    // ---- 2. MPU (start with task 0's view; PendSV reprograms on switch) ----
    reconfigure_mpu_for_task(task::current());
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

    // ---- 4. SysTick heartbeat (~1 Hz off uncalibrated boot ROSC) ----
    cp.SYST.set_clock_source(SystClkSource::Core);
    cp.SYST.set_reload(6_500_000 - 1);
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
