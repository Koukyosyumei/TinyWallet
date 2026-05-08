//! On-board GPIO and LED driver for the XIAO RP2040.
//!
//! Three on-board LEDs (active LOW): R=GPIO17, G=GPIO16, B=GPIO25. The
//! kernel owns all three; user tasks affect them only via SYSCALL_LED.

// RP2040 peripheral addresses (datasheet § 2.14, 2.19, 2.20).
const RESETS_BASE: u32 = 0x4000_C000;
const RESETS_RESET_CLR: u32 = RESETS_BASE | 0x3000; // atomic-clear alias
const RESETS_RESET_DONE: u32 = RESETS_BASE + 0x008;
const RESET_IO_BANK0: u32 = 1 << 5;
const RESET_PADS_BANK0: u32 = 1 << 8;

const IO_BANK0_BASE: u32 = 0x4001_4000;
const PADS_BANK0_BASE: u32 = 0x4001_C000;
const PADS_BANK0_CLR: u32 = PADS_BANK0_BASE | 0x3000;

const SIO_BASE: u32 = 0xD000_0000;
const SIO_GPIO_OUT_SET: u32 = SIO_BASE + 0x014;
const SIO_GPIO_OUT_CLR: u32 = SIO_BASE + 0x018;
const SIO_GPIO_OE_SET: u32 = SIO_BASE + 0x024;

pub const LED_RED: u32 = 17;
pub const LED_GREEN: u32 = 16;
pub const LED_BLUE: u32 = 25;

fn write(addr: u32, val: u32) {
    unsafe {
        (addr as *mut u32).write_volatile(val);
    }
}
fn read(addr: u32) -> u32 {
    unsafe { (addr as *const u32).read_volatile() }
}

pub fn init_leds() {
    // Release IO_BANK0 + PADS_BANK0 from reset (they boot held in reset).
    let mask = RESET_IO_BANK0 | RESET_PADS_BANK0;
    write(RESETS_RESET_CLR, mask);
    while (read(RESETS_RESET_DONE) & mask) != mask {}

    for &pin in &[LED_RED, LED_GREEN, LED_BLUE] {
        // GPIOn_CTRL.FUNCSEL = 5 (SIO). Other CTRL fields = 0.
        write(IO_BANK0_BASE + 4 + pin * 8, 5);
        // PADS_BANK0_GPIOn: clear OD (output-disable, bit 7) so the pad drives.
        write(PADS_BANK0_CLR + 4 + pin * 4, 1 << 7);
        // Drive HIGH first (LED off — active low) so we don't flash on boot.
        write(SIO_GPIO_OUT_SET, 1 << pin);
        // Then enable output.
        write(SIO_GPIO_OE_SET, 1 << pin);
    }
}

/// Active-low: `on=true` clears the pin (drives low).
pub fn set(pin: u32, on: bool) {
    if on {
        write(SIO_GPIO_OUT_CLR, 1 << pin);
    } else {
        write(SIO_GPIO_OUT_SET, 1 << pin);
    }
}
