//! tiny-wallet — toy hardware wallet for the XIAO RP2040.
//!
//! Phase 1 PoC: minimal MPU-enforced microkernel. One privileged kernel,
//! one unprivileged user task, one syscall. The point of this phase is to
//! prove the *isolation mechanism* end-to-end before any wallet logic is
//! written: a buggy or compromised user task cannot reach kernel RAM or
//! peripherals — the MPU traps it.
//!
//! Boot path (after the rp2040 mask ROM has loaded boot2 from flash):
//!   reset → cortex-m-rt → main()  [privileged thread mode, MSP]
//!     → configure MPU
//!     → enable SysTick (kernel heartbeat in handler mode)
//!     → set PSP to top of TASK0 RAM
//!     → write CONTROL = (SPSEL=1, nPRIV=1) → drop privilege
//!     → bx into the user task
//!
//! After the drop, the only path back into kernel code is via SVC (syscalls)
//! or via an exception (SysTick, HardFault). The user task cannot directly
//! call kernel functions because its MPU view excludes everything it wasn't
//! explicitly granted.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use core::mem::MaybeUninit;

use cortex_m::peripheral::{Peripherals, syst::SystClkSource};
use cortex_m_rt::{ExceptionFrame, entry, exception};
use rp2040_hal::{
    Watchdog,
    clocks::init_clocks_and_plls,
    pac::{self as hal_pac, interrupt},
    usb::UsbBus,
};
use usb_device::{
    bus::UsbBusAllocator,
    device::{StringDescriptors, UsbDevice, UsbDeviceBuilder, UsbVidPid},
};
use usbd_serial::SerialPort;
use {defmt_rtt as _, panic_probe as _};

// =============================================================================
// Stage 2 bootloader
// =============================================================================
//
// The RP2040 mask ROM copies the first 256 bytes of flash into RAM and
// executes them. That blob — provided here by the `rp2040-boot2` crate for
// the W25Q080 QSPI flash on the XIAO RP2040 — sets up XIP so the rest of the
// firmware can execute directly from flash.
#[unsafe(link_section = ".boot2")]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;

