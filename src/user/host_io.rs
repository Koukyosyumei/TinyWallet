//! Task 2 — `host_io`. Forwards bytes between USB-CDC and the vault task:
//! USB bytes go to vault via IPC, vault's response goes back out USB.
//! Doesn't interpret the protocol — the vault is the policy enforcer.
//!
//! MaybeUninit avoids `__aeabi_memclr8` on the buffer (see commit
//! ed95e928 — AEABI memclr8 mis-executes on non-8-aligned stack arrays
//! when using rp2040-hal's bootrom helpers; `disable-intrinsics` makes
//! this defensive but harmless).

use crate::syscall::{sys_print, sys_recv, sys_send, sys_usb_read, sys_usb_write, sys_yield};

pub extern "C" fn host_io_main() -> ! {
    sys_print(b"hello host_io");
    let mut buf: core::mem::MaybeUninit<[u8; 256]> = core::mem::MaybeUninit::uninit();
    let buf_ptr = buf.as_mut_ptr() as *mut u8;
    loop {
        // Read from USB.
        let n = unsafe {
            let slice = core::slice::from_raw_parts_mut(buf_ptr, 256);
            sys_usb_read(slice)
        };
        if n == 0 || n == u32::MAX {
            continue;
        }
        let n = n as usize;

        // Forward to vault (task 3).
        let _ = unsafe {
            let slice = core::slice::from_raw_parts(buf_ptr, n);
            sys_send(3, slice)
        };

        // Wait for the vault's response.
        let recv_packed = unsafe {
            let slice = core::slice::from_raw_parts_mut(buf_ptr, 256);
            sys_recv(slice)
        };
        if recv_packed == u32::MAX {
            continue;
        }
        let resp_len = (recv_packed & 0xFFFF) as usize;

        // Write the response back out USB.
        let mut written = 0usize;
        while written < resp_len {
            let r = unsafe {
                let slice = core::slice::from_raw_parts(buf_ptr.add(written), resp_len - written);
                sys_usb_write(slice)
            };
            if r == u32::MAX {
                break;
            }
            if r == 0 {
                sys_yield();
                continue;
            }
            written += r as usize;
        }
    }
}
