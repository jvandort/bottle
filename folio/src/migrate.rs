//! The format of what folio keeps. A home says which it's in (`format`, a number, in it), so folio
//! reads only the format it writes: a home in an older one is converted before anything reads it,
//! once, under a lock, and stamped (when a format changes, its converter goes here); one in a newer
//! one is a newer folio's, and refused. A home with no stamp yet is a new one, and is stamped
//! FORMAT: every home from before formats has been converted to format 1.

use std::fs;
use std::path::Path;

use crate::store;

/// The format folio writes.
pub const FORMAT: u32 = 1;
/// Where a home says its format.
const STAMP: &str = "format";

/// The format `home` says it's in; None if it says none yet.
fn stamped(home: &Path) -> Option<u32> {
    fs::read_to_string(home.join(STAMP))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// folio's home, in this format. A home that isn't there yet has nothing in it to read.
pub fn ensure() -> Result<(), String> {
    let home = crate::home();
    if !home.is_dir() || stamped(&home) == Some(FORMAT) {
        return Ok(());
    }
    let lock = fs::File::create(home.join(".format.lock"))
        .map_err(|e| format!("{}: {e}", home.display()))?;
    // Released as it's closed.
    lock.lock()
        .map_err(|e| format!("{}: {e}", home.display()))?;
    match stamped(&home) {
        Some(FORMAT) => Ok(()), // another command stamped it meanwhile
        None => store::write_bytes(&home.join(STAMP), format!("{FORMAT}\n").as_bytes()),
        Some(newer) if newer > FORMAT => Err(format!(
            "{} is in format {newer}, a newer folio's than this one ({FORMAT}): use that folio",
            home.display()
        )),
        Some(older) => Err(format!(
            "{} is in format {older}, which this folio has no converter from (it writes {FORMAT})",
            home.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir;

    #[test]
    fn a_new_home_is_stamped_and_one_from_another_format_refused() {
        let env = tempdir::isolated();
        let home = env.home().to_path_buf();
        ensure().unwrap();
        assert_eq!(
            fs::read_to_string(home.join(STAMP)).unwrap(),
            format!("{FORMAT}\n")
        );
        ensure().unwrap();
        fs::write(home.join(STAMP), format!("{}\n", FORMAT + 1)).unwrap();
        assert!(ensure().unwrap_err().contains("a newer folio's"));
        fs::write(home.join(STAMP), "0\n").unwrap();
        assert!(ensure().unwrap_err().contains("no converter from"));
    }
}
