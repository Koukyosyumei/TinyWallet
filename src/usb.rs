//! USB-CDC: kernel-owned USB device that exposes a virtual serial port to
//! the host. Bytes from the host are buffered into a kernel RX ring;
//! user tasks pull via SYSCALL_USB_READ.
//!
//! The IRQ handler MUST drain `serial.read` on every poll(), even when
//! no task is waiting. If we don't drain, the OUT endpoint stays full,
//! the USB peripheral keeps the IRQ permanently asserted, and every user
//! task starves. Diagnosed empirically in Phase 3B.

use core::mem::MaybeUninit;

use cortex_m::peripheral::{NVIC, SCB};
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

use crate::task;

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

        NVIC::unmask(hal_pac::Interrupt::USBCTRL_IRQ);
    }
}

/// Push bytes into the kernel RX ring. Drops the tail of `src` if the
/// ring is full — for this PoC we accept loss when no consumer is
/// keeping up; a more conservative kernel would NAK the host instead.
unsafe fn rx_ring_push(src: &[u8]) {
    unsafe {
        let space = RX_RING_SIZE - RX_LEN;
        let n = src.len().min(space);
        for i in 0..n {
            let pos = (RX_HEAD + RX_LEN + i) % RX_RING_SIZE;
            RX_RING[pos] = src[i];
        }
        RX_LEN += n;
    }
}

/// Pop up to `dst_max` bytes from the RX ring into the user buffer at
/// `dst_ptr`. Returns the number copied. Caller is responsible for
/// having validated (dst_ptr, dst_max) against the calling task's
/// regions — this helper writes through privileged kernel access.
pub unsafe fn rx_ring_pop(dst_ptr: u32, dst_max: u32) -> u32 {
    unsafe {
        let n = RX_LEN.min(dst_max as usize);
        let dst = dst_ptr as *mut u8;
        for i in 0..n {
            let pos = (RX_HEAD + i) % RX_RING_SIZE;
            dst.add(i).write_volatile(RX_RING[pos]);
        }
        RX_HEAD = (RX_HEAD + n) % RX_RING_SIZE;
        RX_LEN -= n;
        n as u32
    }
}

#[interrupt]
fn USBCTRL_IRQ() {
    // SAFETY: this IRQ shares NVIC priority with SVCall (both default 0),
    // so neither preempts the other; PendSV is lowest priority and never
    // touches USB state. Single-writer to STATE statics in the runtime
    // sense.
    unsafe {
        let serial_slot = &raw mut STATE.serial;
        let device_slot = &raw mut STATE.device;
        let serial = (*serial_slot).assume_init_mut();
        let device = (*device_slot).assume_init_mut();

        if !device.poll(&mut [serial]) {
            return;
        }

        // Always drain serial.read into the kernel RX ring — must drain
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
                if RX_LEN > 0 {
                    let n = rx_ring_pop(out_ptr, max_len);
                    (*t).state = task::TaskState::Ready;
                    task::poke_blocked_task_r0(idx, n);
                    SCB::set_pendsv();
                }
                break;
            }
        }
    }
}
