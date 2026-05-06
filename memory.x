/* tiny-wallet memory layout
 *
 * RP2040 has 264 KiB SRAM:
 *   SRAM0..3  256 KiB contiguous at 0x20000000  (used here as RAM)
 *   SRAM4     4 KiB    at 0x20040000           (unused for now)
 *   SRAM5     4 KiB    at 0x20041000           (unused for now)
 *
 * For the microkernel PoC we put kernel data + bss + stack in SRAM0..3.
 * User-task RAM regions are declared as `#[repr(align(N))]` statics in Rust;
 * the linker places them inside RAM, and the kernel uses their runtime
 * address to set up MPU regions. We don't need a separate MEMORY region for
 * each task — alignment on the type is enough for the MPU.
 */

MEMORY {
    BOOT2 : ORIGIN = 0x10000000, LENGTH = 0x100
    FLASH : ORIGIN = 0x10000100, LENGTH = 2048K - 0x100
    RAM   : ORIGIN = 0x20000000, LENGTH = 256K
}
