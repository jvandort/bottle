//! The HTTP cache's files: $BOTTLE_HOME/cache/http/<host>/<path>,
//! the path as the bottle asked for it. Only a whole 200 is kept, so a file is all there is: no
//! headers, no status. A file's modification time is when a bottle last used it: one unused for
//! EXPIRY is removed (Store::expire). A file being downloaded is written in .partial/ and put in place only once
//! it's complete, so a reader never sees half of one.

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Where files are written before they're complete; a hidden name, which no host has.
const PARTIAL: &str = ".partial";
/// A kept file unused for this long is removed: long enough for the threads of a day's work on one
/// project to share their downloads.
pub const EXPIRY: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A partial file this old was left by a process that stopped mid-download.
const PARTIAL_EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);

pub struct Store {
    pub root: PathBuf,
}

impl Store {
    /// The store bottled keeps too: $BOTTLE_HOME/cache/http.
    pub fn at_home() -> Store {
        let home = std::env::var_os("BOTTLE_HOME").map_or_else(
            || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".bottle"),
            PathBuf::from,
        );
        Store {
            root: home.join("cache/http"),
        }
    }

    /// Where the file for `path` on `host` is kept; None for a path no file can be kept at: a
    /// directory's (ending in /), or one the filesystem would read as another (`.`, `..`, `//`).
    pub fn file(&self, host: &str, path: &str) -> Option<PathBuf> {
        let host = host.to_lowercase();
        if !is_host(&host) {
            return None;
        }
        let mut segments = path.split('/');
        if segments.next() != Some("") {
            return None;
        }
        let segments: Vec<&str> = segments.collect();
        if segments.is_empty() {
            return None;
        }
        let mut file = self.root.join(&host);
        for segment in segments {
            if matches!(segment, "" | "." | "..") || segment.contains('\0') || segment.len() > 255 {
                return None;
            }
            file.push(segment);
        }
        Some(file)
    }

    /// The file kept for `path` on `host`, open for reading, and marked used now; None if there
    /// isn't one.
    pub fn open(&self, host: &str, path: &str) -> Option<File> {
        let file = self.file(host, path)?;
        let opened = File::open(&file).ok()?;
        if !opened.metadata().ok()?.is_file() || !named(&opened, &file, &self.root) {
            return None;
        }
        // SAFETY: futimens with a null times sets both to now, on a descriptor this owns.
        unsafe { libc::futimens(opened.as_raw_fd(), std::ptr::null()) };
        Some(opened)
    }

    /// A file for `path` on `host` to write, kept when it's committed; None if it can't be kept.
    pub fn partial(&self, host: &str, path: &str) -> Option<Partial> {
        let file = self.file(host, path)?;
        let dir = self.root.join(PARTIAL);
        fs::create_dir_all(&dir).ok()?;
        let mut random = [0u8; 8];
        File::open("/dev/urandom")
            .and_then(|mut f| io::Read::read_exact(&mut f, &mut random))
            .ok()?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let temporary = dir.join(format!("tmp{name}"));
        let out = File::create_new(&temporary).ok()?;
        Some(Partial {
            root: self.root.clone(),
            file,
            temporary,
            out: Some(out),
        })
    }
}

impl Store {
    /// Removes the files no bottle has used for EXPIRY, partial files older than PARTIAL_EXPIRY,
    /// and the directories that leaves empty. A file being sent as it's removed is still sent
    /// whole.
    pub fn expire(&self) {
        let now = SystemTime::now();
        expire_under(&self.root, &self.root, now);
    }
}

/// Expires what's under `dir`; whether it's empty after.
fn expire_under(root: &Path, dir: &Path, now: SystemTime) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let expiry = if dir == root.join(PARTIAL) {
        PARTIAL_EXPIRY
    } else {
        EXPIRY
    };
    let mut empty = true;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            empty = false;
            continue;
        };
        if meta.is_dir() {
            if expire_under(root, &path, now)
                && path != root.join(PARTIAL)
                && fs::remove_dir(&path).is_ok()
            {
                continue;
            }
        } else if meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > expiry)
            && fs::remove_file(&path).is_ok()
        {
            continue;
        }
        empty = false;
    }
    empty
}

