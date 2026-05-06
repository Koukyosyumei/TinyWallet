# Project Roadmap

## Goal
Design and implement a PoC toy hardware wallet for XIAO RP2040 in Rust, as a microkernel demonstration

## Milestones
- [x] Initial setup
- [x] wrote Cargo.toml
- [x] Phase 1 PoC: MPU-enforced microkernel boots, drops to user task, SVC round-trip, MPU isolation demo
- [x] edited src/main.rs; edited src/main.rs; edited Cargo.toml/build.rs/memory.x/boot2.x/src/main.rs: dropped embassy stack, …
- [x] edited src/main.rs; edited src/main.rs; edited Cargo.toml/build.rs/memory.x/boot2.x/src/main.rs: dropped embassy stack, …
- [x] wrote scripts/build-uf2.sh; wrote .cargo/config.toml; edited .gitignore
- [x] Phase 1.5: LED observability + UF2 build flow
- [x] wrote README.md; Replaced probe-rs runner with scripts/build-uf2.sh wrapping elf2uf2-rs. cargo run --release now produce…
- [x] Phase 1 hardware-validated

## Active Branches
- main (primary)

## Notes
- [2026-05-06 23:19 UTC] `main`: Phase 1 hardware-validated
- [2026-05-06 23:17 UTC] `main`: wrote README.md; Replaced probe-rs runner with scripts/build-uf2.sh wrapping elf2uf2-rs. cargo run --release now produce…
- [2026-05-06 23:17 UTC] `main`: Phase 1.5: LED observability + UF2 build flow
- [2026-05-06 23:12 UTC] `main`: wrote scripts/build-uf2.sh; wrote .cargo/config.toml; edited .gitignore
- [2026-05-06 23:04 UTC] `main`: edited src/main.rs; edited src/main.rs; edited Cargo.toml/build.rs/memory.x/boot2.x/src/main.rs: dropped embassy stack, …
- [2026-05-06 22:59 UTC] `main`: edited src/main.rs; edited src/main.rs; edited Cargo.toml/build.rs/memory.x/boot2.x/src/main.rs: dropped embassy stack, …
- [2026-05-06 22:59 UTC] `main`: Phase 1 PoC: MPU-enforced microkernel boots, drops to user task, SVC round-trip, MPU isolation demo
- [2026-05-06 22:44 UTC] `main`: wrote Cargo.toml
_Add project-wide notes here._
