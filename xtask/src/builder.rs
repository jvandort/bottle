//! Linux's distribution, built on a Mac: in a Linux VM of this Mac's bottle-cli.

use std::path::Path;

use crate::Result;

pub fn dist(target: &str, _out: &Path) -> Result<()> {
    Err(format!("{target}'s dist on a Mac: not yet"))
}