/// A host's name as a store's directory: a name of letters, digits and hyphens between dots.
fn is_host(host: &str) -> bool {
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// Whether what `opened` has open is `file`, letter for letter. On a filesystem that ignores case
/// (a Mac's, by default) `Foo.tgz` opens `foo.tgz`, which is another file to a server: the name it
/// has on disk is asked for (F_GETPATH, macOS only); elsewhere, names are taken to be exact.
#[cfg(target_os = "macos")]
fn named(opened: &File, file: &Path, root: &Path) -> bool {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes into buf.
    if unsafe { libc::fcntl(opened.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
        return false;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let on_disk = String::from_utf8_lossy(&buf[..end]).into_owned();
    let Ok(relative) = file.strip_prefix(root) else {
        return false;
    };
    on_disk.ends_with(&format!("/{}", relative.display()))
}

#[cfg(not(target_os = "macos"))]
fn named(_: &File, _: &Path, _: &Path) -> bool {
    true
}

/// A file being downloaded: written to one side, and put in its place only when complete.
pub struct Partial {
    root: PathBuf,
    file: PathBuf,
    temporary: PathBuf,
    out: Option<File>,
}

impl Partial {
    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        match &mut self.out {
            Some(out) => out.write_all(data),
            None => Err(io::Error::other("committed or discarded already")),
        }
    }

    /// Keeps the file; false if it can't be, where something else already is or would be read as
    /// it (a file where a directory goes, or a directory, perhaps under another case, where the
    /// file does).
    pub fn commit(mut self) -> bool {
        drop(self.out.take());
        let kept = (|| -> io::Result<bool> {
            let parent = self.file.parent().unwrap();
            fs::create_dir_all(parent)?;
            if !named(&File::open(parent)?, parent, &self.root) {
                return Ok(false);
            }
            // A file kept under this name in another case: a rename might leave it its name.
            if let Ok(existing) = File::open(&self.file)
                && !named(&existing, &self.file, &self.root)
            {
                fs::remove_file(&self.file)?;
            }
            fs::rename(&self.temporary, &self.file)?;
            Ok(true)
        })()
        .unwrap_or(false);
        if !kept {
            let _ = fs::remove_file(&self.temporary);
        }
        kept
    }
}

impl Drop for Partial {
    /// Discarded, unless committed.
    fn drop(&mut self) {
        if self.out.is_some() {
            self.out = None;
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, PathBuf) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("bottle-store-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        (Store { root: root.clone() }, root)
    }

    #[test]
    fn a_path_is_a_file_under_its_host_when_it_can_be_one() {
        let (store, root) = store();
        assert_eq!(
            store.file("Repo.Example", "/a/b-1.0.jar"),
            Some(root.join("repo.example/a/b-1.0.jar"))
        );
        for bad in ["/", "/a/", "//a", "/a/../b", "/./a", "a", ""] {
            assert_eq!(store.file("repo.example", bad), None, "{bad}");
        }
        for bad in ["*.example", "..", "a..b", "-a.example", "a_b.example", ""] {
            assert_eq!(store.file(bad, "/a"), None, "{bad}");
        }
    }

    #[test]
    fn a_file_is_there_only_once_its_committed_and_using_it_marks_it() {
        let (store, root) = store();
        assert!(store.open("repo.example", "/a.jar").is_none());
        let mut partial = store.partial("repo.example", "/a.jar").unwrap();
        partial.write(b"half").unwrap();
        assert!(store.open("repo.example", "/a.jar").is_none());
        partial.write(b" and the rest").unwrap();
        assert!(partial.commit());
        let mut kept = String::new();
        io::Read::read_to_string(
            &mut store.open("repo.example", "/a.jar").unwrap(),
            &mut kept,
        )
        .unwrap();
        assert_eq!(kept, "half and the rest");
        // Marked used: its modification time is now.
        let file = root.join("repo.example/a.jar");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        store.open("repo.example", "/a.jar").unwrap();
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        assert!(modified.elapsed().unwrap() < std::time::Duration::from_secs(60));
        assert_eq!(fs::read_dir(root.join(PARTIAL)).unwrap().count(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn one_dropped_unfinished_leaves_nothing() {
        let (store, root) = store();
        let mut partial = store.partial("repo.example", "/a.jar").unwrap();
        partial.write(b"half").unwrap();
        drop(partial);
        assert!(store.open("repo.example", "/a.jar").is_none());
        assert_eq!(fs::read_dir(root.join(PARTIAL)).unwrap().count(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn what_goes_unused_is_removed_with_the_directories_it_leaves_empty() {
        let (store, root) = store();
        for path in ["/old/a.jar", "/new/b.jar"] {
            let mut partial = store.partial("repo.example", path).unwrap();
            partial.write(b"x").unwrap();
            assert!(partial.commit());
        }
        let left = store.partial("repo.example", "/c.jar").unwrap();
        std::mem::forget(left); // as a process that stopped mid-download leaves it
        let long_ago = SystemTime::now() - EXPIRY - Duration::from_secs(60);
        let age = |path: &Path| {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(long_ago)
                .unwrap()
        };
        age(&root.join("repo.example/old/a.jar"));
        let partial = fs::read_dir(root.join(PARTIAL))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        age(&partial);
        store.expire();
        assert!(!root.join("repo.example/old").exists());
        assert!(root.join("repo.example/new/b.jar").exists());
        assert!(!partial.exists());
        assert!(root.join(PARTIAL).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn where_a_directory_is_no_file_is_kept() {
        let (store, root) = store();
        fs::create_dir_all(root.join("repo.example/a.jar/x")).unwrap();
        let partial = store.partial("repo.example", "/a.jar").unwrap();
        assert!(!partial.commit());
        assert_eq!(fs::read_dir(root.join(PARTIAL)).unwrap().count(), 0);
        let _ = fs::remove_dir_all(root);
    }
}
