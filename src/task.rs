//! Kernel task table and per-task region descriptors.
//!
//! A task carries the same (base, size, perms) region list that's programmed
//! into the MPU for it. The kernel uses this list to validate user-supplied
//! pointers in syscalls — without it, a malicious task could pass a kernel
//! pointer to SYSCALL_PRINT and have the kernel exfiltrate kernel memory on
//! the task's behalf (the "confused deputy" pattern).

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

/// Scheduling + IPC state. Phase 2D introduced blocking states; the
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
    /// Task called USB_READ and the USB driver had no bytes available.
    /// Stays here until USBCTRL_IRQ delivers the next host packet.
    BlockedOnUsbRead { out_ptr: u32, max_len: u32 },
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

pub const N_TASKS: usize = 4;

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
///
/// IMPORTANT: explicitly initializes `state` and unused regions —
/// TASKS may land in `.uninit`, which cortex-m-rt does NOT zero-init
/// at boot. Leaving `state` garbage causes pattern-match jumps into
/// the SCS region (HardFault with PC=0xExxxxxxx).
pub fn init_task0(entry_pc: u32, ram_base: u32, ram_size: u32) {
    // SAFETY: called once at boot before any code can read TASKS.
    unsafe {
        let t = &raw mut TASKS[0];
        (*t).entry_pc = entry_pc;
        (*t).initial_psp = (ram_base + ram_size) & !7;
        (*t).saved_psp = 0; // unused until first yield
        (*t).state = TaskState::Ready;
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
        (*t).regions[2] = TaskRegion {
            base: 0,
            size: 0,
            perms: 0,
        };
        (*t).regions[3] = TaskRegion {
            base: 0,
            size: 0,
            perms: 0,
        };
    }
}

/// Populate any task whose first run goes through PendSV's restore
/// path (i.e. anything other than task 0, which boots via
/// `bootstrap_user`). We pre-seed the task's stack with a well-formed
/// exception frame plus 32 bytes of zeros for r4-r11. When PendSV
/// pops both, the hardware exception-return lands at `entry_pc` in
/// unprivileged thread mode.
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
pub fn init_task_with_frame(slot: usize, entry_pc: u32, ram_base: u32, ram_size: u32) {
    // SAFETY: called once at boot, single-threaded. Writes into the
    // task's RAM via privileged kernel access (PRIVDEFENA=1 in MPU).
    // IMPORTANT: explicitly initializes `state` and unused regions —
    // see init_task0 for why (TASKS may live in .uninit).
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
        (*t).state = TaskState::Ready;

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
        (*t).regions[2] = TaskRegion {
            base: 0,
            size: 0,
            perms: 0,
        };
        (*t).regions[3] = TaskRegion {
            base: 0,
            size: 0,
            perms: 0,
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

/// Write the saved r0 slot of a *Blocked* task's exception frame so that
/// when the task is later resumed, its in-flight syscall sees `value` as
/// the return register.
///
/// The task must currently be Blocked (i.e. its full register state has
/// been saved by PendSV and hasn't been popped back). Layout under the
/// saved PSP: 32 bytes of r4-r11, then the 8-word HW exception frame.
/// r0 lives at offset 32 from saved_psp.
pub unsafe fn poke_blocked_task_r0(task_idx: usize, value: u32) {
    unsafe {
        let psp = (*(&raw const TASKS[task_idx])).saved_psp;
        let r0_slot = (psp + 32) as *mut u32;
        r0_slot.write_volatile(value);
    }
}
