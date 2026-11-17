//! Which of bottle-cli's distributions a runner gets, and what one has (its manifest).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::{env, fs, io};

use super::other;

/// sha256sum's lines (a sha256, two spaces, a name), by name.
pub(super) fn sums<'a>(lines: impl Iterator<Item = &'a str>) -> BTreeMap<String, String> {
    lines
        .filter_map(|line| line.split_once("  "))
        .map(|(sum, name)| (name.to_string(), sum.to_string()))
        .collect()
}

/// The bottle-cli folio runs here, its symlinks followed: $BOTTLE_CLI, else bottle-cli on PATH.
pub(super) fn bottle_cli() -> io::Result<PathBuf> {
    let named = match env::var_os("BOTTLE_CLI").filter(|c| !c.is_empty()) {
        Some(cli) => PathBuf::from(cli),
        None => env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("bottle-cli"))
            .find(|cli| cli.is_file())
            .ok_or_else(|| other("no bottle-cli: install it, or set BOTTLE_CLI"))?,
    };
    fs::canonicalize(&named).map_err(|e| other(format!("{}: {e}", named.display())))
}

/// Where the distribution for `target` is: $FOLIO_RUNNER_DISTS/dist-TARGET, if that's set; else
/// bottle-cli's own, if it's built for `target`; else dist-TARGET beside it.
pub(super) fn dist_for(target: &str) -> io::Result<PathBuf> {
    if let Some(dists) = env::var_os("FOLIO_RUNNER_DISTS") {
        return Ok(PathBuf::from(dists).join(format!("dist-{target}")));
    }
    let cli = bottle_cli()?;
    let own = cli
        .parent()
        .ok_or_else(|| other("no directory beside bottle-cli"))?;
    let ours = format!("{}-unknown-linux-musl", env::consts::ARCH);
    if cfg!(target_os = "linux") && target == ours {
        return Ok(own.to_path_buf());
    }
    Ok(own
        .parent()
        .ok_or_else(|| other("no directory beside bottle-cli"))?
        .join(format!("dist-{target}")))
}
