//! Embeds the agent bottle's VMs run, if $BOTTLE_EMBED_AGENT names one (a static Linux build of
//! bottle-agent, as `cargo xtask dist` makes): boot then serves it from bottle-cli's own bytes.
//! Without it, boot reads the agent from a file (see boot.rs).

use std::env;
use std::fs;
use std::path::Path;

fn main() {
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
