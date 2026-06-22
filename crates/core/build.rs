//! Compile the bundled sync agent at build time.
//!
//! The agent (`crates/agent`) is a std-only, dependency-free binary. We compile it here, for the
//! same target rustle itself is being built for, and drop it in `OUT_DIR` so the library can
//! `include_bytes!` it. The cli/mcp thus *bundle* the agent and **deploy the prebuilt binary** to
//! a remote — nothing is ever compiled on the remote. (Client/remote arch+libc parity is already
//! assumed by the tool, the same reason artifact copy-back works.)

use std::path::Path;
use std::process::Command;

fn main() {
    let main_rs = "../agent/src/main.rs";
    let proto_rs = "../agent/src/proto.rs";
    println!("cargo:rerun-if-changed={main_rs}");
    println!("cargo:rerun-if-changed={proto_rs}");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let out = Path::new(&out_dir).join("rustle-agent");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    // Build for the *target* triple (matters when rustle is itself cross-compiled); a bare
    // `rustc main.rs` resolves `mod proto;` to the sibling file — no cargo, no registry, no deps.
    let target = std::env::var("TARGET").expect("TARGET not set");

    let status = Command::new(&rustc)
        .args(["--edition", "2021", "-O", "-C", "strip=symbols", "--target", &target])
        .arg(main_rs)
        .arg("-o")
        .arg(&out)
        .status()
        .expect("failed to spawn rustc to build the bundled agent");
    assert!(
        status.success(),
        "failed to compile the bundled rustle agent ({main_rs})"
    );
}
