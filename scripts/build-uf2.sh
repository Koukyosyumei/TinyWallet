#!/usr/bin/env bash
# Cargo runner: convert the built ELF into a UF2 file at the project root.
#
# Used as the `runner` in .cargo/config.toml, so `cargo run --release`
# does `cargo build --release` + this script in one step. Cargo passes
# the ELF path as $1.
#
# Output: ./tiny-wallet.uf2 — drag onto the RPI-RP2 drive when the XIAO
# is in BOOTSEL mode (hold BOOT button while plugging in USB).
#
# Why a script rather than `runner = "elf2uf2-rs"`: putting the UF2 at the
# project root keeps it easy to find from Windows when working in WSL
# (\\wsl.localhost\...\tiny-wallet\tiny-wallet.uf2).

set -euo pipefail

ELF="${1:?usage: build-uf2.sh <path-to-elf>}"
OUT="tiny-wallet.uf2"

elf2uf2-rs "$ELF" "$OUT"
SIZE=$(wc -c < "$OUT")
echo "→ wrote $(realpath "$OUT") (${SIZE} bytes)"
echo "  drag onto the RPI-RP2 drive (hold BOOT while plugging XIAO into USB)"
