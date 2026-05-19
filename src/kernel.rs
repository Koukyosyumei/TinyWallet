//! Kernel-side syscall handlers — invoked from the SVCall exception
//! dispatcher in `main.rs`.
//!
//! Every handler that takes a user pointer must validate it against the
//! caller's MPU regions (`task::validate_buf`) before dereferencing —
//! that's the kernel's only defense against the confused-deputy pattern
//! where a user task tricks the kernel into reading kernel memory on its
//! behalf.

use cortex_m::peripheral::SCB;

use crate::syscall::IPC_MAX_BYTES;
use crate::{gpio, task, usb};

pub fn syscall_print(ptr: u32, len: u32) -> u32 {
    if len > 256 {
        return u32::MAX;
    }
    if !task::validate_buf(task::current(), ptr, len, task::PERM_R) {
        defmt::warn!(
            "kernel: rejected SYSCALL_PRINT — buf not in caller's regions (ptr=0x{:08x} len={})",
            ptr,
            len,
        );
        return u32::MAX;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    match core::str::from_utf8(bytes) {
        Ok(s) => defmt::info!("[user→kernel print] {}", s),
        Err(_) => defmt::info!("[user→kernel print] (non-utf8, {} bytes)", bytes.len()),
    }
    0
}

pub fn syscall_led(which: u32, value: u32) -> u32 {
    let pin = match which {
        0 => gpio::LED_RED,
        1 => gpio::LED_GREEN,
        2 => gpio::LED_BLUE,
        _ => return u32::MAX,
    };
    gpio::set(pin, value != 0);
    0
}

/// Pack (sender, len) for return from RECV: low 16 bits = len,
/// bits 16..23 = sender index.
fn encode_recv_result(sender: u8, len: u32) -> u32 {
    ((sender as u32) << 16) | (len & 0xFFFF)
}

pub fn syscall_send(target: u32, msg_ptr: u32, msg_len: u32) -> u32 {
    let target = target as usize;
    if target >= task::N_TASKS || msg_len > IPC_MAX_BYTES {
        return u32::MAX;
    }
    let cur = unsafe { task::CURRENT_TASK };
    if target == cur {
        return u32::MAX; // self-send is meaningless and would deadlock
    }
    let cur_task = unsafe { &*(&raw const task::TASKS[cur]) };
    if !task::validate_buf(cur_task, msg_ptr, msg_len, task::PERM_R) {
        return u32::MAX;
    }

    // Snapshot the target's state once. If it's BlockedOnRecv, we deliver
    // the message right now and wake it; otherwise we block the caller
    // until the target eventually calls RECV.
    let target_state = unsafe { (*(&raw const task::TASKS[target])).state };
    match target_state {
        task::TaskState::BlockedOnRecv { out_ptr, max_len } => {
            let copy_len = msg_len.min(max_len);
            // SAFETY: kernel is privileged (PRIVDEFENA=1), both buffers
            // were validated against their owning task's regions when the
            // tasks issued their respective syscalls.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    msg_ptr as *const u8,
                    out_ptr as *mut u8,
                    copy_len as usize,
                );
                let target_t = &raw mut task::TASKS[target];
                (*target_t).state = task::TaskState::Ready;
                // Deliver the recv result through the target's saved frame
                // so its sys_recv call returns with the right value.
                task::poke_blocked_task_r0(target, encode_recv_result(cur as u8, copy_len));
            }
            // Caller stays Ready; pend a switch so the freshly-Ready
            // target gets a chance to run.
            SCB::set_pendsv();
            0
        }
        _ => {
            // Block caller. Its r4-r11 + exception frame will be saved by
            // the PendSV that fires next; when the target eventually
            // RECVs, that handler will copy from msg_ptr and wake us.
            unsafe {
                let cur_t = &raw mut task::TASKS[cur];
                (*cur_t).state = task::TaskState::BlockedOnSend {
                    target: target as u8,
                    msg_ptr,
                    msg_len,
                };
            }
            SCB::set_pendsv();
            // The 0 we return here will be overwritten by the rendezvous
            // handler before we ever resume — see poke_blocked_task_r0.
            0
        }
    }
}

pub fn syscall_recv(out_ptr: u32, max_len: u32) -> u32 {
    if max_len > IPC_MAX_BYTES {
        return u32::MAX;
    }
    let cur = unsafe { task::CURRENT_TASK };
    let cur_task = unsafe { &*(&raw const task::TASKS[cur]) };
    if !task::validate_buf(cur_task, out_ptr, max_len, task::PERM_W) {
        return u32::MAX;
    }

    // Look for a task already blocked-on-send-to-us. If found, deliver
    // its message immediately and wake it.
    for sender in 0..task::N_TASKS {
        if sender == cur {
            continue;
        }
        let sender_state = unsafe { (*(&raw const task::TASKS[sender])).state };
        if let task::TaskState::BlockedOnSend {
            target,
            msg_ptr,
            msg_len,
        } = sender_state
        {
            if target as usize == cur {
                let copy_len = msg_len.min(max_len);
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        msg_ptr as *const u8,
                        out_ptr as *mut u8,
                        copy_len as usize,
                    );
                    let sender_t = &raw mut task::TASKS[sender];
                    (*sender_t).state = task::TaskState::Ready;
                    // Sender's sys_send will see 0 (success) when it resumes.
                    task::poke_blocked_task_r0(sender, 0);
                }
                return encode_recv_result(sender as u8, copy_len);
            }
        }
    }

    // No pending sender — block caller.
    unsafe {
        let cur_t = &raw mut task::TASKS[cur];
        (*cur_t).state = task::TaskState::BlockedOnRecv { out_ptr, max_len };
    }
    SCB::set_pendsv();
    // Will be overwritten by the rendezvous-on-send handler.
    0
}

/// Drain bytes from the kernel RX ring into the user task's buffer. If
/// the ring is empty, park the caller; USBCTRL_IRQ will deliver to it
/// when the next host packet arrives.
pub fn syscall_usb_read(out_ptr: u32, max_len: u32) -> u32 {
    if max_len == 0 || max_len > 256 {
        return u32::MAX;
    }
    let cur = unsafe { task::CURRENT_TASK };
    let cur_task = unsafe { &*(&raw const task::TASKS[cur]) };
    if !task::validate_buf(cur_task, out_ptr, max_len, task::PERM_W) {
        return u32::MAX;
    }
    unsafe {
        if usb::RX_LEN > 0 {
            return usb::rx_ring_pop(out_ptr, max_len);
        }
    }
    unsafe {
        let cur_t = &raw mut task::TASKS[cur];
        (*cur_t).state = task::TaskState::BlockedOnUsbRead { out_ptr, max_len };
    }
    SCB::set_pendsv();
    0
}

pub fn syscall_usb_write(in_ptr: u32, in_len: u32) -> u32 {
    if in_len == 0 || in_len > 256 {
        return u32::MAX;
    }
    let cur = unsafe { task::CURRENT_TASK };
    let cur_task = unsafe { &*(&raw const task::TASKS[cur]) };
    if !task::validate_buf(cur_task, in_ptr, in_len, task::PERM_R) {
        return u32::MAX;
    }
    unsafe {
        let serial_slot = &raw mut usb::STATE.serial;
        let serial = (*serial_slot).assume_init_mut();
        let user_buf = core::slice::from_raw_parts(in_ptr as *const u8, in_len as usize);
        match serial.write(user_buf) {
            Ok(n) => n as u32,
            Err(_) => 0, // TX buffer full — caller retries
        }
    }
}
