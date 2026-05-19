# tiny-wallet — TUTORIAL

A slow walkthrough of the codebase, intended for readers who:

- Know basic Rust (let, struct, enum, traits, references, lifetimes — at a survey level).
- Do **not** know how operating systems, microkernels, or low-level Cortex-M programming work.
- Do **not** know what a hardware wallet is.

The [README](README.md) gives the high-level summary and the build instructions. This file walks through every module and explains why each piece exists and how it fits together.

If you've never touched embedded code or kernel code before, start at "Background" and read top-to-bottom. If you already know what an MPU and a syscall are, skip to "Module walkthroughs."

---

## Background

### What a hardware wallet does

When you "own" a cryptocurrency like Bitcoin or Ethereum, what you actually own is a **private key** — a 256-bit number that lets you sign transactions that move money out of your account. Anyone who learns the key can move your money. So the key is the asset.

A hardware wallet is a small dedicated computer whose only job is to:

1. Hold the private key inside it forever.
2. Sign transactions that you've approved.
3. Never reveal the key to anything connected to it (your laptop, the network, even the wallet's own USB-facing software).

The threat model assumes your laptop is compromised — that's the whole reason the device exists. So the wallet has to be designed so that even if the host computer (and even parts of the wallet's own firmware) is malicious, the key cannot escape.

This project is a **toy** demonstration of the architectural pattern. It uses a Seeed XIAO RP2040 board (~$5), a hardcoded seed (real wallets generate one from random entropy at first boot), and ed25519 (real wallets typically use secp256k1 for Bitcoin/Ethereum). The point is not to ship a wallet; the point is to show the isolation mechanism end-to-end.

Concretely, the architectural property we want is: the code that talks to the host (USB) cannot read the bytes that hold the private key. They live in a different memory region that the host-facing code is not allowed to touch.

### What the XIAO RP2040 is

The Seeed XIAO RP2040 is a development board the size of a postage stamp. The chip on it is the RP2040 — the same chip in the Raspberry Pi Pico. Inside the RP2040 are:

- Two ARM **Cortex-M0+** cores running up to 133 MHz (we use one, at 125 MHz).
- 264 KB of SRAM (working memory).
- A USB controller (the chip can pretend to be a USB device — a serial port, keyboard, etc. — when plugged into a host).
- An **MPU** — Memory Protection Unit. **This is the critical part.** See below.
- Various peripherals: GPIO pins (for blinking LEDs), timers, etc.

Flash memory (where our firmware lives) is on a separate chip the RP2040 reads via QSPI. The XIAO has 2 MB. We use less than 40 KB.

### Privileged vs unprivileged mode

Cortex-M0+ has two privilege levels:

- **Privileged mode** — code can do anything: read/write any memory, talk to any peripheral, configure the MPU, take exceptions.
- **Unprivileged mode** — code can do less: it can only access memory the MPU lets it, and many configuration registers are off-limits.

When the chip resets, code starts in privileged mode. We use that mode to set up the MPU (deciding what each "task" is allowed to access), then we drop to unprivileged mode and run the four user tasks. Once unprivileged, a task cannot put itself back to privileged on its own — the only way back is to ask the kernel via a special instruction (`svc`), which the kernel can choose to grant or refuse.

This is the same idea as user-space vs kernel-space on Linux/macOS/Windows, just at a much smaller scale.

### What the MPU does

The MPU is a hardware unit inside the chip that watches every memory access and decides whether to allow it. We program it at boot with a list of **regions**:

> "Region 0: starting at address `0x2000_0000`, 8 KB long, allow the user-mode code to read and write."
> "Region 1: starting at address `0x1000_0000`, 16 MB long, allow read+execute (the user task can run code from flash, but not write to it)."

Anything not covered by a granted region triggers a **HardFault** — the chip stops the code dead and jumps to our HardFault handler. We use this to detect (and visibly indicate) when a task tries to do something it shouldn't.

A subtle but important detail for Armv6-M (the Cortex-M0+'s instruction set): an MPU region must be a **power-of-2 size** (8 KB, 16 KB, …) and **naturally aligned** (an 8 KB region must start at an address that's a multiple of 8 KB). That's why the task RAM blocks in `main.rs` are declared with `#[repr(C, align(8192))]`.

We reprogram the MPU on every context switch so that each task sees only its own RAM. Without that, a malicious or buggy task 2 (host_io) could reach into task 3 (vault) and read the private key bytes — defeating the whole point of the architecture.

### What a microkernel is

In a "monolithic" operating system (Linux, traditional Unix), almost everything runs as part of one big privileged kernel: the file system, the network stack, the device drivers, the scheduler. A bug anywhere can be a security catastrophe because it has full access.

A **microkernel** keeps the privileged part as small as possible: usually just the scheduler, the inter-task message-passing system, and the memory protection. Everything else — file systems, drivers, even the USB stack — runs as **unprivileged tasks** on top. They talk to each other and to hardware indirectly, by sending messages through the kernel.

