//! `bottle-agent unpack DIR [--oci]`: extracts the tar on stdin into DIR, as root, everything as
//! the tar says: owners, modes, times, xattrs, hard links, devices and fifos. The host runs it as
//! a command, `/proc/self/exe unpack ...` (the agent's own binary, whatever root it's in), to copy
//! files in for a build, or to apply an image's layer: with --oci, the layer's whiteouts are
//! applied as it goes, what they name removed, and an overlayfs above makes the whiteouts.
//!
//! The tar crate reads; the writing is here, its unpacking turning devices into files. Paths
//! resolve as they would for anything else in the bottle: through its symlinks, in its root.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};

const OPAQUE: &[u8] = b".wh..wh..opq";
const WHITEOUT: &[u8] = b".wh.";

pub fn main(args: &[String]) -> Result<(), String> {
    let (dir, oci) = match args {
        [dir] => (dir, false),
        [dir, flag] if flag == "--oci" => (dir, true),
        _ => return Err("usage: bottle-agent unpack DIR [--oci]".into()),
    };
    unpack(io::stdin().lock(), Path::new(dir), oci).map_err(|e| e.to_string())
}

/// Extracts the tar `tar` into `dir`, applying its whiteouts if `oci`.
pub fn unpack(tar: impl Read, dir: &Path, oci: bool) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)?;
    let mut archive = tar::Archive::new(tar);
    // What this tar made: its whiteouts are of what was there before it, never of its own.
    let mut made = HashSet::new();
    // Directories' times are set last: what's made in them changes them.
    let mut dir_times = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type().as_byte();
        if matches!(kind, b'g' | b'x' | b'L' | b'K') {
            continue;
        }
        let rel = normalize(&entry.path_bytes())?;
        let what =
            |e: io::Error| io::Error::other(format!("{}: {e}", String::from_utf8_lossy(&rel)));
        let (parent, name) = split(&rel);
        if oci && name == OPAQUE {
            let parent_path = dir.join(path(parent));
            if let Ok(children) = fs::read_dir(&parent_path) {
                for child in children {
                    let child = child?;
                    let child_rel = join(parent, child.file_name().as_bytes());
                    if !made.contains(&child_rel) {
                        remove(&child.path()).map_err(what)?;
                    }
                }
            }
            continue;
        }
        if oci && let Some(hidden) = name.strip_prefix(WHITEOUT) {
            let target = join(parent, hidden);
            if !made.contains(&target) {
                remove(&dir.join(path(&target))).map_err(what)?;
            }
            continue;
        }
        let dst = dir.join(path(&rel));
        if !parent.is_empty() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o755)
                .create(dir.join(path(parent)))
                .map_err(what)?;
        }
        write(&mut entry, kind, dir, &dst, &mut dir_times).map_err(what)?;
        made.insert(rel);
    }
    for (dst, mtime) in dir_times.into_iter().rev() {
        set_time(&dst, mtime)?;
    }
    Ok(())
}

