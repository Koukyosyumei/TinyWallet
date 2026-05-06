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
