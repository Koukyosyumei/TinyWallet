//! Copy `memory.x` and `boot2.x` into the output directory and tell the linker
//! to use them. `boot2.x` is INSERTed before `.text` so the second-stage
//! bootloader lands in the BOOT2 region declared in memory.x.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();

    File::create(out.join("boot2.x"))
        .unwrap()
        .write_all(include_bytes!("boot2.x"))
        .unwrap();

    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=boot2.x");

    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
    println!("cargo:rustc-link-arg-bins=-Tboot2.x");
}
