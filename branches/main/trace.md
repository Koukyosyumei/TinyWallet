# OTA Log — Branch: main

[22:36:41] OBSERVE: read AGENTS.md
[22:36:42] OBSERVE: read Cargo.toml
[22:36:42] OBSERVE: read src/main.rs
[22:36:42] OBSERVE: read build.rs
[22:36:43] OBSERVE: read memory.x
[22:36:43] OBSERVE: read .cargo/config.toml
[22:40:35] THINK: Logical-microkernel-via-Embassy rejected; MPU-enforced privileged kernel chosen by user. Embassy executor incompatible — must drop embassy-executor + embassy-rp and go cortex-m-rt + rp2040-hal + rp2040-boot2. Cortex-M0+ MPU has 8 regions, power-of-two sizes, no MemManage exception (escalates to HardFault). PoC #1 scope: kernel boot → MPU setup → PSP setup → drop to unprivileged → user task → SVC syscall round-trip → demonstrate forbidden access faults.
[22:42:19] ACT: wrote Cargo.toml
