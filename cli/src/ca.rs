//! `bottle-cli ca`: a machine's caching CA (see bottle_agent_client::ca), for bottle to install in
//! the bottle's trust store.
//!
//!     bottle-cli ca certificate NAME [HOST]...
//!     bottle-cli ca forget NAME
//!
//! `certificate` prints NAME's CA certificate for HOSTs, made if it isn't, as PEM; nothing, for no
//! hosts. NAME is a machine's, or `bottle-builds` for builds.

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
        Some("certificate") => ca::certificate(&name, &args.collect::<Vec<_>>()).map(|pem| {
            if let Some(pem) = pem {
                print!("{pem}");
            }
        }),
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
