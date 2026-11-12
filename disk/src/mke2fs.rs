//! Empty disks, made with mke2fs (see dist/mke2fs.sh): a machine's own, and each new stage's, which
//! a VM then writes. bottle runs mke2fs, never links it: e2fsprogs' library is GPL-2 in parts.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::other;

const BLOCK: u64 = 4096;

/// What mke2fs is given in place of its own mke2fs.conf, the same wherever it runs (a machine's
/// /etc/mke2fs.conf, if it has one, says otherwise): no room kept for root or for growing, and no
/// journal but the one asked for. mke2fs looks up a type for the filesystem's size too (floppy,
/// small, big, huge): they're all the same.
const CONFIG: &str = "\
[defaults]
	base_features = sparse_super,large_file,filetype,dir_index,ext_attr
	blocksize = 4096
	inode_size = 256
	inode_ratio = 16384
	reserved_ratio = 0
	enable_periodic_fsck = 0

[fs_types]
	ext4 = {
		features = extent,huge_file,flex_bg,metadata_csum,64bit,dir_nlink,extra_isize
	}
	floppy = {
		inode_ratio = 16384
	}
	small = {
		inode_ratio = 16384
	}
	big = {
		inode_ratio = 16384
	}
	huge = {
		inode_ratio = 16384
	}
";

/// Makes an empty filesystem `bytes` big at `path`, if nothing's there: a bottle's own disk, or a
/// build's, written in the VM, so with a journal, a small one. Sparse: what isn't used of it takes
/// no room.
pub fn empty(mke2fs: &Path, path: &Path, bytes: u64) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let file = path.file_name().unwrap_or_default().to_string_lossy();
    let partial = path.with_file_name(format!(".{file}.{}", std::process::id()));
    let mut config = partial.as_os_str().to_owned();
    config.push(".mke2fs.conf");
    let config = PathBuf::from(config);
    fs::write(&config, CONFIG)?;
    let made = Command::new(mke2fs)
        .env("MKE2FS_CONFIG", &config)
        .args([
            "-q",
            "-t",
            "ext4",
            "-b",
            "4096",
            "-O",
            "has_journal",
            "-J",
            "size=16",
        ])
        .args(["-E", "root_owner=0:0,lazy_itable_init=1"])
        .arg(&partial)
        .arg((bytes / BLOCK).to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let _ = fs::remove_file(&config);
    let made = made.map_err(|e| other(format!("{}: {e}", mke2fs.display())))?;
    if !made.status.success() {
        let _ = fs::remove_file(&partial);
        return Err(other(format!(
            "mke2fs failed ({}): {}",
            made.status,
            String::from_utf8_lossy(&made.stderr).trim()
        )));
    }
    fs::rename(&partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_mke2fs_not_there_is_an_error_and_leaves_nothing() {
        let dir = std::env::temp_dir().join(format!("bottle-disk-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.ext4");
        let err = empty(Path::new("/nonexistent/mke2fs"), &path, 1 << 30).unwrap_err();
        assert!(err.to_string().contains("/nonexistent/mke2fs"), "{err}");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }
}
