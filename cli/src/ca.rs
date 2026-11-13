//! `bottle-cli ca`: a machine's caching CA (see bottle_agent_client::ca), for bottle to install in
//! the bottle's trust store.
//!
//!     bottle-cli ca certificate NAME [HOST]...
//!     bottle-cli ca forget NAME
//!
//! `certificate` prints NAME's CA certificate for HOSTs, made if it isn't, as PEM; nothing, for no
//! hosts. NAME is a machine's, or `bottle-builds` for builds. Without HOSTs, a machine made with a
//! network policy's are its policy's: those it caches over https and routes by name.

use std::process::ExitCode;

use bottle_agent_client::ca;

use crate::FAILED;

pub const USAGE: &str = "ca certificate NAME [HOST]... | ca forget NAME";

pub fn main(mut args: impl Iterator<Item = String>) -> ExitCode {
    let command = args.next();
    let Some(name) = args.next() else {
        return crate::usage_error(USAGE);
    };
    let result = match command.as_deref() {
        Some("certificate") => {
            let mut hosts: Vec<String> = args.collect();
            (|| {
                if hosts.is_empty() {
                    hosts = crate::machine::terminated_hosts(&name)?.unwrap_or_default();
                }
                if let Some(pem) = ca::certificate(&name, &hosts)? {
                    print!("{pem}");
                }
                Ok(())
            })()
        }
        Some("forget") if args.next().is_none() => ca::forget(&name),
        _ => return crate::usage_error(USAGE),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bottle-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}
