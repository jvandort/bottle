//! Where folio keeps what it knows, and how it writes it so a reader never sees half.
//!
//! ```text
//! $FOLIO_HOME (default ~/.folio)/
//!   repos/broker/              where the broker kept its notes, before notes.rs (see broker.rs)
//!   <repo key>/                one per repo (see repos.rs)
//!     repo.json                the repo's path and name
//!     spec.json                the repo's bottle spec, canonical
//!     <thread>/
//!       current -> <slot>/     the thread's state as it is now
//!       <slot>/                thread.json, and later the snapshot archives
//! ```
//!
//! A thread's state is a set of files that must change together, so it's written as a whole new
//! slot, named by the time it was written, and `current` is moved onto it by renaming a symlink
//! over it, which is atomic. Files the change doesn't touch are hardlinked from the old slot. A
//! reader that follows `current` sees one whole slot or the other, never a mix; a crash leaves
//! `current` as it was. Any slot `current` doesn't point to is left over, and the next write
//! removes it, so a thread never has more than two.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::pyjson;

pub const CURRENT: &str = "current";

/// What goes in a slot under a name: its content, a file to move in (from the same filesystem: an
/// archive streamed in beside the slots, say), or nothing (left out of the new slot).
pub enum Content {
    Bytes(Vec<u8>),
    Move(PathBuf),
    Leave,
}

/// The JSON at `path`; None if there's no file. One that isn't JSON is an error.
pub fn read_json(path: &Path) -> Result<Option<Value>, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Replace `path` atomically with `data`, as JSON (its keys in its own order).
pub fn write_json(path: &Path, data: &impl Serialize) -> Result<(), String> {
    write_bytes(path, format!("{}\n", pyjson::dumps(data, false)).as_bytes())
}

/// Replace `path` atomically with `content`.
pub fn write_bytes(path: &Path, content: &[u8]) -> Result<(), String> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let staging = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    replace_with(path, &staging, content)
}

/// Replace `path` atomically with `data`, as JSON, its keys sorted: how bottle's records have always
/// been written.
pub fn write_json_sorted(path: &Path, data: &impl Serialize) -> Result<(), String> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let staging = path.with_file_name(format!(".{name}.tmp"));
    replace_with(path, &staging, format!("{}\n", pyjson::dumps(data, true)).as_bytes())
}

fn replace_with(path: &Path, staging: &Path, content: &[u8]) -> Result<(), String> {
    let failed = |e: io::Error| format!("{}: {e}", path.display());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(failed)?;
    }
    fs::write(staging, content).map_err(failed)?;
    fs::rename(staging, path).map_err(failed)
}

/// The slot `current` points at, or None if nothing has been written.
pub fn current_slot(directory: &Path) -> Option<PathBuf> {
    fs::read_link(directory.join(CURRENT))
        .ok()
        .map(|target| directory.join(target))
}

/// Write a new slot in `directory` with `files`, and every other file of the current slot; make it
/// current.
pub fn write_slot(directory: &Path, files: Vec<(&str, Content)>) -> Result<PathBuf, String> {
    let failed = |e: io::Error| format!("{}: {e}", directory.display());
    fs::create_dir_all(directory).map_err(failed)?;
    let old = current_slot(directory);
    remove_leftovers(directory, old.as_deref());
    let slot = new_slot(directory).map_err(failed)?;
    for (name, content) in &files {
        match content {
            Content::Bytes(bytes) => fs::write(slot.join(name), bytes).map_err(failed)?,
            Content::Move(from) => fs::rename(from, slot.join(name)).map_err(failed)?,
            Content::Leave => {}
        }
    }
    if let Some(old) = &old {
        for kept in fs::read_dir(old).map_err(failed)?.flatten() {
            let name = kept.file_name();
            if !files.iter().any(|(n, _)| *n == name.to_string_lossy()) {
                fs::hard_link(kept.path(), slot.join(&name)).map_err(failed)?;
            }
        }
    }
    let staging = directory.join(format!(".{CURRENT}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&staging);
    std::os::unix::fs::symlink(slot.file_name().unwrap(), &staging).map_err(failed)?;
    fs::rename(&staging, directory.join(CURRENT)).map_err(failed)?;
    if let Some(old) = old {
        let _ = fs::remove_dir_all(old);
    }
    Ok(slot)
}

/// A slot's name: when it was written, `YYYYmmddTHHMMSS.mmmZ`.
pub fn slot_name(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs() as libc::time_t;
    // SAFETY: gmtime_r writes the struct it's given and reads the time it's given.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&seconds, &mut tm);
        tm
    };
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}.{:03}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        since.subsec_millis()
    )
}

fn is_slot_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 20
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'.'
        && b[16..19].iter().all(u8::is_ascii_digit)
        && b[19] == b'Z'
}

