//! Task 0 — the **client**. Toggles the blue LED on its own loop, then
//! every 4th iteration sends "ping" to task 1 and waits for the reply.

use crate::syscall::{sys_print, sys_recv, sys_send, sys_set_led};

pub extern "C" fn task0_main() -> ! {
    sys_print(b"hello task0 (client)");
    let mut counter: u32 = 0;
    let mut reply_buf = [0u8; 16];
    loop {
        sys_set_led(2, counter & 1 == 0); // blue toggle
        counter = counter.wrapping_add(1);
        // Busy-wait sized for ~125 MHz core.
        for _ in 0..8_000_000 {
            unsafe { core::arch::asm!("nop") };
        }
        if counter.is_multiple_of(4) {
            sys_send(1, b"ping");
            let _ = sys_recv(&mut reply_buf);
        }
    }
}
