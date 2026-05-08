//! User-side syscall ABI.
//!
//! User side: place the syscall number in r0 and args in r1..r3, then `svc #0`.
//! Kernel SVCall handler reads the saved frame from PSP, dispatches, and
//! writes the return value back into the saved r0 slot so the caller sees it.

pub const SYSCALL_PRINT: u32 = 1;
pub const SYSCALL_LED: u32 = 2;
pub const SYSCALL_YIELD: u32 = 3;
pub const SYSCALL_SEND: u32 = 4;
pub const SYSCALL_RECV: u32 = 5;
pub const SYSCALL_USB_READ: u32 = 6;
pub const SYSCALL_USB_WRITE: u32 = 7;

/// Maximum bytes we'll copy in a single SEND/RECV. Bounds the validation
/// surface. Sized for the wallet's IPC: command bytes + payload (sign
/// inputs) or response bytes (hex-encoded signature = 128 chars + slack).
pub const IPC_MAX_BYTES: u32 = 256;

#[inline(always)]
fn syscall2(num: u32, a1: u32, a2: u32) -> u32 {
    let ret: u32;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("r0") num,
            in("r1") a1,
            in("r2") a2,
            lateout("r0") ret,
        );
    }
    ret
}

#[inline(always)]
fn syscall3(num: u32, a1: u32, a2: u32, a3: u32) -> u32 {
    let ret: u32;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("r0") num,
            in("r1") a1,
            in("r2") a2,
            in("r3") a3,
            lateout("r0") ret,
        );
    }
    ret
}

pub fn sys_print(buf: &[u8]) {
    syscall2(SYSCALL_PRINT, buf.as_ptr() as u32, buf.len() as u32);
}

/// Ask the kernel to drive an on-board LED. `which`: 0=red, 1=green, 2=blue.
pub fn sys_set_led(which: u32, on: bool) {
    syscall2(SYSCALL_LED, which, on as u32);
}

/// Cooperative yield: kernel pends PendSV, which (after this SVC returns)
/// switches to the next ready task. Returns when this task is rescheduled.
#[allow(dead_code)]
pub fn sys_yield() {
    syscall2(SYSCALL_YIELD, 0, 0);
}

/// Send a message to another task. Blocks the caller until the target is
/// in `recv` to pick it up (synchronous rendezvous). Returns 0 on success.
pub fn sys_send(target: u32, msg: &[u8]) -> u32 {
    syscall3(SYSCALL_SEND, target, msg.as_ptr() as u32, msg.len() as u32)
}

/// Receive a message into `buf`. Blocks until some task sends to us.
/// Returns a packed u32: low 16 bits = message length, bits 16..23 =
/// sender task ID. `0xFFFFFFFF` indicates an error (e.g. invalid buffer).
pub fn sys_recv(buf: &mut [u8]) -> u32 {
    syscall2(SYSCALL_RECV, buf.as_mut_ptr() as u32, buf.len() as u32)
}

/// Read bytes from the USB-CDC RX buffer into `buf`. Blocks if no data
/// is available; the kernel's USBCTRL_IRQ wakes the caller when the host
/// next sends a packet. Returns the number of bytes written into `buf`.
/// `0xFFFFFFFF` indicates a validation error.
pub fn sys_usb_read(buf: &mut [u8]) -> u32 {
    syscall2(SYSCALL_USB_READ, buf.as_mut_ptr() as u32, buf.len() as u32)
}

/// Write bytes to the USB-CDC TX buffer. Non-blocking: returns the
/// number of bytes accepted (may be less than `buf.len()` if the TX
/// buffer is full). Caller is expected to retry. `0xFFFFFFFF` indicates
/// a validation error.
pub fn sys_usb_write(buf: &[u8]) -> u32 {
    syscall2(SYSCALL_USB_WRITE, buf.as_ptr() as u32, buf.len() as u32)
}