fn write<R: Read>(
    entry: &mut tar::Entry<R>,
    kind: u8,
    root: &Path,
    dst: &Path,
    dir_times: &mut Vec<(PathBuf, i64)>,
) -> io::Result<()> {
    let header = entry.header().clone();
    let old = header.as_old();
    let mode = blank_is_zero(header.mode().map(u64::from), &old.mode)? as u32 & 0o7777;
    let uid = blank_is_zero(header.uid(), &old.uid)? as libc::uid_t;
    let gid = blank_is_zero(header.gid(), &old.gid)? as libc::gid_t;
    let mtime = blank_is_zero(header.mtime(), &old.mtime)? as i64;
    let mut xattrs = Vec::new();
    if let Some(records) = entry.pax_extensions()? {
        for record in records {
            let record = record?;
            if let Some(name) = record.key_bytes().strip_prefix(b"SCHILY.xattr.") {
                xattrs.push((name.to_vec(), record.value_bytes().to_vec()));
            }
        }
    }
    let existing = fs::symlink_metadata(dst).ok();
    if kind == b'5' {
        match existing {
            Some(m) if m.is_dir() => {}
            Some(_) => {
                fs::remove_file(dst)?;
                fs::create_dir(dst)?;
            }
            None => fs::create_dir(dst)?,
        }
    } else {
        if existing.is_some() {
            remove(dst)?;
        }
        match kind {
            b'0' | b'\0' | b'7' | b'S' => {
                let mut file = fs::File::create(dst)?;
                io::copy(entry, &mut file)?;
            }
            b'1' => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| io::Error::other("a hard link to nothing"))?;
                fs::hard_link(root.join(path(&normalize(&target)?)), dst)?;
                // A hard link is its target: the target's owner, mode and times are its own.
                return Ok(());
            }
            b'2' => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| io::Error::other("a symlink to nothing"))?;
                symlink(std::ffi::OsStr::from_bytes(&target), dst)?;
            }
            b'3' | b'4' | b'6' => {
                let file_type = match kind {
                    b'3' => libc::S_IFCHR,
                    b'4' => libc::S_IFBLK,
                    _ => libc::S_IFIFO,
                };
                let dev = match kind {
                    b'6' => 0,
                    _ => libc::makedev(
                        header.device_major()?.unwrap_or(0),
                        header.device_minor()?.unwrap_or(0),
                    ),
                };
                let c = cstr(dst)?;
                // SAFETY: mknod with a C string path.
                if unsafe { libc::mknod(c.as_ptr(), file_type | mode, dev) } < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            other => {
                return Err(io::Error::other(format!(
                    "an entry of type {:?}",
                    other as char
                )));
            }
        }
    }
    let c = cstr(dst)?;
    // SAFETY: lchown, then (not for a symlink) chmod, with a C string path: owner first, since
    // changing it clears setuid and setgid.
    unsafe {
        if libc::lchown(c.as_ptr(), uid, gid) < 0 {
            return Err(io::Error::last_os_error());
        }
        if kind != b'2' && libc::chmod(c.as_ptr(), mode as libc::mode_t) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    for (name, value) in xattrs {
        let name = CString::new(name).map_err(|_| io::Error::other("an xattr name with a NUL"))?;
        // SAFETY: lsetxattr with C strings and a value of the length given.
        if unsafe {
            libc::lsetxattr(
                c.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    if kind == b'5' {
        dir_times.push((dst.to_path_buf(), mtime));
    } else {
        set_time(dst, mtime)?;
    }
    Ok(())
}

/// A number from a header, 0 if its field is blank, as GNU tar and libarchive read it.
fn blank_is_zero(n: io::Result<u64>, field: &[u8]) -> io::Result<u64> {
    match n {
        Err(_) if field.iter().all(|&b| b == 0 || b == b' ') => Ok(0),
        n => n,
    }
}

/// Sets the access and modification times of `dst`, not following a symlink.
fn set_time(dst: &Path, mtime: i64) -> io::Result<()> {
    let c = cstr(dst)?;
    let t = libc::timespec {
        tv_sec: mtime as libc::time_t,
        tv_nsec: 0,
    };
    // SAFETY: utimensat with a C string path and two timespecs.
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            [t, t].as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Removes what's at `p`, a directory with everything in it; nothing there is fine.
fn remove(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// A tar's path, relative: no leading `/`, no `.` or empty components; `..` refused.
fn normalize(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len());
    for component in raw.split(|&b| b == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                return Err(io::Error::other(format!(
                    "{}: a path with ..",
                    String::from_utf8_lossy(raw)
                )));
            }
            c => {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(c);
            }
        }
    }
    Ok(out)
}

fn split(rel: &[u8]) -> (&[u8], &[u8]) {
    match rel.iter().rposition(|&b| b == b'/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => (&[], rel),
    }
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        name.to_vec()
    } else {
        [dir, b"/", name].concat()
    }
}

fn path(rel: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(rel))
}

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::other("a path with a NUL"))
}
