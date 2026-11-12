//! Embeds the agent bottle's VMs run, if $BOTTLE_EMBED_AGENT names one (a static Linux build of
//! bottle-agent, as `cargo xtask dist` makes): boot then serves it from bottle-cli's own bytes.
//! Without it, boot reads the agent from a file (see boot.rs).

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    // On Linux with glibc, libkrun (linked in) loads libkrunfw, the kernel, beside this program,
    // where the distribution has it.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu")
    {
        println!("cargo::rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
    }
    // Static on Linux, with musl: libkrun's libloading asks for libdl, which is musl's libc; Rust's
    // own musl has no libdl.a to say so, so an empty one is linked.
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("musl") {
        let out = Path::new(&env::var_os("OUT_DIR").unwrap()).to_path_buf();
        fs::write(out.join("libdl.a"), b"!<arch>\n").unwrap();
        println!("cargo::rustc-link-search=native={}", out.display());
    }
    println!("cargo::rustc-check-cfg=cfg(embedded_agent)");
    println!("cargo::rerun-if-env-changed=BOTTLE_EMBED_AGENT");
    let Some(agent) = env::var_os("BOTTLE_EMBED_AGENT") else {
        return;
    };
    let agent = Path::new(&agent);
    println!("cargo::rerun-if-changed={}", agent.display());
    let out = Path::new(&env::var_os("OUT_DIR").unwrap()).join("agent");
    if let Err(e) = fs::copy(agent, &out) {
        panic!("BOTTLE_EMBED_AGENT={}: {e}", agent.display());
    }
    println!("cargo::rustc-cfg=embedded_agent");
}