The benefit: a bug in (say) the USB driver only crashes the USB driver. It cannot reach the keys held by another task, because the MPU stops it.

This project is a microkernel by design — the smallest one that can demonstrate the pattern. We have:

- **A kernel** (privileged, ~600 lines): scheduler, syscall dispatch, IPC, USB driver, MPU programming.
- **Four tasks** (unprivileged):
  - `client` — toy task that pings the server.
  - `server` — toy task that responds.
  - `host_io` — talks to the USB host on behalf of the wallet.
  - `vault` — holds the private key, signs on request.

The vault and host_io are the meaningful pair. The client/server tasks exist mainly to demonstrate that the kernel can run more than one task and that IPC works.

The USB driver living *in the kernel* rather than as a task is a Phase 3 simplification — a stricter design would put it in its own task too. We may move it later.

### Tasks, context switches, system calls

A **task** is, roughly, a process: it has its own stack, its own RAM, and its own slice of execution time. Unlike Linux processes, our tasks all live in the same flash binary — there's no file system, no loader. We hardcode four tasks at compile time.

A **context switch** is the act of pausing one task and resuming another. We do it cooperatively — a task asks to switch by calling `sys_yield`, or implicitly by blocking in `sys_recv` waiting for a message.

A **system call** (or "syscall") is the way an unprivileged task asks the kernel to do something on its behalf — print something, drive an LED, send a message to another task, write to USB. Our implementation uses the ARM `svc` instruction, which raises an exception the kernel handles. See `syscall.rs` and the SVCall handler in `main.rs`.

### What "IPC" means

Inter-process communication. Our tasks talk to each other by sending byte arrays through `sys_send` / `sys_recv`. Sending blocks the sender until the recipient has called `recv`; receiving blocks until someone sends. This style is called **synchronous rendezvous** — it's the simplest IPC primitive, and it's what microkernels like L4 use.

