//! Running mke2fs on a stage's tar: how big a filesystem it needs, and the filesystem itself.

use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::thread;

use crate::other;
use crate::tree::{Kind, Stage, split};

const BLOCK: u64 = 4096;
const INODE: u64 = 256;

/// What mke2fs is given in place of its own mke2fs.conf, the same wherever it runs. No journal,
/// no room kept for root or for growing: the disks are written once, and only read after. mke2fs
/// looks up a type for the filesystem's size too (floppy, small, big, huge): they're all the same.
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

/// A filesystem big enough for a stage: in blocks, and inodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub blocks: u64,
    pub inodes: u64,
}

/// How big a filesystem `stage` needs, with `whiteouts` more for the stages below. Generous: the
/// disk is a sparse file, and what's not used of it takes no room.
pub fn size(stage: &Stage, whiteouts: usize) -> Size {
    let mut inodes = whiteouts as u64;
    let mut blocks = 0;
    let mut names = whiteouts as u64 * 32;
    for (path, node) in &stage.nodes {
        names += 12 + split(path).map_or(0, |(_, name)| name.len() as u64);
        let Some(meta) = &node.meta else {
            inodes += 1;
            blocks += u64::from(matches!(node.kind, Kind::Dir { .. }));
            continue;
        };
        if matches!(node.kind, Kind::Hardlink { .. }) {
            continue;
        }
        inodes += 1;
        blocks += match meta.kind {
            b'5' => 1,
            // Past 59 bytes, a symlink's target takes a block of its own.
            b'2' => u64::from(meta.link.len() > 59),
            // The data, and an extent block per 128 MiB or so.
            b'0' => meta.size.div_ceil(BLOCK) + meta.size / (128 << 20),
            _ => 0,
        };
        // Xattrs that don't fit in the inode go in a block.
        let xattrs: usize = meta.pax.iter().map(|(k, v)| k.len() + v.len()).sum();
        blocks += u64::from(xattrs > 64);
    }
    blocks += names.div_ceil(BLOCK);
    inodes += 16;
    let tables = (inodes * INODE).div_ceil(BLOCK);
    let used = blocks + tables;
    Size {
        blocks: used + used / 4 + 4096,
        inodes: inodes + inodes / 8,
    }
}

/// Makes the filesystem `size` big at `path` (which mustn't be there), of the tar `write` writes,
/// with `mke2fs`. `seed` makes its UUID and directory hash seed, so a disk made of the same is the
/// same, byte for byte.
pub fn make(
    mke2fs: &Path,
    path: &Path,
    size: Size,
    seed: &[u8; 16],
    write: impl FnOnce(&mut BufWriter<ChildStdin>) -> io::Result<()>,
) -> io::Result<()> {
    let mut config = path.as_os_str().to_owned();
    config.push(".mke2fs.conf");
    let config = PathBuf::from(config);
    fs::write(&config, CONFIG)?;
    let uuid = uuid(seed);
    let child = Command::new(mke2fs)
        .env("MKE2FS_CONFIG", &config)
        .env("E2FSPROGS_FAKE_TIME", "1")
        .args(["-q", "-t", "ext4", "-b", "4096", "-N"])
        .arg(size.inodes.to_string())
        .args(["-U", &uuid, "-E"])
        .arg(format!(
            "root_owner=0:0,hash_seed={uuid},lazy_itable_init=1"
        ))
        .args(["-d", "-"])
        .arg(path)
        .arg(size.blocks.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| other(format!("{}: {e}", mke2fs.display())));
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            let _ = fs::remove_file(&config);
            return Err(e);
        }
    };
    let mut stderr = child.stderr.take().unwrap();
    let said = thread::spawn(move || {
        let mut said = Vec::new();
        let _ = stderr.read_to_end(&mut said);
        said
    });
    let mut stdin = BufWriter::with_capacity(1 << 20, child.stdin.take().unwrap());
    let written = write(&mut stdin).and_then(|()| stdin.flush());
    drop(stdin);
    let status = child.wait();
    let said = said.join().unwrap();
    let _ = fs::remove_file(&config);
    let status = status?;
    match written {
        // mke2fs stopped reading: why is its to say.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe && !status.success() => {}
        // What went wrong writing the tar comes first: mke2fs failing on what it got is only
        // what followed.
        Err(e) => return Err(e),
        Ok(()) => {}
    }
    if !status.success() {
        return Err(other(format!(
            "mke2fs failed ({status}): {}",
            String::from_utf8_lossy(&said).trim()
        )));
    }
    Ok(())
}

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

fn uuid(seed: &[u8; 16]) -> String {
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uuid_is_its_seed_in_hex() {
        let seed: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(uuid(&seed), "00010203-0405-0607-0809-0a0b0c0d0e0f");
    }

    #[test]
    fn writing_to_nothing_is_an_error_not_a_hang() {
        let dir = std::env::temp_dir().join(format!("bottle-disk-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.ext4");
        let size = Size {
            blocks: 1024,
            inodes: 64,
        };
        let err = make(
            Path::new("/nonexistent/mke2fs"),
            &path,
            size,
            &[0; 16],
            |_| Ok(()),
        );
        assert!(err.unwrap_err().to_string().contains("/nonexistent/mke2fs"));
        assert!(!dir.join("x.ext4.mke2fs.conf").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
