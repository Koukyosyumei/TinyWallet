# TinyWallet

A toy hardware wallet for the [Seeed XIAO RP2040](https://wiki.seeedstudio.com/XIAO-RP2040/),
written in Rust as a microkernel demonstration. The goal isn't a production wallet
— it's to *build the isolation properties* a hardware wallet needs from the bottom up:
MPU-enforced privilege separation, syscall ABI, task isolation, kernel-mediated IPC,
and a vault task whose secret key is unreachable from the host-facing code.

## Status

| Phase | What it builds | Status |
|-------|----------------|--------|
| 1     | MPU-enforced microkernel: boot, MPU config, privilege drop, SVC syscall, deliberate-fault demo | ✅ Hardware-validated |
| 2A    | Syscall pointer validation (closes confused-deputy hole) | ✅ Hardware-validated |
| 2B    | Task table as single source of truth for task setup | ✅ Hardware-validated |
| 2C    | PendSV-driven cooperative context switch + per-task MPU reprogramming | ✅ Hardware-validated |
| 2D    | Synchronous L4-style IPC (send / recv with rendezvous) | ✅ Hardware-validated |
| 3A    | USB-CDC enumeration + clock-tree bring-up + kernel-side echo | ✅ Hardware-validated |
| 3B    | host_io user task: USB bytes flow through syscalls + kernel RX ring buffer | ✅ Hardware-validated |
| 4A    | Vault task with ed25519 (`salty`) keypair, IPC-driven get-pubkey + sign | ✅ Hardware-validated |
| 4B    | Sign command end-to-end + offline verification | ✅ Hardware-validated (signature for `"hello\n"` verifies via Python `cryptography`) |
| 4C    | User confirmation gating (button or USB-confirm) | ⏳ |
| 5     | Persistent seed, real entropy, BIP32-style derivation, host protocol | ⏳ |

## Demo

After flashing the current build:

- **Blue + green LEDs** keep blinking continuously (Phase 2D's IPC ping/pong between tasks 0 and 1 — a heartbeat that proves kernel + scheduler + MPU reprogramming all stay healthy).
- **`/dev/ttyACM0`** appears on the host (USB-CDC). From WSL: `usbipd attach --wsl --busid <id>`. Then `stty -F /dev/ttyACM0 raw -echo`.
- Send commands as one-shot transmissions (interactive char-at-a-time would trigger one vault round-trip per character):
  ```bash
  printf "p\n" > /dev/ttyACM0      # → 64 hex chars + \n  (ed25519 public key)
  printf "shello\n" > /dev/ttyACM0 # → 128 hex chars + \n (ed25519 signature of "hello\n")
  ```
- Read responses with `cat /dev/ttyACM0` in another terminal.

The signature can be verified offline against the pubkey to confirm the wallet's keypair is real ed25519.

**Red LED solid = HardFault.** A few seconds later the kernel's diagnostic blinks blue 1-4 times to encode the fault PC's region: 1=flash (our code), 2=RAM (executing data), 3=SCS (corrupt branch / memclr trap), 4=other.

## Build & flash

```bash
cargo run --release    # → produces ./tiny-wallet.uf2
```

Hold the **BOOT** button on the XIAO while plugging it into USB; the board mounts as `RPI-RP2`. Drag `tiny-wallet.uf2` onto that drive (from Windows Explorer at `\\wsl.localhost\Ubuntu\home\.../tiny-wallet/` if you're in WSL). The board reboots into the firmware automatically.

A debug probe is *not* required — observability is via the on-board LEDs and USB-CDC. With a probe (e.g. picoprobe over SWD), swap the runner in `.cargo/config.toml` back to `probe-rs run --chip RP2040` for `defmt` logs over RTT.

## Architecture (current)

```
┌────────── Privileged kernel (handler/main on MSP) ─────────────────────┐
│  cortex-m-rt entry → main():                                           │
│    gpio::init_leds                                                     │
│    usb_io::init   (XOSC + PLLs to 125/48 MHz, USB-CDC up, IRQ unmasked)│
│    init_task0/1/2/3                                                    │
│    reconfigure_mpu_for_task(task0)                                     │
│    PendSV priority 0xFF, SysTick @ 1 Hz                                │
│    bootstrap_user(task0)                                               │
│                                                                        │
│  Exception handlers:                                                   │
│    SVCall      → read PSP frame, dispatch syscall                      │
│    PendSV      → naked: save r4-r11, pendsv_switch (round-robin,       │
│                  reprograms MPU for incoming task), restore r4-r11     │
│    SysTick     → KERNEL_TICKS counter (no LED — task 1 owns green)     │
│    USBCTRL_IRQ → poll usb-device, drain RX ring, deliver to            │
│                  BlockedOnUsbRead task if any                          │
│    HardFault   → red LED solid + 1-4 blue blink PC-region diagnostic   │
│                                                                        │
│  Syscalls:                                                             │
│    PRINT, LED, YIELD, SEND, RECV, USB_READ, USB_WRITE                  │
│  Per-task data:                                                        │
│    entry_pc, initial_psp, saved_psp, regions[4], state                 │
│    state ∈ { Ready, BlockedOnRecv, BlockedOnSend, BlockedOnUsbRead }   │
└────────────────────────────────────────────────────────────────────────┘
┌────── Unprivileged user tasks (PSP, MPU-isolated, 8 KiB RAM each) ─────┐
│ Task 0  client    : blue toggle + ping every 4 iters → task 1          │
│ Task 1  server    : recv ping, green toggle, send pong → task 0        │
│ Task 2  host_io   : USB bytes ↔ vault (syscall + IPC bridge)           │
│ Task 3  vault     : holds ed25519 keypair in own RAM, signs on request │
└────────────────────────────────────────────────────────────────────────┘
```

User-task MPU view: 8 KiB own RAM (RW), 16 MiB flash (RX). Anything else faults.

## Roadmap

### Immediate next steps (resume here)

- [ ] **Phase 4C — user confirmation.** XIAO's BOOT button isn't usable post-reset, so either (a) wire an external button to a GPIO pin and have vault block on it, or (b) implement a USB-side "send 'y' within N seconds to confirm" protocol. Option (b) doesn't have the same security properties as a physical button but demonstrates the architectural pattern.

### Cleanup that's been deferred

- [ ] **Refactor `src/main.rs` into modules.** It's ~1700 lines now with kernel, GPIO, MPU, tasks, syscalls, USB, and four user tasks all inline. Split into `src/{kernel/mod, task, syscall, gpio, mpu, usb, user/...}.rs` before adding more features.
- [ ] **Real entropy.** Phase 4 PoC uses a hardcoded seed. Use the RP2040 ROSC entropy source (read `ROSC_RANDOMBIT`) seeded into a CSPRNG.
- [ ] **Persistent seed.** Currently regenerated on every boot from the constant. A real wallet would derive from a stored recovery seed. Use the last sectors of QSPI flash with a write-protected scheme.

### Larger architectural directions

- [ ] **Phase 5 — host protocol.** Today's commands are single-byte hand-parsed. Move to a structured framing (length-prefixed COBS / CBOR / similar) so multi-message exchanges are robust.
- [ ] **Phase 6 — BIP32-style hierarchical derivation.** Derive child keys per-purpose (signing key, encryption key, etc.) so the root seed never touches signing operations.
- [ ] **Phase 7 — secp256k1.** ed25519 is the easy curve; for actual Bitcoin/Ethereum interop the wallet needs secp256k1 ECDSA. `k256` from RustCrypto is no_std-compatible.
- [ ] **Phase 8 — pin entry / display.** XIAO RP2040 has no display, but the architecture should accommodate one (an SPI/I2C display task with its own MPU region, kernel-mediated input).

### Lessons captured (in `~/.claude/projects/.../memory/`)

- `feedback_aeabi_memclr_alignment.md` — Rust stack `[0u8; N]` lowers to `__aeabi_memclr8`; if the dest isn't 8-aligned the impl mis-executes. Use `MaybeUninit::uninit()` or `repr(align(8))`.
- `feedback_rp2040_hal_bootrom_aeabi.md` — rp2040-hal overrides AEABI memcpy/memclr with calls into the boot ROM, which has unstated alignment constraints that break crypto code. Enable `disable-intrinsics`.
- `feedback_cortex_m0_ctx_switch.md` — When saving/restoring r4-r11 across a Cortex-M0+ context switch, restore the HIGH half (r8-r11) BEFORE the LOW half (r4-r7); the reverse silently corrupts the incoming task's r4-r7 because `ldmia` can only target r0-r7.
- `feedback_verify_chip_features.md` — Verify implementer-optional silicon features (MPU, FPU, etc.) against the datasheet/SDK *before* committing a design that depends on them.
