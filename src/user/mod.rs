//! User tasks. Each runs in unprivileged thread mode on its own PSP,
//! with an MPU view that grants only read+execute on flash and read+write
//! on its own 8 KiB RAM block. All other memory — kernel RAM, peripherals,
//! sibling tasks' RAM — faults on access.

pub mod client;
pub mod host_io;
pub mod server;
pub mod vault;
