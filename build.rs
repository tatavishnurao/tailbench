//! Build script: records the version of the rustc that compiles tailbench, so
//! the run manifest reports the compiler actually used rather than whatever
//! `rustc` happens to be on PATH at run time.

use std::process::Command;

fn main() {
    // Cargo sets RUSTC to the compiler it is building with.
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "UNKNOWN".to_owned());
    println!("cargo:rustc-env=TAILBENCH_RUSTC_VERSION={version}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTC");
}
