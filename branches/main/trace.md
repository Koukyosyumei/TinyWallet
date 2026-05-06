# OTA Log — Branch: main

[22:36:41] OBSERVE: read AGENTS.md
[22:36:42] OBSERVE: read Cargo.toml
[22:36:42] OBSERVE: read src/main.rs
[22:36:42] OBSERVE: read build.rs
[22:36:43] OBSERVE: read memory.x
[22:36:43] OBSERVE: read .cargo/config.toml
[22:40:35] THINK: Logical-microkernel-via-Embassy rejected; MPU-enforced privileged kernel chosen by user. Embassy executor incompatible — must drop embassy-executor + embassy-rp and go cortex-m-rt + rp2040-hal + rp2040-boot2. Cortex-M0+ MPU has 8 regions, power-of-two sizes, no MemManage exception (escalates to HardFault). PoC #1 scope: kernel boot → MPU setup → PSP setup → drop to unprivileged → user task → SVC syscall round-trip → demonstrate forbidden access faults.
[22:42:19] ACT: wrote Cargo.toml


---
_[Checkpoint: 69fbc44c — wrote Cargo.toml]_
---

[22:47:34] OBSERVE: Confirmed RP2040 has MPU (8 regions) via pico-sdk m0plus.h header. User flagged uncertainty; verified before proceeding.
[22:47:35] ACT: wrote feedback_verify_chip_features.md
[22:47:41] ACT: wrote MEMORY.md
[22:51:34] OBSERVE: read mpu.rs
[22:51:46] ACT: edited Cargo.toml
[22:51:55] ACT: wrote memory.x
[22:52:00] ACT: wrote boot2.x
[22:52:06] ACT: wrote build.rs
[22:53:25] ACT: wrote src/main.rs
[22:54:16] ACT: edited src/main.rs
[22:54:33] ACT: edited Cargo.toml
[22:54:38] ACT: edited src/main.rs
[22:54:42] ACT: edited src/main.rs
[22:54:44] ACT: edited src/main.rs
[22:59:17] ACT: edited Cargo.toml/build.rs/memory.x/boot2.x/src/main.rs: dropped embassy stack, switched to cortex-m-rt + rp2040-boot2 + cortex-m (with critical-section-single-core for defmt). Wrote Phase 1 microkernel: MPU setup (2 regions: task0 RAM RW unpriv, flash RX both), PSP drop to unprivileged thread mode via inline asm, SVCall handler dispatching SYSCALL_PRINT, SysTick heartbeat, HardFault handler. Single user task that prints via syscall and deliberately faults on iteration 5 to demo MPU enforcement.


---
_[Checkpoint: 69fbc7c5 — Phase 1 PoC: MPU-enforced microkernel boots, drops to user task, SVC round-trip, MPU isolation demo]_
---



---
_[Checkpoint: 69fbc7d9 — edited src/main.rs; edited src/main.rs; edited Cargo.toml/build.rs/memory.x/boot2.x/src/main.rs: dropped embassy stack, …]_
---



---
_[Checkpoint: 69fbc918 — edited src/main.rs; edited src/main.rs; edited Cargo.toml/build.rs/memory.x/boot2.x/src/main.rs: dropped embassy stack, …]_
---

[23:11:36] ACT: wrote scripts/build-uf2.sh
[23:11:54] ACT: wrote .cargo/config.toml
[23:12:01] OBSERVE: read .gitignore
[23:12:04] ACT: edited .gitignore


---
_[Checkpoint: 69fbcadc — wrote scripts/build-uf2.sh; wrote .cargo/config.toml; edited .gitignore]_
---

[23:15:28] ACT: edited src/main.rs
[23:15:40] ACT: edited src/main.rs
[23:16:01] ACT: edited src/main.rs
[23:16:10] ACT: edited src/main.rs
[23:16:13] ACT: edited src/main.rs
[23:16:20] ACT: edited src/main.rs
[23:16:38] ACT: edited src/main.rs
[23:16:43] ACT: edited src/main.rs
[23:16:50] OBSERVE: read README.md
[23:17:18] ACT: wrote README.md
[23:17:29] ACT: Replaced probe-rs runner with scripts/build-uf2.sh wrapping elf2uf2-rs. cargo run --release now produces ./tiny-wallet.uf2 in project root for WSL→Windows drag-drop flashing.