Because messages flow through the kernel, the kernel can enforce policy (e.g. validate that the message buffer is inside the sender's region). Crucially, the recipient task gets a *copy* of the message via the kernel — the sender's memory is never directly readable by the recipient.

That's how the host_io ↔ vault link works: host_io can ask the vault to "sign these bytes," and the vault returns the signature, but neither side has direct access to the other's memory.

---

## The architecture in one picture

```
                      ┌──────────────┐
                      │   Host PC    │
                      │ (untrusted)  │
                      └──────┬───────┘
                             │ USB-CDC
                  /dev/ttyACM0
                             │
   ┌─────────────────────────▼──────────────────────────┐
   │  XIAO RP2040 (privileged kernel + MPU isolation)   │
   │                                                    │
   │   ┌──────────┐  ┌──────────┐  ┌──────────────┐     │
   │   │ task 0   │  │ task 1   │  │ task 2       │     │
   │   │ client   │  │ server   │  │ host_io      │     │
   │   │ (toy)    │  │ (toy)    │  │ USB ↔ vault  │     │
   │   └────┬─────┘  └────┬─────┘  └──────┬───────┘     │
   │        │ syscalls    │ syscalls      │ syscalls    │
   │        ▼             ▼               ▼             │
   │   ┌──────────────────────────────────────────┐     │
   │   │   Kernel: scheduler, IPC, USB, MPU       │     │
   │   │   (the only privileged code)             │     │
   │   └────────────────────┬─────────────────────┘     │
   │                        │ IPC                       │
   │                        ▼                           │
   │                  ┌──────────────┐                  │
   │                  │ task 3       │                  │
   │                  │ vault        │                  │
   │                  │ holds key,   │                  │
   │                  │ signs only   │                  │
   │                  └──────────────┘                  │
   └────────────────────────────────────────────────────┘
```

The **trust boundary** the wallet enforces: bytes flow through `host_io` to `vault`, `vault` returns a signature, and the seed bytes inside vault's RAM cannot be read by any other task — even if `host_io` is fully compromised. The MPU is what enforces that.

---

## Boot flow

When you plug in the board (or press reset):

1. The RP2040's mask ROM (a small program built into the chip) runs first. It looks at the start of QSPI flash, copies the first 256 bytes (called "boot2") into RAM, and jumps to it. Boot2 sets up the QSPI controller so the rest of the firmware can run directly from flash.
2. cortex-m-rt's reset handler runs. It zeroes our `.bss` section (uninitialized statics), copies our `.data` section from flash into RAM, sets up the MSP (main stack pointer), and calls our `main()`.
3. Our `main()` (in `src/main.rs`):
   - Initializes the LEDs.
   - Initializes the USB peripheral and clock tree.
   - Populates the task table (four tasks, each with its own RAM region and entry point).
   - Programs the MPU for task 0's view.
   - Sets up SysTick (the kernel's heartbeat timer).
   - Drops to unprivileged mode and `bx`'s into task 0's entry function.
4. Task 0 starts running on its own stack, in unprivileged mode. From here on, the kernel only runs in response to:
   - **SVC exceptions** — a user task issued a syscall.
   - **SysTick exception** — the periodic kernel heartbeat.
   - **USBCTRL_IRQ** — the host sent a USB packet.
   - **PendSV exception** — a cooperative context switch was pended.
   - **HardFault** — something went wrong (usually an MPU violation).

After `main()` "returns" into task 0, the kernel never executes linearly again. It only fires on exceptions.

---

## Module walkthroughs

The codebase is split into small files by concern:

```
src/
  main.rs       — #[entry], ISRs, bootstrap_user, .boot2, task RAM statics
  syscall.rs    — user-side ABI (SYSCALL_* constants, syscall2/3, sys_*)
  task.rs       — Task struct, scheduler, validate_buf, poke_blocked_task_r0
  gpio.rs       — on-board LED driver
  mpu.rs        — MPU configuration + per-task reprogramming
  usb.rs        — USB-CDC + RX ring + USBCTRL_IRQ
  kernel.rs     — kernel-side syscall handlers (called from SVCall)
  user/{client,server,host_io,vault}.rs  — the four task entry functions
```

We'll walk through each in roughly the order it gets used during boot.

---

### `src/main.rs` — the entry point and ISR boundary

This is the binary's "root" file. It contains:

- The `.boot2` blob that the mask ROM runs first.
- The user task RAM blocks (`TASK0_RAM` … `TASK3_RAM`).
- All the exception handlers (SVCall, SysTick, HardFault, PendSV).
- `bootstrap_user` — the one-way trapdoor that drops to unprivileged mode.
- The `#[entry] fn main()` — kernel boot.

#### `BOOT2` — the bootloader

```rust
#[unsafe(link_section = ".boot2")]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;
```

`#[link_section = ".boot2"]` tells the linker to put this 256-byte array at a special address (the start of flash). The mask ROM reads exactly those 256 bytes at boot.

`#[used]` tells the compiler **not** to optimize this static away even though no Rust code references it — the linker reads it directly, not us.

The blob itself comes from the `rp2040-boot2` crate, which provides one for each common QSPI flash chip. The XIAO uses the W25Q080.

#### Task RAM blocks

```rust
#[repr(C, align(8192))]
struct TaskRam(#[allow(dead_code)] [u8; 8192]);

static mut TASK0_RAM: TaskRam = TaskRam([0; 8192]);
// … TASK1_RAM, TASK2_RAM, TASK3_RAM
```

Each task gets an 8 KB block of RAM, **8 KB-aligned**. The alignment matters because, recall, MPU regions on Armv6-M must be naturally aligned to their size. We want each task's RAM to be exactly one MPU region, so we make sure it can be one.

These blocks hold each task's **stack** and any static-like data the task uses. The vault's keypair, for instance, is constructed on task 3's stack inside `TASK3_RAM`.

#### `SVCall` — the syscall dispatcher

When an unprivileged task issues `svc #0`, the chip raises an SVCall exception, which vectors here:

```rust
#[exception]
unsafe fn SVCall() {
    let psp: *mut u32;
    unsafe { core::arch::asm!("mrs {}, psp", out(reg) psp) };
    let frame = unsafe { core::slice::from_raw_parts_mut(psp, 8) };

    let num = frame[0];
    let a1  = frame[1];
    let a2  = frame[2];
    let a3  = frame[3];

    let ret = match num {
        SYSCALL_PRINT     => kernel::syscall_print(a1, a2),
        SYSCALL_LED       => kernel::syscall_led(a1, a2),
        SYSCALL_YIELD     => { SCB::set_pendsv(); 0 }
        SYSCALL_SEND      => kernel::syscall_send(a1, a2, a3),
        SYSCALL_RECV      => kernel::syscall_recv(a1, a2),
        SYSCALL_USB_READ  => kernel::syscall_usb_read(a1, a2),
        SYSCALL_USB_WRITE => kernel::syscall_usb_write(a1, a2),
        _ => u32::MAX,
    };
    frame[0] = ret;
}
```

When an exception is taken on Cortex-M, the hardware automatically saves r0..r3, r12, lr, pc, xpsr to the **active stack**. Because our user tasks run on the Process Stack Pointer (PSP, separate from the kernel's MSP), the saved frame is on PSP. The handler reads it back: r0 holds the syscall number, r1..r3 hold the arguments.

We `match` on the syscall number and call the appropriate kernel-side handler in `kernel.rs`. The return value gets written back into the saved r0 slot, so when the exception returns, the user task's `let ret: u32` (the `lateout("r0") ret` in `syscall.rs`) sees it.

This is how user code "calls" kernel code without ever holding a function pointer to it — control flows through the exception vector table, which only privileged code (the kernel itself, at boot) can write.

#### `HardFault` — the visible diagnostic

When something faults — bad memory access, bad jump, etc. — the chip vectors here. We turn the LED red, then blink the blue LED a number of times to encode the fault PC's region:

- 1 blink: PC was in flash → the offending instruction is in our code.
- 2 blinks: PC was in RAM → we somehow tried to execute data (usually a corrupted function pointer).
- 3 blinks: PC was in the System Control Space (~0xE000_0000) → usually means an exception fired during another exception, or a bootrom helper misexecuted.
- 4 blinks: PC was somewhere else → a function pointer was uninitialized (often address 0).

Useful when you don't have a debugger attached. The README's troubleshooting table maps blink counts to past root causes.

#### `PendSV` — the context switcher

This is the most subtle piece of the kernel. PendSV is configured to the **lowest priority** in the chip — meaning if any other exception is pending, it runs first. We pend PendSV from inside SVCall (when the syscall is `SYSCALL_YIELD` or when an IPC blocks the caller). Because PendSV has lower priority than SVCall, it doesn't fire until SVCall has fully returned. By the time PendSV runs, the user task's stack is in its "real" pre-SVC state — which is what we want to save.

PendSV is written as a **naked function** with hand-written assembly:

```rust
#[unsafe(naked)]
pub unsafe extern "C" fn PendSV() {
    core::arch::naked_asm!(
        "mrs   r0, psp",
        "subs  r0, #32",
        "stmia r0!, {{r4-r7}}",
        // … save r8-r11, call into Rust, restore everything …
    );
}
```

Why naked? Because the normal Rust function preamble would clobber r4-r11 before we get a chance to save them. Cortex-M0+ saves r0-r3, r12, lr, pc, xpsr automatically on exception entry — but **not** r4-r11 (those are "callee-saved" by convention and the hardware doesn't push them). If we let the compiler emit a normal function preamble, it would overwrite r4-r11 to set up its own stack frame, losing the user task's state.

The handler:

1. **Saves r4-r11** to a 32-byte slot below the current PSP. The code goes through r4-r7 as temporaries when saving r8-r11 — Cortex-M0+'s `stmia` can only target r0-r7, so we have to bounce through them.
2. **Calls `pendsv_switch`** (a regular Rust function) with the new low-water mark as its argument. That function picks the next ready task, reprograms the MPU for it, and returns the new task's saved PSP.
3. **Restores r4-r11** from the new task's stack — **HIGH half first, then LOW half**. The order matters; doing it the other way silently corrupts r4-r7 (see `feedback_cortex_m0_ctx_switch.md` in `~/.claude/projects/.../memory/`).
4. **Writes the new PSP** and `bx lr` does an exception return, which causes the hardware to pop the standard r0..xpsr frame and resume the new task.

#### `bootstrap_user` — the privilege drop

```rust
unsafe fn bootstrap_user(task: &task::Task) -> ! {
    unsafe {
        core::arch::asm!(
            "msr psp, {psp}",
            "msr control, {ctrl}",
            "isb",
            "bx  {entry}",
            psp   = in(reg) task.initial_psp,
            entry = in(reg) task.entry_pc,
            ctrl  = in(reg) 3u32,
            options(noreturn),
        );
    }
}
```

This is the one-way trapdoor. It:

1. Loads PSP with the top of the task's stack.
2. Writes `CONTROL = 3`, which sets SPSEL=1 (use PSP, not MSP) **and** nPRIV=1 (unprivileged thread mode).
3. ISB — Instruction Synchronization Barrier — forces the privilege change to take effect before the next instruction executes.
4. `bx` jumps to the task's entry function.

After this, we're running unprivileged. There is no return; the only way back to kernel code is via SVC, IRQ, or exception.

The comment in the code notes a subtle bug an earlier version had — using a hardcoded `movs r0, #3` for the CONTROL value, which would clobber the entry address whenever the compiler picked r0 to hold it. The fix is to let the compiler pick the registers and never name r0 explicitly.

#### `main` — kernel boot

The actual `#[entry]` function. Reads top to bottom:

1. **Take ownership of the cortex-m peripherals.** The cortex-m crate gives you a `Peripherals` struct that you can only `take()` once — guarantees there's a single owner of, e.g., the SCB (System Control Block) registers.
2. **Explicitly initialize CURRENT_TASK to 0.** The comment explains why this matters: TASKS lives in `.uninit`, which cortex-m-rt does NOT zero-init at boot. Garbage in the `TaskState` enum tag would let the scheduler's `match` jump to a random address.
3. **`gpio::init_leds()`** — bring up the on-board LEDs.
4. **`usb::init()`** — bring up clocks (the chip boots at ~6.5 MHz; we want 125 MHz) and the USB peripheral.
5. **Populate the task table** — addresses of the task RAM blocks, function pointers to each task's entry, the size of each region.
6. **`mpu::reconfigure_for_task(task::current())`** — program the MPU for task 0's view.
7. **Lower PendSV's priority to 0xFF** so it never preempts SVCall.
8. **Configure SysTick** for a ~1-second tick (125,000,000 cycles at 125 MHz).
9. **`bootstrap_user(t)`** — drop to task 0 and never return.

---

### `src/syscall.rs` — the user-side ABI

```rust
pub const SYSCALL_PRINT:     u32 = 1;
pub const SYSCALL_LED:       u32 = 2;
pub const SYSCALL_YIELD:     u32 = 3;
pub const SYSCALL_SEND:      u32 = 4;
pub const SYSCALL_RECV:      u32 = 5;
pub const SYSCALL_USB_READ:  u32 = 6;
pub const SYSCALL_USB_WRITE: u32 = 7;

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
```

`svc #0` is the ARM instruction "supervisor call." When executed in unprivileged mode, it raises an SVCall exception. The kernel's SVCall handler runs in privileged mode, which is how the privilege bridge happens.

The constraint annotations on the inline asm tell the compiler:

- `in("r0") num` — put `num` in r0 before the instruction.
- `lateout("r0") ret` — after the instruction, read r0 into `ret`. "Late" means the compiler is allowed to use r0 as both an input and the same output register without thinking they conflict.

Each `sys_*` function is a thin wrapper:

```rust
pub fn sys_print(buf: &[u8]) {
    syscall2(SYSCALL_PRINT, buf.as_ptr() as u32, buf.len() as u32);
}
```

`sys_recv` returns a packed u32: low 16 bits hold the message length, bits 16..23 hold the sender task ID. That packing is a Cortex-M0+ pragmatism — it has only one return register and we want both pieces.

The `IPC_MAX_BYTES = 256` constant is the upper bound on a single send/recv. Bounds checking it in the kernel limits the surface a malicious task can exploit by passing absurd lengths.

---

### `src/task.rs` — the task data model

The `Task` struct describes everything the kernel needs to know about a task:

```rust
pub struct Task {
    pub entry_pc: u32,             // function pointer to the task's main()
    pub initial_psp: u32,          // top of the task's stack at boot
    pub saved_psp: u32,            // SP at the most recent suspension
    pub regions: [TaskRegion; 4],  // MPU regions granted to this task
    pub state: TaskState,          // Ready / blocked on something
}
```

`TaskRegion` is a generic `(base, size, perms)` tuple — it intentionally doesn't carry RP2040-specific MPU encoding. The MPU module translates it to RASR bits at switch time. This indirection means we could (in principle) port the kernel to a different ARM chip by only rewriting `mpu.rs`.

Why does the kernel store each task's regions when the MPU is the source of truth? **Because the kernel needs to validate user pointers** before dereferencing them in syscalls. If the user passes a buffer pointer to `sys_print`, the kernel needs to check "is this pointer inside the calling task's regions?" before reading from it. Otherwise a malicious task could pass a kernel address and have the kernel read kernel memory on its behalf — a class of bug called the **confused deputy**.

That validation lives in `task::validate_buf`:

```rust
pub fn validate_buf(task: &Task, ptr: u32, len: u32, need: u8) -> bool {
    let end = match ptr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    for r in &task.regions {
        if r.size == 0 { continue; }
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
```

Note the `checked_add` everywhere — a malicious task could pass `ptr = 0xFFFF_FFF0, len = 0xFFFF_FFFF` to try to overflow the comparison and slip past. `checked_add` rules that out by returning `None` on overflow.

`TaskState` is a simple enum:

```rust
pub enum TaskState {
    Ready,
    BlockedOnRecv     { out_ptr: u32, max_len: u32 },
    BlockedOnSend     { target: u8, msg_ptr: u32, msg_len: u32 },
    BlockedOnUsbRead  { out_ptr: u32, max_len: u32 },
}
```

Blocked states carry the buffer the task was waiting on, so the wakeup path knows where to write.

`pick_next_ready` is the scheduler:

```rust
pub fn pick_next_ready(current: usize) -> usize {
    for i in 1..=N_TASKS {
        let idx = (current + i) % N_TASKS;
        if matches!(unsafe { (*(&raw const TASKS[idx])).state }, TaskState::Ready) {
            return idx;
        }
    }
    panic!("scheduler: no Ready tasks");
}
```

Round-robin: starting after the current task, return the next task that's `Ready`. If everyone's blocked, panic — that means a true deadlock.

`init_task0` and `init_task_with_frame` are two slightly different ways to set up a task. Task 0 is special because we boot into it directly via `bootstrap_user` — it doesn't need a fake exception frame. Tasks 1..3 start running via PendSV's restore path, so we pre-seed their stacks with a fake exception frame so PendSV's `bx lr` lands on the task's entry function in unprivileged thread mode. The comment in `init_task_with_frame` shows the exact stack layout.

`poke_blocked_task_r0` is how the kernel "delivers" a return value to a blocked task:

```rust
pub unsafe fn poke_blocked_task_r0(task_idx: usize, value: u32) {
    unsafe {
        let psp = (*(&raw const TASKS[task_idx])).saved_psp;
        let r0_slot = (psp + 32) as *mut u32;
        r0_slot.write_volatile(value);
    }
}
```

When a task is blocked, its full register state lives on its stack (PendSV pushed r4-r11 below the auto-saved r0..xpsr frame). The saved r0 sits at offset 32 from the saved PSP (32 bytes of r4-r11, then r0 is the first word of the exception frame). Writing into that slot means that when the task is later resumed and the exception-return pops the frame, r0 will contain `value` — which becomes the return value of the syscall the task was waiting in.

This is how, e.g., a blocked `sys_recv` eventually returns the packed `(sender, len)` to the user task.

---

### `src/mpu.rs` — the MPU programmer

This module translates our generic `TaskRegion` into the RP2040's hardware-specific RASR encoding.

```rust
pub fn reconfigure_for_task(t: &task::Task) {
    let regions = [
        Region {
            number: 0,
            base: t.regions[0].base,
            size_bytes: t.regions[0].size,
            attrs: perms_to_rasr_attrs(t.regions[0].perms),
        },
        // region 1: flash (read+execute)
    ];
    configure(&regions);
}
```

`configure` writes each region's `RBAR` and `RASR` registers. The `RBAR` includes a VALID bit + region number, which is a convenient shortcut: we don't need a separate write to `RNR` to select which region we're configuring.

`PRIVDEFENA = 1` (in the CTRL write) means: when running in privileged mode, fall back to the default memory map for any address not covered by a region. So the kernel keeps full access without us having to enumerate every peripheral as a region. Only unprivileged code is restricted to what the regions explicitly allow.

The constants:

- `RASR_AP_PRIV_RW_UNPRIV_RW = 0b011 << 24` — both kernel and user can read/write.
- `RASR_AP_PRIV_RO_UNPRIV_RO = 0b110 << 24` — both can read, neither can write (for flash).
- `RASR_XN = 1 << 28` — execute-never (set for RAM regions so user code can't run instructions out of its own data).
- `RASR_MEM_NORMAL = (1 << 18) | (1 << 17)` — cacheability/sharability bits suitable for SRAM and flash on RP2040.

`size_field` turns "8192 bytes" into the funny RASR encoding (`log2(size) - 1`, in bits 5..1). The table from the ARMv6-M ARM is what tells you that's the right encoding.

---

### `src/usb.rs` — USB-CDC + the IRQ

**USB-CDC** (Communications Device Class) is the standard way for a microcontroller to pretend to be a serial port over USB. When we plug the XIAO into a host, the host sees `/dev/ttyACM0` (Linux/macOS) or `COMx` (Windows). Anything we write to the USB endpoint shows up as bytes on that serial port; anything the host writes shows up in our IRQ.

`init()` brings up the clock tree (XOSC + PLL_SYS @ 125 MHz + PLL_USB @ 48 MHz) and the USB peripheral. The 48 MHz USB clock is mandated by USB spec; the 125 MHz core clock is so we run faster (the chip boots at ~6.5 MHz on its internal ROSC).

After `init()` returns, the USB peripheral is alive and `USBCTRL_IRQ` is unmasked in the NVIC (the chip's interrupt controller).

The IRQ handler:

```rust
#[interrupt]
fn USBCTRL_IRQ() {
    unsafe {
        // poll the USB stack — drives all CDC bookkeeping
        if !device.poll(&mut [serial]) { return; }

        // ALWAYS drain serial.read into the RX ring
        let mut tmp = [0u8; 64];
        if let Ok(n) = serial.read(&mut tmp) {
            if n > 0 { rx_ring_push(&tmp[..n]); }
        }

        // If a task is parked waiting for USB bytes, deliver and wake.
        for idx in 0..task::N_TASKS {
            // … find a BlockedOnUsbRead task, pop the ring into its
            // buffer, mark it Ready, poke r0 with the byte count,
            // pend a context switch.
        }
    }
}
```

The "always drain" is **load-bearing**. If the IRQ doesn't drain `serial.read` on every fire, the OUT endpoint stays full, the chip keeps the IRQ permanently asserted, and the handler refires forever — no user task ever gets to run again. We diagnosed this empirically in Phase 3B; the comment marks it.

The **RX ring** is a kernel-private 256-byte circular buffer. The IRQ pushes into it; `sys_usb_read` (via `kernel::syscall_usb_read`) pops from it.

Why a ring buffer at all? Because USB packets arrive in bursts of up to 64 bytes, but a user task might call `sys_usb_read` with a smaller buffer, or might not be running yet when the host sends. We need to hold the surplus until a task asks for more.

When a task calls `sys_usb_read` and there are no buffered bytes, the kernel parks it in `BlockedOnUsbRead`. The next time USBCTRL_IRQ fires with data, the handler walks the task table, finds the parked task, copies bytes into its buffer, marks it `Ready`, pokes its r0 with the byte count, and pends a context switch.

---

### `src/gpio.rs` — the LED driver

The simplest module. The XIAO has three on-board LEDs at GPIO17 (red), GPIO16 (green), and GPIO25 (blue). They are **active-low** — driving the pin LOW turns the LED on; driving HIGH turns it off.

`init_leds()` does the dance to bring up the GPIO peripheral (it boots held in reset; we have to release it), select the SIO function for each pin, and configure them as outputs with the LED off.

The constants like `SIO_GPIO_OUT_SET` and `SIO_GPIO_OUT_CLR` are RP2040-specific peripheral addresses. The "atomic alias" addresses (the `_CLR` and `_SET` variants) let us atomically modify single bits without a read-modify-write:

```rust
pub fn set(pin: u32, on: bool) {
    if on {
        write(SIO_GPIO_OUT_CLR, 1 << pin);   // active-low: clear pin → LED on
    } else {
        write(SIO_GPIO_OUT_SET, 1 << pin);   // set pin → LED off
    }
}
```

This is what `kernel::syscall_led` ends up calling.

---

### `src/kernel.rs` — kernel-side syscall handlers

Each handler corresponds to one entry in the SVCall match in `main.rs`.

`syscall_print` is the simplest:

```rust
pub fn syscall_print(ptr: u32, len: u32) -> u32 {
    if len > 256 { return u32::MAX; }
    if !task::validate_buf(task::current(), ptr, len, task::PERM_R) {
        defmt::warn!("kernel: rejected SYSCALL_PRINT — buf not in caller's regions");
        return u32::MAX;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    // … print via defmt
    0
}
```

Bound the length, validate the pointer, then dereference. **Always validate before dereferencing** — that's the rule.

`syscall_send` is the interesting one. It encodes synchronous rendezvous:

```rust
match target_state {
    BlockedOnRecv { out_ptr, max_len } => {
        // Target was already waiting — deliver immediately and wake it.
        copy_nonoverlapping(msg_ptr, out_ptr, copy_len);
        target.state = Ready;
        poke_blocked_task_r0(target, encode_recv_result(cur, copy_len));
        SCB::set_pendsv();
        0  // sender stays Ready, returns 0
    }
    _ => {
        // Target wasn't ready — block the caller.
        cur.state = BlockedOnSend { target, msg_ptr, msg_len };
        SCB::set_pendsv();
        0  // will be overwritten when target eventually recvs
    }
}
```

When SEND finds the target already in `BlockedOnRecv`, it can deliver the message right away by copying from sender to receiver and waking the receiver. When the target is **not** ready, the sender blocks instead and waits for the receiver to call RECV — the symmetric case in `syscall_recv` does the corresponding wake-the-sender path.

In both directions, the kernel does the actual byte copy through privileged kernel access (the kernel can read both regions). Receivers never get a pointer into the sender's memory.

`syscall_usb_read` is similar but the wakeup source is `USBCTRL_IRQ` rather than another task's syscall. `syscall_usb_write` calls into the `usbd_serial` driver directly because writes don't block — if the TX buffer is full, we just return 0 and the caller retries.

---

### `src/user/*.rs` — the four user tasks

Each task is a `pub extern "C" fn …() -> !` — never returns, infinite loop. They run in unprivileged thread mode on PSP.

#### `client.rs` — task 0

```rust
pub extern "C" fn task0_main() -> ! {
    sys_print(b"hello task0 (client)");
    let mut counter: u32 = 0;
    let mut reply_buf = [0u8; 16];
    loop {
        sys_set_led(2, counter & 1 == 0);   // toggle blue
        counter = counter.wrapping_add(1);
        for _ in 0..8_000_000 { unsafe { core::arch::asm!("nop") }; }
        if counter.is_multiple_of(4) {
            sys_send(1, b"ping");
            let _ = sys_recv(&mut reply_buf);
        }
    }
}
```

Toggles the blue LED on every iteration; every fourth iteration, sends "ping" to task 1 and waits for "pong." The busy-wait loop is just a delay (we don't have a `sleep` syscall yet).

#### `server.rs` — task 1

Sits in `recv` waiting for a ping; when it gets one, toggles the green LED and replies "pong." The fact that the green LED ever blinks is empirical proof that IPC delivery is happening — task 0 doesn't yield between pings, so without IPC, task 1 would never get scheduled.

#### `host_io.rs` — task 2

The USB-side glue. In a loop:

1. Read bytes from USB (`sys_usb_read`, blocks until a host packet arrives).
2. Forward the bytes to the vault (`sys_send(3, …)`).
3. Wait for the vault's response (`sys_recv`).
4. Write the response back out USB (`sys_usb_write`, retrying on TX-full).

`host_io` does **not** parse the protocol. It just shovels bytes between USB and the vault. That's the architectural commitment: any "protocol smarts" go inside the vault, where compromise can't help an attacker.

The `MaybeUninit` for the buffer is defensive against an old hazard where the compiler's `__aeabi_memclr8` zeroing helper would route through rp2040-hal into the bootrom and misexecute on misaligned stack arrays. With `disable-intrinsics` enabled in `Cargo.toml` (which we did in Phase 4A), this isn't strictly necessary anymore — but the cost is zero and it documents the past hazard. See `feedback_aeabi_memclr_alignment.md` and `feedback_rp2040_hal_bootrom_aeabi.md` in `~/.claude/projects/-home-koukyosyumei-Dev-tiny-wallet/memory/` for the war story.

#### `vault.rs` — task 3

Holds the keypair and signs:

```rust
pub extern "C" fn vault_main() -> ! {
    let seed_bytes: [u8; 32] = [ /* hardcoded for PoC */ ];
    let keypair = salty::Keypair::from(&seed_bytes);
    // …
    loop {
        let recv_packed = sys_recv(&mut req);
        let cmd = req[0];
        let resp_len = match cmd {
            b'p' => /* return pubkey hex */,
            b's' => /* return signature hex over remaining bytes */,
            _    => /* return "?\n" */,
        };
        sys_send(sender, &resp[..resp_len]);
    }
}
```

The `salty` crate is a no_std ed25519 implementation. `keypair.sign(msg)` returns a 64-byte signature; the vault hex-encodes it into the response buffer with the local `hex_encode` helper.

The architectural property: `seed_bytes` lives on the vault's stack inside `TASK3_RAM`. The MPU configuration the kernel applies before each switch ensures that only the vault has read access to that region. `host_io` and the toy tasks have their own RAM regions and cannot reach into vault's. So even if the host PC compromises `host_io` (e.g. by exploiting a parser bug), it can ask the vault to sign things but cannot read the seed.

For a real wallet, we'd also want:

- The seed generated from real entropy at first boot, persisted to flash, wiped from RAM after derivation, and unsealable only with a user-supplied PIN.
- A user-confirmation step before each sign — the device prompts on a display ("Sign payment of $100 to addr X?") and only signs after the user presses a physical button. That's Phase 4C.
- secp256k1 instead of ed25519 for Bitcoin/Ethereum interop.

---

## Failure modes and what blinks mean

When the firmware faults, here's how to read the LEDs:

| LED state | Meaning |
|---|---|
| Red solid | HardFault. The diagnostic blink starts ~1 second later. |
| Red solid + 1 blue blink | PC was in flash. Most common: an MPU violation in user code. Check the defmt log for the exact address. |
| Red solid + 2 blue blinks | PC was in RAM. Usually a corrupted function pointer that pointed into the stack. |
| Red solid + 3 blue blinks | PC was in the SCS region (~0xE000_0000). Usually means we double-faulted, or an AEABI helper jumped through the bootrom function table by mistake. The fix for the AEABI case is `rp2040-hal/disable-intrinsics` in `Cargo.toml`. |
| Red solid + 4 blue blinks | PC was somewhere else entirely — usually 0, meaning a function pointer was uninitialized. |

The blink pattern repeats every ~6 seconds.

If the device just sits silent (no LEDs at all) after flashing, the most likely cause is a panic before `init_leds()` ran — usually in `usb::init()`. Connect a probe and check the defmt log.

---

## Where to go next

- **Phase 4C — user confirmation.** Add a physical-button (or USB-confirm) gate so the vault only signs when the user has explicitly approved. This is the single biggest gap between this toy and a real wallet — without it, a compromised host can ask for arbitrary signatures and the wallet has no way to refuse.
- **Phase 5 — persistent seed + real entropy.** Generate the seed from RP2040's `ROSC_RANDOMBIT` at first boot, store it in flash sealed by a PIN.
- **Phase 6 — BIP32 derivation.** Derive child keys per-purpose so the root seed never participates directly in signing.
- **Phase 7 — secp256k1.** For Bitcoin/Ethereum compat.
- **Phase 8 — display.** Currently we have no way to show the user *what* they're signing. The XIAO has no display, but the architecture should accommodate one as another isolated task.

If you want to dig deeper on the kernel side, the most interesting reading order is:

1. **`src/main.rs` `PendSV`** (the assembly + the `pendsv_switch` Rust helper) — the heart of the context switch.
2. **`src/kernel.rs` `syscall_send` / `syscall_recv`** — the synchronous-rendezvous IPC.
3. **`src/usb.rs` `USBCTRL_IRQ`** — the load-bearing "always drain" pattern and the IRQ→task wakeup.
4. **`src/main.rs` `bootstrap_user`** — the one-way privilege drop.

If you want to actually use this as a wallet — don't. It is a teaching artifact, not a security product.