// =============================================================================
// User task RAM
// =============================================================================
//
// 8 KiB block, 8 KiB-aligned, so the MPU can cover it as a single region.
// MPU regions on Armv6-M must be a power-of-two size and naturally aligned.
// Phase 2 will introduce a task table; for now, hardcode one task.
#[repr(C, align(8192))]
struct TaskRam(#[allow(dead_code)] [u8; 8192]);

static mut TASK0_RAM: TaskRam = TaskRam([0; 8192]);
static mut TASK1_RAM: TaskRam = TaskRam([0; 8192]);
static mut TASK2_RAM: TaskRam = TaskRam([0; 8192]);
static mut TASK3_RAM: TaskRam = TaskRam([0; 8192]);

// =============================================================================
// Syscall ABI
// =============================================================================
//
// User side: place the syscall number in r0 and args in r1..r3, then `svc #0`.
// Kernel SVCall handler reads the saved frame from PSP, dispatches, and
// writes the return value back into the saved r0 slot so the caller sees it.

const SYSCALL_PRINT: u32 = 1;
const SYSCALL_LED: u32 = 2;
const SYSCALL_YIELD: u32 = 3;
const SYSCALL_SEND: u32 = 4;
const SYSCALL_RECV: u32 = 5;
const SYSCALL_USB_READ: u32 = 6;
const SYSCALL_USB_WRITE: u32 = 7;

/// Maximum bytes we'll copy in a single SEND/RECV. Bounds the validation
/// surface. Sized for the wallet's IPC: command bytes + payload (sign
/// inputs) or response bytes (hex-encoded signature = 128 chars + slack).
const IPC_MAX_BYTES: u32 = 256;

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

fn sys_print(buf: &[u8]) {
    syscall2(SYSCALL_PRINT, buf.as_ptr() as u32, buf.len() as u32);
}

/// Ask the kernel to drive an on-board LED. `which`: 0=red, 1=green, 2=blue.
fn sys_set_led(which: u32, on: bool) {
    syscall2(SYSCALL_LED, which, on as u32);
}

/// Cooperative yield: kernel pends PendSV, which (after this SVC returns)
/// switches to the next ready task. Returns when this task is rescheduled.
#[allow(dead_code)]
fn sys_yield() {
    syscall2(SYSCALL_YIELD, 0, 0);
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

/// Send a message to another task. Blocks the caller until the target is
/// in `recv` to pick it up (synchronous rendezvous). Returns 0 on success.
fn sys_send(target: u32, msg: &[u8]) -> u32 {
    syscall3(SYSCALL_SEND, target, msg.as_ptr() as u32, msg.len() as u32)
}

/// Receive a message into `buf`. Blocks until some task sends to us.
/// Returns a packed u32: low 16 bits = message length, bits 16..23 =
/// sender task ID. `0xFFFFFFFF` indicates an error (e.g. invalid buffer).
fn sys_recv(buf: &mut [u8]) -> u32 {
    syscall2(SYSCALL_RECV, buf.as_mut_ptr() as u32, buf.len() as u32)
}

/// Read bytes from the USB-CDC RX buffer into `buf`. Blocks if no data
/// is available; the kernel's USBCTRL_IRQ wakes the caller when the host
/// next sends a packet. Returns the number of bytes written into `buf`.
/// `0xFFFFFFFF` indicates a validation error.
fn sys_usb_read(buf: &mut [u8]) -> u32 {
    syscall2(SYSCALL_USB_READ, buf.as_mut_ptr() as u32, buf.len() as u32)
}

/// Write bytes to the USB-CDC TX buffer. Non-blocking: returns the
/// number of bytes accepted (may be less than `buf.len()` if the TX
/// buffer is full). Caller is expected to retry. `0xFFFFFFFF` indicates
/// a validation error.
fn sys_usb_write(buf: &[u8]) -> u32 {
    syscall2(SYSCALL_USB_WRITE, buf.as_ptr() as u32, buf.len() as u32)
}

// =============================================================================
// User task
// =============================================================================
//
// Runs in unprivileged thread mode on PSP. MPU-allowed memory:
//   - read+execute  flash             (so the task can run its own code)
//   - read+write    its own 8 KiB RAM (TASK0_RAM)
// Anything else — kernel RAM, peripherals — should fault.
/// Task 0 — the **client**. Toggles the blue LED on its own loop, then
/// every 4th iteration sends "ping" to task 1 and waits for the reply.
extern "C" fn task0_main() -> ! {
    sys_print(b"hello task0 (client)");
    let mut counter: u32 = 0;
    let mut reply_buf = [0u8; 16];
    loop {
        sys_set_led(2, counter & 1 == 0); // blue toggle
        counter = counter.wrapping_add(1);
        // Busy-wait sized for ~125 MHz core (was 400_000 at ~6.5 MHz).
        for _ in 0..8_000_000 {
            unsafe { core::arch::asm!("nop") };
        }
        if counter.is_multiple_of(4) {
            sys_send(1, b"ping");
            let _ = sys_recv(&mut reply_buf);
        }
    }
}

/// Task 1 — the **server**. Sits in `recv` waiting for requests; on each
/// one, toggles the green LED and replies "pong". The fact that green
/// blinks at all proves IPC delivery is happening — task 0 doesn't yield
/// between pings, so without IPC task 1 would never run.
extern "C" fn task1_main() -> ! {
    sys_print(b"hello task1 (server)");
    let mut counter: u32 = 0;
    let mut req_buf = [0u8; 16];
    loop {
        let _ = sys_recv(&mut req_buf); // blocks until task 0 pings
        sys_set_led(1, counter & 1 == 0); // green toggle
        counter = counter.wrapping_add(1);
        // Busy-wait sized for ~125 MHz core (was 200_000 at ~6.5 MHz).
        for _ in 0..4_000_000 {
            unsafe { core::arch::asm!("nop") };
        }
        sys_send(0, b"pong");
    }
}

/// Task 2 — `host_io`. Forwards bytes between USB-CDC and the vault task:
/// USB bytes go to vault via IPC, vault's response goes back out USB.
/// Doesn't interpret the protocol — the vault is the policy enforcer.
///
/// MaybeUninit avoids `__aeabi_memclr8` on the buffer (see commit
/// ed95e928 — AEABI memclr8 mis-executes on non-8-aligned stack arrays).
extern "C" fn host_io_main() -> ! {
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

/// Task 3 — `vault`. Holds the ed25519 keypair in MPU-isolated RAM
/// (TASK3_RAM, only this task can read it directly). Receives commands
/// from host_io via IPC, signs / responds, never exposes the secret.
///
/// Commands (first byte of message):
///   'p'              → respond with hex-encoded public key (64 chars + '\n')
///   's' <payload>    → respond with hex-encoded ed25519 signature over
///                      `<payload>` (128 chars + '\n')
///   anything else    → respond "?\n"
/// Task 3 — `vault`. Holds the ed25519 keypair in MPU-isolated RAM
/// (TASK3_RAM, only this task can read it directly). Receives commands
/// from host_io via IPC, signs / responds, never exposes the secret.
///
/// Commands (first byte of message):
///   'p'              → respond with hex-encoded public key (64 chars + '\n')
///   's' <payload>    → respond with hex-encoded ed25519 signature over
///                      `<payload>` (128 chars + '\n')
///   anything else    → respond "?\n"
extern "C" fn vault_main() -> ! {
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

// =============================================================================
// Kernel: USB-CDC (host-facing serial port)
// =============================================================================
//
// Phase 3A: the USB driver lives entirely in privileged kernel code. The
// host enumerates this device as a CDC-ACM (virtual serial port) at
// /dev/ttyACMx (Linux/macOS) or COMx (Windows). The IRQ handler echoes
// any bytes received back to the host.
//
// Phase 3B will replace the in-IRQ echo with a syscall path so a
// user-space `host_io` task can read/write through the kernel.
mod usb_io {
    use super::*;

    /// 12 MHz crystal on the XIAO RP2040.
    const XTAL_FREQ_HZ: u32 = 12_000_000;

    pub struct UsbState {
        bus: MaybeUninit<UsbBusAllocator<UsbBus>>,
        pub serial: MaybeUninit<SerialPort<'static, UsbBus>>,
        pub device: MaybeUninit<UsbDevice<'static, UsbBus>>,
    }

    /// Backing storage for the USB stack. Initialized once at boot, then
    /// shared between USBCTRL_IRQ and the SVCall handlers (both at NVIC
    /// priority 0 — neither preempts the other, so plain access is safe).
    pub static mut STATE: UsbState = UsbState {
        bus: MaybeUninit::uninit(),
        serial: MaybeUninit::uninit(),
        device: MaybeUninit::uninit(),
    };

    /// Kernel RX ring buffer. The USB IRQ drains the OUT endpoint into here
    /// on every poll(); SYSCALL_USB_READ pulls from here. This decoupling
    /// is *load-bearing*: if the IRQ doesn't drain serial.read whenever
    /// data is available, the OUT endpoint stays "full" and the USB
    /// peripheral keeps the IRQ permanently asserted, starving every user
    /// task. Diagnosed empirically in Phase 3B.
    pub const RX_RING_SIZE: usize = 256;
    pub static mut RX_RING: [u8; RX_RING_SIZE] = [0; RX_RING_SIZE];
    pub static mut RX_HEAD: usize = 0; // next byte to consume
    pub static mut RX_LEN: usize = 0; // valid bytes from RX_HEAD

    /// Initialize system clocks (XOSC + PLL_SYS @ 125 MHz + PLL_USB @ 48 MHz)
    /// and bring up the USB peripheral as a CDC-ACM device. Must be called
    /// exactly once, before NVIC unmasks USBCTRL_IRQ.
    pub fn init() {
        let mut pac = hal_pac::Peripherals::take().unwrap();
        let mut watchdog = Watchdog::new(pac.WATCHDOG);

        // System clock setup. After this returns, the core is running at
        // 125 MHz and PLL_USB is providing the 48 MHz USB clock.
        let clocks = init_clocks_and_plls(
            XTAL_FREQ_HZ,
            pac.XOSC,
            pac.CLOCKS,
            pac.PLL_SYS,
            pac.PLL_USB,
            &mut pac.RESETS,
            &mut watchdog,
        )
        .ok()
        .expect("clock setup failed");

        let usb_bus = UsbBusAllocator::new(UsbBus::new(
            pac.USBCTRL_REGS,
            pac.USBCTRL_DPRAM,
            clocks.usb_clock,
            true, // force VBUS detect
            &mut pac.RESETS,
        ));

        // SAFETY: called exactly once at boot before USBCTRL_IRQ is
        // unmasked, so there's no concurrent reader of STATE yet.
        unsafe {
            let bus_slot = &raw mut STATE.bus;
            (*bus_slot).write(usb_bus);

            // 'static reference to the bus allocator: cast through the raw
            // pointer so the borrow checker accepts the 'static lifetime
            // (the data really is in static storage and lives forever).
            let bus_ref: &'static UsbBusAllocator<UsbBus> =
                &*((&raw const STATE.bus) as *const UsbBusAllocator<UsbBus>);

            let serial_slot = &raw mut STATE.serial;
            (*serial_slot).write(SerialPort::new(bus_ref));

            // VID 0x16c0 / PID 0x27dd is V-USB's shared test pair —
            // appropriate for a hobbyist PoC. Replace before shipping.
            let device = UsbDeviceBuilder::new(bus_ref, UsbVidPid(0x16c0, 0x27dd))
                .strings(&[StringDescriptors::default()
                    .manufacturer("tiny-wallet")
                    .product("tiny-wallet PoC")
                    .serial_number("0001")])
                .unwrap()
                .device_class(usbd_serial::USB_CLASS_CDC)
                .build();
            let device_slot = &raw mut STATE.device;
            (*device_slot).write(device);

            cortex_m::peripheral::NVIC::unmask(hal_pac::Interrupt::USBCTRL_IRQ);
        }
    }
}

/// Push bytes into the kernel RX ring. Drops the tail of `src` if the
/// ring is full — for this PoC we accept loss when no consumer is
/// keeping up; a more conservative kernel would NAK the host instead.
unsafe fn rx_ring_push(src: &[u8]) {
    unsafe {
        let space = usb_io::RX_RING_SIZE - usb_io::RX_LEN;
        let n = src.len().min(space);
        for i in 0..n {
            let pos = (usb_io::RX_HEAD + usb_io::RX_LEN + i) % usb_io::RX_RING_SIZE;
            usb_io::RX_RING[pos] = src[i];
        }
        usb_io::RX_LEN += n;
    }
}

/// Pop up to `dst_max` bytes from the RX ring into the user buffer at
/// `dst_ptr`. Returns the number copied. Caller is responsible for
/// having validated (dst_ptr, dst_max) against the calling task's
/// regions — this helper writes through privileged kernel access.
unsafe fn rx_ring_pop(dst_ptr: u32, dst_max: u32) -> u32 {
    unsafe {
        let n = usb_io::RX_LEN.min(dst_max as usize);
        let dst = dst_ptr as *mut u8;
        for i in 0..n {
            let pos = (usb_io::RX_HEAD + i) % usb_io::RX_RING_SIZE;
            dst.add(i).write_volatile(usb_io::RX_RING[pos]);
        }
        usb_io::RX_HEAD = (usb_io::RX_HEAD + n) % usb_io::RX_RING_SIZE;
        usb_io::RX_LEN -= n;
        n as u32
    }
}

#[interrupt]
fn USBCTRL_IRQ() {
    // SAFETY: this IRQ shares NVIC priority with SVCall (both default 0),
    // so neither preempts the other; PendSV is lowest priority and never
    // touches USB state. Single-writer to usb_io statics in the runtime
    // sense.
    unsafe {
        let serial_slot = &raw mut usb_io::STATE.serial;
        let device_slot = &raw mut usb_io::STATE.device;
        let serial = (*serial_slot).assume_init_mut();
        let device = (*device_slot).assume_init_mut();

        if !device.poll(&mut [serial]) {
            return;
        }

        // ALWAYS drain serial.read into the kernel RX ring, even if no
        // task is waiting. If we don't drain, the OUT endpoint stays full,
        // the USB peripheral keeps the IRQ pending, and the handler re-
        // fires forever — no user task ever runs again.
        // Always drain into the kernel RX ring — must drain serial.read
        // every poll() or the OUT endpoint stays full and the IRQ pends
        // forever.
        let mut tmp = [0u8; 64];
        if let Ok(n) = serial.read(&mut tmp) {
            if n > 0 {
                rx_ring_push(&tmp[..n]);
            }
        }

        // If a task is parked waiting for USB bytes and we have buffered
        // data, deliver it now and wake the task.
        for idx in 0..task::N_TASKS {
            let t = &raw mut task::TASKS[idx];
            if let task::TaskState::BlockedOnUsbRead { out_ptr, max_len } = (*t).state {
                if usb_io::RX_LEN > 0 {
                    let n = rx_ring_pop(out_ptr, max_len);
                    (*t).state = task::TaskState::Ready;
                    poke_blocked_task_r0(idx, n);
                    cortex_m::peripheral::SCB::set_pendsv();
                }
                break;
            }
        }
    }
}

// =============================================================================
// Kernel: GPIO (XIAO RP2040 on-board LEDs)
// =============================================================================
//
// The XIAO RP2040 has three simple-GPIO LEDs (active LOW): R=GPIO17,
// G=GPIO16, B=GPIO25. The kernel owns all three; the user task can only
// affect them via SYSCALL_LED.
mod gpio {
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
}

// =============================================================================
// Kernel: tasks
// =============================================================================
//
// A task carries the same (base, size, perms) region list that's programmed
// into the MPU for it. The kernel uses this list to validate user-supplied
// pointers in syscalls — without it, a malicious task could pass a kernel
// pointer to SYSCALL_PRINT and have the kernel exfiltrate kernel memory on
// the task's behalf (the "confused deputy" pattern).
//
// Phase 2A: one hardcoded task. Phase 2B will introduce a real table.
mod task {
    pub const PERM_R: u8 = 1 << 0;
    pub const PERM_W: u8 = 1 << 1;
    pub const PERM_X: u8 = 1 << 2;
    pub const PERM_RW: u8 = PERM_R | PERM_W;
    pub const PERM_RX: u8 = PERM_R | PERM_X;

    #[derive(Clone, Copy)]
    pub struct TaskRegion {
        pub base: u32,
        pub size: u32,
        pub perms: u8,
    }

    /// Scheduling + IPC state. Phase 2D introduces blocking states; the
    /// scheduler skips them when picking the next task to run.
    #[derive(Clone, Copy)]
    pub enum TaskState {
        Ready,
        /// Task called RECV and is parked waiting for some sender to target
        /// it. `out_ptr` / `max_len` describe the buffer where the message
        /// should be written when delivery happens.
        BlockedOnRecv { out_ptr: u32, max_len: u32 },
        /// Task called SEND and is parked because the target wasn't in
        /// RECV. Stays here until the target's RECV picks up the message.
        BlockedOnSend {
            target: u8,
            msg_ptr: u32,
            msg_len: u32,
        },
        /// Task called USB_READ and the USB driver had no bytes available.
        /// Stays here until USBCTRL_IRQ delivers the next host packet.
        BlockedOnUsbRead { out_ptr: u32, max_len: u32 },
    }

    pub struct Task {
        /// Function pointer the kernel jumps to when first running this task.
        pub entry_pc: u32,
        /// Initial PSP value (top of the task's stack, 8-byte aligned).
        /// Used only by `bootstrap_user` for the first task; thereafter
        /// `saved_psp` carries the actual SP across context switches.
        pub initial_psp: u32,
        /// PSP value at the most recent suspension. For task 0, undefined
        /// until its first yield. For task 1+, init code seeds this to point
        /// at a fake exception frame so the first PendSV restore lands at
        /// `entry_pc` cleanly.
        pub saved_psp: u32,
        /// MPU regions granted to this task, mirrored for syscall pointer
        /// validation. Empty entries (size == 0) are skipped.
        pub regions: [TaskRegion; 4],
        /// Scheduler state; blocked tasks are skipped by `pick_next_ready`.
        pub state: TaskState,
    }

    pub const N_TASKS: usize = 4;

    const EMPTY: TaskRegion = TaskRegion { base: 0, size: 0, perms: 0 };
    const EMPTY_TASK: Task = Task {
        entry_pc: 0,
        initial_psp: 0,
        saved_psp: 0,
        regions: [EMPTY; 4],
        state: TaskState::Ready,
    };

    pub static mut TASKS: [Task; N_TASKS] = [EMPTY_TASK; N_TASKS];

    /// Index of the task that's currently executing in user mode. Updated
    /// by PendSV during a context switch. Read by syscall handlers (so
    /// `validate_buf` checks against the right task's regions) and by
    /// PendSV (to know which slot to save the outgoing PSP into).
    pub static mut CURRENT_TASK: usize = 0;

    /// Populate task 0. Bootstraps via `bootstrap_user` so it doesn't need
    /// a fake initial exception frame — kernel `main()` calls
    /// `bootstrap_user(task0)` directly which loads `initial_psp` and `bx`s
    /// to `entry_pc`. After task 0's first yield, `saved_psp` is set by
    /// PendSV and `initial_psp` is no longer read.
    ///
    /// IMPORTANT: explicitly initializes `state` and unused regions —
    /// TASKS may land in `.uninit`, which cortex-m-rt does NOT zero-init
    /// at boot. Leaving `state` garbage causes pattern-match jumps into
    /// the SCS region (HardFault with PC=0xExxxxxxx).
    pub fn init_task0(entry_pc: u32, ram_base: u32, ram_size: u32) {
        // SAFETY: called once at boot before any code can read TASKS.
        unsafe {
            let t = &raw mut TASKS[0];
            (*t).entry_pc = entry_pc;
            (*t).initial_psp = (ram_base + ram_size) & !7;
            (*t).saved_psp = 0; // unused until first yield
            (*t).state = TaskState::Ready;
            (*t).regions[0] = TaskRegion {
                base: ram_base,
                size: ram_size,
                perms: PERM_RW,
            };
            (*t).regions[1] = TaskRegion {
                base: 0x1000_0000,
                size: 16 * 1024 * 1024,
                perms: PERM_RX,
            };
            (*t).regions[2] = TaskRegion {
                base: 0,
                size: 0,
                perms: 0,
            };
            (*t).regions[3] = TaskRegion {
                base: 0,
                size: 0,
                perms: 0,
            };
        }
    }

    /// Populate any task whose first run goes through PendSV's restore
    /// path (i.e. anything other than task 0, which boots via
    /// `bootstrap_user`). We pre-seed the task's stack with a well-formed
    /// exception frame plus 32 bytes of zeros for r4-r11. When PendSV
    /// pops both, the hardware exception-return lands at `entry_pc` in
    /// unprivileged thread mode.
    ///
    /// Stack layout (grows down from `top`):
    ///
    /// ```text
    /// top         ──────────────  PSP after first run resumes
    ///   -4   xpsr  = 0x01000000
    ///   -8   pc    = entry_pc
    ///   -12  lr    = 0
    ///   -16  r12   = 0
    ///   -20  r3    = 0
    ///   -24  r2    = 0
    ///   -28  r1    = 0
    ///   -32  r0    = 0
    ///   -36..-64  r4-r11 = zeros   ── PendSV pops these first
    /// saved_psp  ──────────────  initial PSP value PendSV sees
    /// ```
    pub fn init_task_with_frame(slot: usize, entry_pc: u32, ram_base: u32, ram_size: u32) {
        // SAFETY: called once at boot, single-threaded. Writes into the
        // task's RAM via privileged kernel access (PRIVDEFENA=1 in MPU).
        // IMPORTANT: explicitly initializes `state` and unused regions —
        // see init_task0 for why (TASKS may live in .uninit).
        unsafe {
            let t = &raw mut TASKS[slot];
            (*t).entry_pc = entry_pc;
            let top = (ram_base + ram_size) & !7;
            (*t).initial_psp = top;

            // Fake exception frame at top-32 .. top.
            let frame_base = top - 32;
            (frame_base as *mut u32).add(0).write_volatile(0); // r0
            (frame_base as *mut u32).add(1).write_volatile(0); // r1
            (frame_base as *mut u32).add(2).write_volatile(0); // r2
            (frame_base as *mut u32).add(3).write_volatile(0); // r3
            (frame_base as *mut u32).add(4).write_volatile(0); // r12
            (frame_base as *mut u32).add(5).write_volatile(0); // lr
            (frame_base as *mut u32).add(6).write_volatile(entry_pc); // pc
            (frame_base as *mut u32).add(7).write_volatile(0x0100_0000); // xpsr (T bit)

            // Zeros for r4-r11 at top-64 .. top-32.
            let saved_psp = top - 64;
            for i in 0..8 {
                (saved_psp as *mut u32).add(i).write_volatile(0);
            }
            (*t).saved_psp = saved_psp;
            (*t).state = TaskState::Ready;

            (*t).regions[0] = TaskRegion {
                base: ram_base,
                size: ram_size,
                perms: PERM_RW,
            };
            (*t).regions[1] = TaskRegion {
                base: 0x1000_0000,
                size: 16 * 1024 * 1024,
                perms: PERM_RX,
            };
            (*t).regions[2] = TaskRegion {
                base: 0,
                size: 0,
                perms: 0,
            };
            (*t).regions[3] = TaskRegion {
                base: 0,
                size: 0,
                perms: 0,
            };
        }
    }

    /// Returns the task currently running in user mode (per CURRENT_TASK).
    /// Read from kernel context (syscall handlers, PendSV) — never from a
    /// user task directly.
    pub fn current() -> &'static Task {
        // SAFETY: CURRENT_TASK is written only by PendSV (one writer) and
        // read by handlers that run with PendSV preempted/disabled, so
        // there's no torn-read race in this PoC. TASKS is initialized once
        // at boot.
        unsafe {
            let idx = CURRENT_TASK;
            &*(&raw const TASKS[idx])
        }
    }

    /// Round-robin scheduler: starting after `current`, return the index
    /// of the next Ready task. Wraps around. If no task is Ready (true
    /// deadlock), panics.
    pub fn pick_next_ready(current: usize) -> usize {
        for i in 1..=N_TASKS {
            let idx = (current + i) % N_TASKS;
            if matches!(
                unsafe { (*(&raw const TASKS[idx])).state },
                TaskState::Ready
            ) {
                return idx;
            }
        }
        panic!("scheduler: no Ready tasks");
    }

    /// Returns true iff `[ptr, ptr+len)` is fully inside one of the task's
    /// regions and that region grants the access in `need`.
    pub fn validate_buf(task: &Task, ptr: u32, len: u32, need: u8) -> bool {
        let end = match ptr.checked_add(len) {
            Some(e) => e,
            None => return false,
        };
        for r in &task.regions {
            if r.size == 0 {
                continue;
            }
            let r_end = match r.base.checked_add(r.size) {
                Some(e) => e,
                None => continue,
            };
            if ptr >= r.base && end <= r_end && (r.perms & need) == need {
                return true;
            }
        }
        false
    }
}

// =============================================================================
// Kernel: MPU
// =============================================================================
mod mpu_cfg {
    use cortex_m::peripheral::MPU;

    // RASR field encodings — see Armv6-M ARM § B3.5.
    pub const RASR_ENABLE: u32 = 1 << 0;
    pub const RASR_XN: u32 = 1 << 28;
    // AP[2:0] in bits 26..24. Only the variants we actually use are defined;
    // add more (e.g. PRIV_RW_UNPRIV_NONE = 0b001) when a future region needs
    // them.
    pub const RASR_AP_PRIV_RW_UNPRIV_RW: u32 = 0b011 << 24;
    pub const RASR_AP_PRIV_RO_UNPRIV_RO: u32 = 0b110 << 24;
    // S=1, C=1, B=0, TEX=0 → "Outer & inner write-through, shareable".
    // Adequate default for SRAM and flash on RP2040.
    pub const RASR_MEM_NORMAL: u32 = (1 << 18) | (1 << 17);

    pub struct Region {
        pub number: u8,
        pub base: u32,
        pub size_bytes: u32,
        pub attrs: u32, // OR of RASR_* flags above (without ENABLE/SIZE)
    }

    fn size_field(size_bytes: u32) -> u32 {
        // SIZE encoding = log2(size_bytes) - 1, in bits 5..1.
        let log2 = 31 - size_bytes.leading_zeros();
        (log2 - 1) << 1
    }

    impl Region {
        fn rbar(&self) -> u32 {
            // VALID=1 (bit 4) + REGION (bits 3..0) lets us write rbar and
            // implicitly select the region number — saves a write to RNR.
            (self.base & !0x1F) | (1 << 4) | (self.number as u32 & 0xF)
        }

        fn rasr(&self) -> u32 {
            self.attrs | size_field(self.size_bytes) | RASR_ENABLE
        }
    }

    pub fn configure(regions: &[Region]) {
        // Use the static MPU pointer rather than a borrowed `&MPU` handle.
        // Callers like the PendSV context-switch path don't have access to
        // the cortex-m Peripherals struct (it was consumed at boot), and
        // there's only ever one MPU on the chip.
        // SAFETY: kernel-only, single-threaded with respect to MPU register
        // writes (PendSV is the only runtime caller; boot is single-threaded).
        let mpu = unsafe { &*MPU::PTR };
        unsafe {
            // Disable while we reconfigure.
            mpu.ctrl.write(0);

            for r in regions {
                mpu.rbar.write(r.rbar());
                mpu.rasr.write(r.rasr());
            }

            // ENABLE=1 (bit 0), PRIVDEFENA=1 (bit 2).
            // PRIVDEFENA=1 means privileged code falls back to the default
            // memory map for addresses not covered by any region — so the
            // kernel keeps full access without us having to enumerate every
            // peripheral. Unprivileged code only gets what regions grant.
            mpu.ctrl.write((1 << 0) | (1 << 2));

            cortex_m::asm::dsb();
            cortex_m::asm::isb();
        }
    }
}

use mpu_cfg::{
    RASR_AP_PRIV_RO_UNPRIV_RO, RASR_AP_PRIV_RW_UNPRIV_RW, RASR_MEM_NORMAL, RASR_XN, Region,
    configure,
};

// =============================================================================
// Kernel: SVC dispatch
// =============================================================================

fn syscall_print(ptr: u32, len: u32) -> u32 {
    if len > 256 {
        return u32::MAX;
    }
    // Validate the user-supplied buffer is inside the calling task's regions
    // before dereferencing — closes the confused-deputy hole. Without this
    // check, the user task could pass a kernel pointer and have us read
    // kernel memory on its behalf.
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

fn syscall_led(which: u32, value: u32) -> u32 {
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

/// Write the saved r0 slot of a *Blocked* task's exception frame so that
/// when the task is later resumed, its in-flight syscall sees `value` as
/// the return register.
///
/// The task must currently be Blocked (i.e. its full register state has
/// been saved by PendSV and hasn't been popped back). Layout under the
/// saved PSP: 32 bytes of r4-r11, then the 8-word HW exception frame.
/// r0 lives at offset 32 from saved_psp.
unsafe fn poke_blocked_task_r0(task_idx: usize, value: u32) {
    unsafe {
        let psp = (*(&raw const task::TASKS[task_idx])).saved_psp;
        let r0_slot = (psp + 32) as *mut u32;
        r0_slot.write_volatile(value);
    }
}

fn syscall_send(target: u32, msg_ptr: u32, msg_len: u32) -> u32 {
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
        task::TaskState::BlockedOnRecv {
            out_ptr,
            max_len,
        } => {
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
                poke_blocked_task_r0(target, encode_recv_result(cur as u8, copy_len));
            }
            // Caller stays Ready; pend a switch so the freshly-Ready
            // target gets a chance to run.
            cortex_m::peripheral::SCB::set_pendsv();
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
            cortex_m::peripheral::SCB::set_pendsv();
            // The 0 we return here will be overwritten by the rendezvous
            // handler before we ever resume — see poke_blocked_task_r0.
            0
        }
    }
}

fn syscall_recv(out_ptr: u32, max_len: u32) -> u32 {
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
                    poke_blocked_task_r0(sender, 0);
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
    cortex_m::peripheral::SCB::set_pendsv();
    // Will be overwritten by the rendezvous-on-send handler.
    0
}

/// Drain bytes from the kernel RX ring into the user task's buffer. If
/// the ring is empty, park the caller; USBCTRL_IRQ will deliver to it
/// when the next host packet arrives.
fn syscall_usb_read(out_ptr: u32, max_len: u32) -> u32 {
    if max_len == 0 || max_len > 256 {
        return u32::MAX;
    }
    let cur = unsafe { task::CURRENT_TASK };
    let cur_task = unsafe { &*(&raw const task::TASKS[cur]) };
    if !task::validate_buf(cur_task, out_ptr, max_len, task::PERM_W) {
        return u32::MAX;
    }
    unsafe {
        if usb_io::RX_LEN > 0 {
            return rx_ring_pop(out_ptr, max_len);
        }
    }
    unsafe {
        let cur_t = &raw mut task::TASKS[cur];
        (*cur_t).state = task::TaskState::BlockedOnUsbRead { out_ptr, max_len };
    }
    cortex_m::peripheral::SCB::set_pendsv();
    0
}

fn syscall_usb_write(in_ptr: u32, in_len: u32) -> u32 {
    if in_len == 0 || in_len > 256 {
        return u32::MAX;
    }
    let cur = unsafe { task::CURRENT_TASK };
    let cur_task = unsafe { &*(&raw const task::TASKS[cur]) };
    if !task::validate_buf(cur_task, in_ptr, in_len, task::PERM_R) {
        return u32::MAX;
    }
    unsafe {
        let serial_slot = &raw mut usb_io::STATE.serial;
        let serial = (*serial_slot).assume_init_mut();
        let user_buf = core::slice::from_raw_parts(in_ptr as *const u8, in_len as usize);
        match serial.write(user_buf) {
            Ok(n) => n as u32,
            Err(_) => 0, // TX buffer full — caller retries
        }
    }
}

#[exception]
unsafe fn SVCall() {
    // The exception was taken from unprivileged thread mode (the user task
    // is the only thing running on PSP), so r0..r3,r12,lr,pc,xpsr are saved
    // on PSP — not MSP, which is now the active stack for the handler.
    let psp: *mut u32;
    unsafe { core::arch::asm!("mrs {}, psp", out(reg) psp) };
    let frame = unsafe { core::slice::from_raw_parts_mut(psp, 8) };

    let num = frame[0];
    let a1 = frame[1];
    let a2 = frame[2];
    let a3 = frame[3];

    let ret = match num {
        SYSCALL_PRINT => syscall_print(a1, a2),
        SYSCALL_LED => syscall_led(a1, a2),
        SYSCALL_YIELD => {
            // Pend PendSV. Because PendSV is configured to lowest priority,
            // it fires *after* this SVC's exception return — at which point
            // PSP is back to the user task's pre-SVC stack, which is what
            // we want PendSV to save.
            cortex_m::peripheral::SCB::set_pendsv();
            0
        }
        SYSCALL_SEND => syscall_send(a1, a2, a3),
        SYSCALL_RECV => syscall_recv(a1, a2),
        SYSCALL_USB_READ => syscall_usb_read(a1, a2),
        SYSCALL_USB_WRITE => syscall_usb_write(a1, a2),
        _ => {
            defmt::warn!("kernel: unknown syscall {}", num);
            u32::MAX
        }
    };

    // Caller will see this in r0 after exception return.
    frame[0] = ret;
}

// =============================================================================
// Kernel: heartbeat (SysTick)
// =============================================================================
//
// SysTick fires roughly once a second off the boot ROSC (~6.5 MHz core clock
// before any clock tree configuration). Used here purely as a liveness signal
// so the operator can see the kernel is alive even if the user task is
// silent or stuck.
static KERNEL_TICKS: AtomicU32 = AtomicU32::new(0);

#[exception]
fn SysTick() {
    // Plain load+store on AtomicU32 — M0+ lacks CAS, but Relaxed load/store
    // compile to LDR/STR, which is sufficient since SysTick is the only
    // writer. Phase 2C: SysTick no longer drives any LED — task 1 owns
    // green via SYSCALL_LED. SysTick stays armed for timing reference and
    // as the future preemption hook (Phase 2D could pend PendSV here for
    // round-robin preemption).
    let n = KERNEL_TICKS.load(Ordering::Relaxed).wrapping_add(1);
    KERNEL_TICKS.store(n, Ordering::Relaxed);
    if n % 5 == 0 {
        defmt::info!("kernel: heartbeat tick={}", n);
    }
}

// =============================================================================
// Kernel: HardFault
// =============================================================================
//
// On Cortex-M0+ the MPU fault path escalates to HardFault — there's no
// MemManage exception or fault-status register, so we can't *prove* it was
// the MPU. In this PoC the only realistic source of a HardFault is an MPU
// violation from the user task, which is exactly what we want to demonstrate.
#[exception]
unsafe fn HardFault(ef: &ExceptionFrame) -> ! {
    let pc = ef.pc();
    defmt::error!(
        "HardFault! pc=0x{:08x} lr=0x{:08x} xpsr=0x{:08x}",
        pc, ef.lr(), ef.xpsr(),
    );
    gpio::set(gpio::LED_GREEN, false);
    gpio::set(gpio::LED_BLUE, false);
    gpio::set(gpio::LED_RED, true);

    // Diagnostic without a probe: blink BLUE N times based on the PC
    // region, with a long pause first so the burst is unmistakable.
    //   1 = PC in flash (0x10000000..)  → fault inside our code
    //   2 = PC in RAM   (0x20000000..)  → executing data (bad jump)
    //   3 = PC in SCS   (0xE0000000..)  → exception inside an exception
    //   4 = other                       → jumped to garbage (e.g. addr 0)
    let n = if (0x1000_0000..0x1020_0000).contains(&pc) {
        1
    } else if (0x2000_0000..0x2010_0000).contains(&pc) {
        2
    } else if pc >= 0xE000_0000 {
        3
    } else {
        4
    };

    fn long_delay() {
        for _ in 0..20_000_000 {
            cortex_m::asm::nop();
        }
    }
    fn short_delay() {
        for _ in 0..3_000_000 {
            cortex_m::asm::nop();
        }
    }
    long_delay(); // pause so the burst is visually distinct
    for _ in 0..n {
        gpio::set(gpio::LED_BLUE, true);
        short_delay();
        gpio::set(gpio::LED_BLUE, false);
        short_delay();
    }

    loop {
        cortex_m::asm::wfi();
    }
}

// =============================================================================
// Kernel: per-task MPU reprogramming (used on context switch)
// =============================================================================
//
// Each Task carries generic R/W/X permission bits in its TaskRegion list.
// On switch, those need to be translated to the RP2040's RASR encoding
// (AP field, XN bit) and written to MPU regions 0..N. PRIVDEFENA stays
// on so kernel-mode access is unaffected.

fn perms_to_rasr_attrs(perms: u8) -> u32 {
    let mut attrs = RASR_MEM_NORMAL;
    if perms & task::PERM_X == 0 {
        attrs |= RASR_XN;
    }
    let r = perms & task::PERM_R != 0;
    let w = perms & task::PERM_W != 0;
    attrs |= match (r, w) {
        (true, true) => RASR_AP_PRIV_RW_UNPRIV_RW,
        (true, false) => RASR_AP_PRIV_RO_UNPRIV_RO,
        _ => 0, // no-access (shouldn't happen for any granted region)
    };
    attrs
}

fn reconfigure_mpu_for_task(t: &task::Task) {
    // Both tasks currently use exactly two regions (RAM + flash). When
    // future tasks need more regions, generalize this.
    let regions = [
        Region {
            number: 0,
            base: t.regions[0].base,
            size_bytes: t.regions[0].size,
            attrs: perms_to_rasr_attrs(t.regions[0].perms),
        },
        Region {
            number: 1,
            base: t.regions[1].base,
            size_bytes: t.regions[1].size,
            attrs: perms_to_rasr_attrs(t.regions[1].perms),
        },
    ];
    configure(&regions);
}

// =============================================================================
// Kernel: PendSV — cooperative context switch
// =============================================================================
//
// Triggered indirectly by SYSCALL_YIELD (which sets PENDSVSET). Because
// PendSV is configured to the lowest priority, it runs after the
// triggering SVC has fully exited — so when PendSV fires, PSP holds the
// outgoing user task's true SP (no nested-exception frame on top).
//
// We can't write the body in plain Rust, because the cortex-m-rt
// `#[exception]` macro generates a wrapper with a function preamble that
// would clobber r4-r11 before we can save them. So PendSV is a fully
// naked function that:
//   1. Saves r4-r11 below the current PSP (Cortex-M0+ doesn't auto-save
//      callee-saved registers on exception entry — only r0-r3, r12, lr,
//      pc, xpsr).
//   2. Calls a Rust helper (`pendsv_switch`) with the new low-water PSP
//      as its argument; it returns the incoming task's saved PSP.
//   3. Restores r4-r11 from the incoming task's PSP.
//   4. Updates the PSP register and exception-returns. The hardware then
//      pops the standard 8-word frame from the new PSP, jumping into
//      the new task at its saved PC (or its entry point on first run).

#[unsafe(no_mangle)]
extern "C" fn pendsv_switch(outgoing_psp: u32) -> u32 {
    // SAFETY: runs only inside the PendSV handler, which is the unique
    // writer of CURRENT_TASK and the per-task saved_psp slots.
    unsafe {
        let cur = task::CURRENT_TASK;
        let tasks = &raw mut task::TASKS;
        (*tasks)[cur].saved_psp = outgoing_psp;

        // Round-robin pick that respects scheduling state — blocked tasks
        // (waiting on IPC) are skipped.
        let next = task::pick_next_ready(cur);
        task::CURRENT_TASK = next;

        let next_task = &(*tasks)[next];
        reconfigure_mpu_for_task(next_task);
        next_task.saved_psp
    }
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn PendSV() {
    core::arch::naked_asm!(
        // ---- save r4-r11 below the outgoing PSP ----
        "mrs   r0, psp",
        "subs  r0, #32",
        "stmia r0!, {{r4-r7}}",      // r4-r7 → [psp-32 .. psp-16]; r0 += 16
        "mov   r4, r8",
        "mov   r5, r9",
        "mov   r6, r10",
        "mov   r7, r11",
        "stmia r0!, {{r4-r7}}",      // r8-r11 → [psp-16 .. psp]; r0 += 16
        "subs  r0, #32",             // r0 = psp - 32 (the new low-water mark)

        // ---- call into Rust to update CURRENT_TASK + reprogram MPU ----
        "push  {{lr}}",              // preserve EXC_RETURN across the bl
        "bl    pendsv_switch",
        "pop   {{r1}}",
        "mov   lr, r1",
        // r0 = incoming task's saved_psp

        // ---- restore r4-r11 from the incoming PSP ----
        // CRITICAL: load the HIGH half first (saved r8-r11) using r4-r7 as
        // temps and move into r8-r11; THEN load the LOW half (saved r4-r7)
        // into r4-r7 directly. Doing it the other way around silently
        // corrupts r4-r7 because the second ldmia overwrites them with the
        // high-half values that should have gone to r8-r11.
        "adds  r0, #16",             // r0 = saved_psp + 16 (start of high half)
        "ldmia r0!, {{r4-r7}}",      // r4-r7 = saved r8,r9,r10,r11 (temps)
        "mov   r8, r4",
        "mov   r9, r5",
        "mov   r10, r6",
        "mov   r11, r7",
        "subs  r0, #32",             // r0 = saved_psp (start of low half)
        "ldmia r0!, {{r4-r7}}",      // r4-r7 = saved r4,r5,r6,r7 (final)
        "adds  r0, #16",             // r0 = saved_psp + 32 (start of exception frame)

        // ---- set PSP for the hardware exception return ----
        "msr   psp, r0",
        "bx    lr",                  // EXC_RETURN — HW pops the frame, runs new task
    );
}

// =============================================================================
// Kernel: privilege drop
// =============================================================================
//
// `bootstrap_user` is the one-way trapdoor from kernel to user mode: load
// the task's PSP, set CONTROL=(SPSEL=1, nPRIV=1), ISB, then bx into the
// task entry. After this point the only path back to kernel code is via
// SVC or an exception.
//
// In Phase 2C this same routine will be reused on the very first switch
// to *any* task, by feeding it different Task entries. The signature also
// makes it easier to extend later when each task needs MPU reprogramming.
unsafe fn bootstrap_user(task: &task::Task) -> ! {
    // All three values flow through compiler-picked input registers; the
    // asm body never names a hardcoded register. Earlier versions used
    // `movs r0, #3` for the CONTROL value, which silently broke whenever
    // the compiler happened to pick `r0` for `{entry}` — the `movs`
    // clobbered the entry address before the `bx`, jumping to address 3.
    // (`options(noreturn)` forbids declaring `r0` as a clobber explicitly,
    // so just don't touch r0 at all.)
    unsafe {
        core::arch::asm!(
            "msr psp, {psp}",        // PSP = top of user stack
            "msr control, {ctrl}",   // CONTROL = nPRIV(1) | SPSEL(1) = 3
            "isb",                   // commit privilege change before next insn
            "bx  {entry}",           // jump into user task (unprivileged, on PSP)
            psp   = in(reg) task.initial_psp,
            entry = in(reg) task.entry_pc,
            ctrl  = in(reg) 3u32,
            options(noreturn),
        );
    }
}

// =============================================================================
// Kernel entry
// =============================================================================
#[entry]
fn main() -> ! {
    defmt::info!("tiny-wallet kernel: boot");
    let mut cp = Peripherals::take().unwrap();

    // Explicitly initialize task scheduling state — TASKS and CURRENT_TASK
    // landed at the .bss / .uninit boundary; cortex-m-rt only zero-inits
    // .bss, so anything in .uninit holds whatever was in RAM at reset.
    // Garbage TaskState discriminants make Rust's pattern-match jump to
    // arbitrary addresses (e.g. SCS region → HardFault).
    unsafe {
        core::ptr::write_volatile(&raw mut task::CURRENT_TASK, 0);
    }

    // ---- 0. On-board LEDs (kernel-owned, used as the visible status display) ----
    gpio::init_leds();
    defmt::info!("kernel: LEDs initialized (R=17 G=16 B=25)");

    // ---- 0.5. System clocks + USB-CDC ----
    //
    // After this call: core runs at 125 MHz (was ~6.5 MHz on boot ROSC),
    // PLL_USB gives 48 MHz to the USB block, and the device enumerates as
    // a CDC-ACM serial port. Must run before SysTick is configured because
    // SysTick reload depends on the core clock.
    usb_io::init();
    defmt::info!("kernel: USB-CDC up (VID=0x16c0 PID=0x27dd)");

    // ---- 1. Populate the task table ----
    //
    // Each Task entry holds entry_pc / initial_psp / saved_psp / regions.
    // Task 0 boots via `bootstrap_user` (uses initial_psp). Task 1 boots
    // via PendSV's restore path on the first switch (uses saved_psp,
    // which init_task1 seeds with a fake exception frame).
    let task0_ram_base = &raw const TASK0_RAM as u32;
    let task1_ram_base = &raw const TASK1_RAM as u32;
    let task2_ram_base = &raw const TASK2_RAM as u32;
    let task3_ram_base = &raw const TASK3_RAM as u32;
    let task_ram_size: u32 = 8 * 1024;
    defmt::info!(
        "kernel: TASK0_RAM=0x{:08x} TASK1_RAM=0x{:08x} TASK2_RAM=0x{:08x} TASK3_RAM=0x{:08x} (each {} B)",
        task0_ram_base,
        task1_ram_base,
        task2_ram_base,
        task3_ram_base,
        task_ram_size,
    );
    task::init_task0(
        task0_main as *const () as u32,
        task0_ram_base,
        task_ram_size,
    );
    task::init_task_with_frame(
        1,
        task1_main as *const () as u32,
        task1_ram_base,
        task_ram_size,
    );
    task::init_task_with_frame(
        2,
        host_io_main as *const () as u32,
        task2_ram_base,
        task_ram_size,
    );
    task::init_task_with_frame(
        3,
        vault_main as *const () as u32,
        task3_ram_base,
        task_ram_size,
    );

    // ---- 2. MPU (start with task 0's view; PendSV reprograms on switch) ----
    reconfigure_mpu_for_task(task::current());
    defmt::info!("kernel: MPU configured for task 0");

    // ---- 3. PendSV must be lowest priority so it never preempts SVCall ----
    //
    // We pend PendSV from inside the SVCall handler (via SYSCALL_YIELD).
    // For PendSV to fire only *after* SVCall returns, its priority must be
    // strictly lower than SVCall's. Setting it to 0xFF gives the lowest
    // possible priority on M0+ (only the top 2 bits of priority are
    // implemented, so 0xFF == 0xC0 == priority 3).
    unsafe {
        cp.SCB
            .set_priority(cortex_m::peripheral::scb::SystemHandler::PendSV, 0xFF);
    }

    cp.SYST.set_clock_source(SystClkSource::Core);
    cp.SYST.set_reload(125_000_000 - 1);
    cp.SYST.clear_current();
    cp.SYST.enable_counter();
    cp.SYST.enable_interrupt();
    defmt::info!("kernel: SysTick armed");

    // ---- 5. Drop to unprivileged thread mode and jump to task 0 ----
    let t = task::current();
    defmt::info!(
        "kernel: dropping to task0: entry=0x{:08x} PSP=0x{:08x}",
        t.entry_pc,
        t.initial_psp,
    );
    unsafe { bootstrap_user(t) }
}