fn new_slot(directory: &Path) -> io::Result<PathBuf> {
    loop {
        let path = directory.join(slot_name(SystemTime::now()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(e) => return Err(e),
        }
    }
}

/// Slots `current` doesn't point to: what a crash mid-write leaves behind. Only slots, by their
/// names: anything else beside them -- a thread's notes, the dot-named work in progress of
/// staging() -- is left alone.
fn remove_leftovers(directory: &Path, keep: Option<&Path>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && Some(path.as_path()) != keep && is_slot_name(&entry.file_name().to_string_lossy())
        {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// A fresh directory beside the slots, for files about to be moved into one: on the same
/// filesystem, so moving them in is a rename. One per process; a crashed one is cleared the next
/// time the same pid comes round, which is harmless, and `folio delete` takes the whole thread
/// directory anyway.
pub fn staging(directory: &Path) -> Result<PathBuf, String> {
    let path = directory.join(format!(".staging.{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn dir() -> (crate::tempdir::Dir, PathBuf) {
        let tmp = crate::tempdir::Dir::new();
        let thread = tmp.path().join("thread");
        (tmp, thread)
    }

    fn read(dir: &Path, name: &str) -> Vec<u8> {
        fs::read(current_slot(dir).unwrap().join(name)).unwrap()
    }

    fn slots(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn bytes(b: &[u8]) -> Content {
        Content::Bytes(b.to_vec())
    }

    #[test]
    fn nothing_written_is_no_slot() {
        let (_tmp, dir) = dir();
        assert_eq!(current_slot(&dir), None);
    }

    #[test]
    fn a_write_becomes_current() {
        let (_tmp, dir) = dir();
        let slot = write_slot(&dir, vec![("a", bytes(b"1"))]).unwrap();
        assert_eq!(current_slot(&dir), Some(slot));
        assert_eq!(read(&dir, "a"), b"1");
        assert!(dir.join(CURRENT).symlink_metadata().unwrap().is_symlink());
    }

    #[test]
    fn files_not_written_carry_over_as_links() {
        let (_tmp, dir) = dir();
        let first = write_slot(&dir, vec![("a", bytes(b"1")), ("big", bytes(b"archive"))]).unwrap();
        let inode = fs::metadata(first.join("big")).unwrap().ino();
        let second = write_slot(&dir, vec![("a", bytes(b"2"))]).unwrap();
        assert_eq!((read(&dir, "a"), read(&dir, "big")), (b"2".to_vec(), b"archive".to_vec()));
        assert_eq!(fs::metadata(second.join("big")).unwrap().ino(), inode);
    }

    #[test]
    fn a_file_given_none_is_left_out() {
        let (_tmp, dir) = dir();
        write_slot(&dir, vec![("a", bytes(b"1")), ("b", bytes(b"2"))]).unwrap();
        let slot = write_slot(&dir, vec![("b", Content::Leave)]).unwrap();
        assert!(slot.join("a").exists() && !slot.join("b").exists());
    }

    #[test]
    fn the_old_slot_goes() {
        let (_tmp, dir) = dir();
        write_slot(&dir, vec![("a", bytes(b"1"))]).unwrap();
        let second = write_slot(&dir, vec![("a", bytes(b"2"))]).unwrap();
        assert_eq!(slots(&dir), [second.file_name().unwrap().to_string_lossy()]);
    }

    #[test]
    fn a_crash_leaves_current_and_the_next_write_clears_up() {
        let (_tmp, dir) = dir();
        let first = write_slot(&dir, vec![("a", bytes(b"1"))]).unwrap();
        fs::create_dir(dir.join("20990101T000000.000Z")).unwrap();
        fs::create_dir(dir.join("notes")).unwrap();
        assert_eq!(read(&dir, "a"), b"1");
        let second = write_slot(&dir, vec![("a", bytes(b"2"))]).unwrap();
        let mut expected = vec![second.file_name().unwrap().to_string_lossy().into_owned(), "notes".into()];
        expected.sort();
        assert_eq!(slots(&dir), expected);
        assert!(!first.exists());
    }

    #[test]
    fn json_round_trips_and_missing_is_none() {
        let tmp = crate::tempdir::Dir::new();
        let path = tmp.path().join("deep").join("x.json");
        assert_eq!(read_json(&path).unwrap(), None);
        write_json(&path, &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(read_json(&path).unwrap(), Some(serde_json::json!({"a": 1})));
        let names: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["x.json"]);
    }

    #[test]
    fn a_slot_is_named_for_when() {
        let time = UNIX_EPOCH + Duration::from_millis(1_731_700_000_123);
        assert_eq!(slot_name(time), "20241115T194640.123Z");
        assert!(is_slot_name(&slot_name(time)));
    }
}
