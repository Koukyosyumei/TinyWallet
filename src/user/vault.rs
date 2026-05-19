//! Task 3 — `vault`. Holds the ed25519 keypair in MPU-isolated RAM
//! (TASK3_RAM, only this task can read it directly). Receives commands
//! from host_io via IPC, signs / responds, never exposes the secret.
//!
//! Commands (first byte of message):
//!   `'p'`              → respond with hex-encoded public key (64 chars + `\n`)
//!   `'s' <payload>`    → respond with hex-encoded ed25519 signature over
//!                        `<payload>` (128 chars + `\n`)
//!   anything else      → respond `?\n`

use crate::syscall::{sys_print, sys_recv, sys_send};

pub extern "C" fn vault_main() -> ! {
    sys_print(b"hello vault");

    // Hardcoded seed for this PoC. The architectural property is that
    // this value lives in vault's RAM, which only the vault's MPU view
    // covers — host_io and other tasks cannot read it.
    let seed_bytes: [u8; 32] = [
        0x9E, 0x55, 0xD1, 0x3C, 0xA1, 0xF3, 0x40, 0x7B, 0xE2, 0x88, 0x91, 0x6F, 0x44, 0x0C, 0xDD,
        0x21, 0x67, 0x05, 0x9A, 0xB7, 0x3D, 0xCE, 0xE8, 0x14, 0x52, 0xFB, 0xA4, 0x9D, 0x10, 0x77,
        0xCC, 0x82,
    ];
    let keypair = salty::Keypair::from(&seed_bytes);

    let mut req: core::mem::MaybeUninit<[u8; 256]> = core::mem::MaybeUninit::uninit();
    let req_ptr = req.as_mut_ptr() as *mut u8;
    let mut resp: core::mem::MaybeUninit<[u8; 256]> = core::mem::MaybeUninit::uninit();
    let resp_ptr = resp.as_mut_ptr() as *mut u8;

    loop {
        let recv_packed = unsafe {
            let slice = core::slice::from_raw_parts_mut(req_ptr, 256);
            sys_recv(slice)
        };
        if recv_packed == u32::MAX {
            continue;
        }
        let sender = ((recv_packed >> 16) & 0xFF) as u32;
        let req_len = (recv_packed & 0xFFFF) as usize;
        if req_len == 0 {
            continue;
        }

        let cmd = unsafe { req_ptr.read() };
        let resp_len = match cmd {
            b'p' => {
                let pk = keypair.public.to_bytes();
                let n = unsafe {
                    let dst = core::slice::from_raw_parts_mut(resp_ptr, 256);
                    hex_encode(&pk, dst)
                };
                unsafe { resp_ptr.add(n).write(b'\n') };
                n + 1
            }
            b's' => {
                let payload = unsafe {
                    core::slice::from_raw_parts(req_ptr.add(1), req_len.saturating_sub(1))
                };
                let sig = keypair.sign(payload);
                let sig_bytes = sig.to_bytes();
                let n = unsafe {
                    let dst = core::slice::from_raw_parts_mut(resp_ptr, 256);
                    hex_encode(&sig_bytes, dst)
                };
                unsafe { resp_ptr.add(n).write(b'\n') };
                n + 1
            }
            _ => {
                unsafe {
                    resp_ptr.add(0).write(b'?');
                    resp_ptr.add(1).write(b'\n');
                }
                2
            }
        };

        let _ = unsafe {
            let slice = core::slice::from_raw_parts(resp_ptr, resp_len);
            sys_send(sender, slice)
        };
    }
}

/// Lower-case hex-encode `src` into `dst`. Returns the number of bytes
/// written. Caller must size `dst` to at least `2 * src.len()`.
fn hex_encode(src: &[u8], dst: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let n = src.len().min(dst.len() / 2);
    for i in 0..n {
        dst[2 * i] = HEX[(src[i] >> 4) as usize];
        dst[2 * i + 1] = HEX[(src[i] & 0x0F) as usize];
    }
    n * 2
}
