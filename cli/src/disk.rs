//! `bottle-cli disk`: makes the disks an OCI image's bottles boot from, one per stage, and prints
//! their paths, bottom first.
//!
//!     bottle-cli disk DIR ARCHIVE [BASE_ARCHIVE]...
//!
//! ARCHIVE is an OCI layout archive of one image; each BASE_ARCHIVE one of an image it's built on,
//! where a stage ends. What's in DIR already is used. mke2fs is $BOTTLE_MKE2FS, or the one beside
//! this program.

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bottle_disk::{Archive, make_disks, splits};

use crate::FAILED;

pub const USAGE: &str = "disk DIR ARCHIVE [BASE_ARCHIVE]...";

/// Runs `bottle-cli disk` with `args`, those after `disk`.
pub fn main(mut args: impl Iterator<Item = String>) -> ExitCode {
    let (Some(dir), Some(archive)) = (args.next(), args.next()) else {
        return crate::usage_error("disk needs DIR and ARCHIVE");
    };
    let bases: Vec<String> = args.collect();
    match run(Path::new(&dir), Path::new(&archive), &bases) {
        Ok(disks) => {
            for disk in disks {
                println!("{}", disk.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bottle-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

fn run(dir: &Path, archive: &Path, bases: &[String]) -> io::Result<Vec<PathBuf>> {
    let mke2fs = mke2fs()?;
    let archive = Archive::open(archive)?;
    let image = archive.image()?;
    let bases = bases
        .iter()
        .map(|base| Archive::open(Path::new(base))?.image())
        .collect::<io::Result<Vec<_>>>()?;
    let splits = splits(&image, &bases)?;
    std::fs::create_dir_all(dir)?;
    make_disks(&archive, &image, &splits, dir, &mke2fs)
}

/// The mke2fs bottle runs: $BOTTLE_MKE2FS, else the one beside this program.
pub fn mke2fs() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("BOTTLE_MKE2FS") {
        return Ok(path.into());
    }
    let beside = env::current_exe()?.with_file_name("mke2fs");
    if beside.exists() {
        return Ok(beside);
    }
    Err(io::Error::other(format!(
        "no mke2fs: not at {}, and no $BOTTLE_MKE2FS",
        beside.display()
    )))
}
