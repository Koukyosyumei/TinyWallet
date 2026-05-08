//! Task 1 — the **server**. Sits in `recv` waiting for requests; on each
//! one, toggles the green LED and replies "pong". The fact that green
//! blinks at all proves IPC delivery is happening — task 0 doesn't yield
//! between pings, so without IPC task 1 would never run.

use crate::syscall::{sys_print, sys_recv, sys_send, sys_set_led};

pub extern "C" fn task1_main() -> ! {
    sys_print(b"hello task1 (server)");
    let mut counter: u32 = 0;
    let mut req_buf = [0u8; 16];
    loop {
        let _ = sys_recv(&mut req_buf); // blocks until task 0 pings
        sys_set_led(1, counter & 1 == 0); // green toggle
        counter = counter.wrapping_add(1);
        // Busy-wait sized for ~125 MHz core.
        for _ in 0..4_000_000 {
            unsafe { core::arch::asm!("nop") };
        }
        sys_send(0, b"pong");
    }
}
