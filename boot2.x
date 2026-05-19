/* Place the second-stage bootloader (256-byte blob from rp2040-boot2)
 * into the BOOT2 memory region defined in memory.x. The RP2040 mask ROM
 * copies the first 256 bytes of flash into RAM and runs them; this blob
 * configures XIP for QSPI flash so the rest of the firmware can execute
 * in place.
 *
 * INSERT BEFORE .text makes this script a partial extension to
 * cortex-m-rt's link.x without redefining its other sections.
 */

SECTIONS {
    .boot2 ORIGIN(BOOT2) :
    {
        KEEP(*(.boot2));
    } > BOOT2
} INSERT BEFORE .text;
