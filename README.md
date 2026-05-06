# TinyWallet

A toy hardware wallet for the [Seeed XIAO RP2040](https://wiki.seeedstudio.com/XIAO-RP2040/),
written in Rust as a microkernel demonstration.

## Status

**Phase 1 PoC — MPU-enforced microkernel boot.**

The kernel boots in privileged mode, configures the Cortex-M0+ MPU with two
regions (an 8 KiB user-task RAM region, a 16 MiB read+execute flash region),
sets up the user task's PSP, and drops to unprivileged thread mode. The user
task can re-enter the kernel only via SVC syscalls. A deliberate forbidden
peripheral access from the user task should be trapped by the MPU and
escalate to the kernel's HardFault handler.

## Build & flash

```bash
cargo run --release
# → produces ./tiny-wallet.uf2
```

Hold the **BOOT** button on the XIAO while plugging it into USB; the board
mounts as `RPI-RP2`. Drag `tiny-wallet.uf2` onto that drive (from Windows
Explorer at `\\wsl.localhost\Ubuntu\home\.../tiny-wallet/` if you're in WSL).
The board reboots into the firmware automatically.

A debug probe is *not* required for this PoC — observability is via the
on-board LEDs. If you have a probe (e.g. picoprobe over SWD), you can swap
the runner in `.cargo/config.toml` back to `probe-rs run --chip RP2040` to
get `defmt` logs over RTT.

## What you should see

| LED        | Pin    | Meaning                                                |
|------------|--------|--------------------------------------------------------|
| Green      | GPIO16 | Kernel SysTick handler is alive (~0.5 Hz blink)        |
| Blue       | GPIO25 | User task is making syscalls (toggles each iteration)  |
| Red, solid | GPIO17 | HardFault — MPU caught a forbidden access. Board halts |

The user task runs a few syscall iterations (green blinks, blue toggles),
then deliberately writes to `SIO_GPIO_OUT_CLR` to drive the blue LED
*directly*, bypassing the syscall. The MPU should deny this access; the
kernel's HardFault handler should turn red on solid and halt with `wfi`.

If red comes on after a few seconds of blue blinks, the isolation is
working as intended.

## Architecture (Phase 1)

```
┌──── Privileged (handler + privileged thread, MSP) ────┐
│   #[entry] main()                                     │
│   ├─ gpio::init_leds()                                │
│   ├─ mpu::configure(2 regions)                        │
│   ├─ SysTick @ ~1 Hz  (kernel heartbeat → green LED)  │
│   └─ asm!: msr psp / msr control / bx → user task     │
│                                                       │
│   Exception handlers:                                 │
│   ├─ SVCall    → read PSP frame, dispatch syscall     │
│   ├─ SysTick   → toggle green LED                     │
│   └─ HardFault → red LED on solid, halt               │
└───────────────────────────────────────────────────────┘
┌──── Unprivileged thread, PSP, MPU-isolated ───────────┐
│   task0_main():                                       │
│     loop {                                            │
│       sys_set_led(blue, on)        ← SVC #0           │
│       busy-wait                                       │
│       on iter 5: write SIO direct  ← MPU traps        │
│     }                                                 │
└───────────────────────────────────────────────────────┘
```

User-task MPU view: 8 KiB own RAM (RW), 16 MiB flash (RX). Anything else
faults.

## Roadmap

- [x] Phase 1 — MPU-enforced kernel + one user task + one syscall + LED status
- [ ] Phase 2 — Syscall pointer validation (close confused-deputy hole), task
      table, PendSV-driven context switch between two tasks, IPC channels
- [ ] Phase 3 — USB-CDC driver in privileged kernel, host-IO task, framed
      protocol
- [ ] Phase 4 — Vault task with ed25519 signing, button-gated approval
