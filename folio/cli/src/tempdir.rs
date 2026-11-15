//! Tests' temporary directories: made fresh, removed when dropped.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Dir(PathBuf);

impl Dir {
    pub fn new() -> Dir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "folio-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        // As git reports it: /tmp may be a symlink (macOS's /var is).
        Dir(path.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The environment folio's tests run in, set once for the process (tests run in parallel, and the
/// environment is the process's): folio's home a temporary one, and git configured by nothing but
/// what a test sets, committing as Test.
pub fn isolated() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let home = std::env::temp_dir().join(format!("folio-test-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let vars = [
            ("FOLIO_HOME", home.to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "Test"),
            ("GIT_AUTHOR_EMAIL", "test@example.com"),
            ("GIT_COMMITTER_NAME", "Test"),
            ("GIT_COMMITTER_EMAIL", "test@example.com"),
        ];
        for (key, value) in vars {
            // SAFETY: once, before any test reads the environment (each calls this first).
            unsafe { std::env::set_var(key, value) };
        }
    });
}

/// git `args`, which must succeed; its stdout, trimmed.
pub fn git(args: &[&str]) -> String {
    let out = std::process::Command::new("git").args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo at `path`, on main, with a first commit.
pub fn repo(path: &Path) -> PathBuf {
    let p = path.to_str().unwrap();
    git(&["init", "-q", "-b", "main", p]);
    git(&["-C", p, "commit", "-q", "--allow-empty", "-m", "initial"]);
    path.to_path_buf()
}
