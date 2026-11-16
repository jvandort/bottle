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

/// The environment folio's tests run in, for a test that reads it or folio's home: held while the
/// test runs, so one test at a time has it (the environment is the process's, and tests run in
/// parallel). Git is configured by nothing but what a test sets, committing as Test; folio's home
/// is a fresh one for each test.
pub struct Isolated {
    _held: std::sync::MutexGuard<'static, ()>,
    home: Dir,
}

impl Isolated {
    pub fn home(&self) -> &Path {
        self.home.path()
    }
}

pub fn isolated() -> Isolated {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let held = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = Dir::new();
    let vars = [
        ("FOLIO_HOME", home.path().to_str().unwrap()),
        ("BOTTLE_HOME", "/nonexistent/bottle-home"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_AUTHOR_NAME", "Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ];
    for (key, value) in vars {
        // SAFETY: under the lock every test that reads the environment holds.
        unsafe { std::env::set_var(key, value) };
    }
    for key in ["FOLIO_FEATURES", "FAKE_CLI_FAIL", "FAKE_CLI_MISSING"] {
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
    }
    Isolated { _held: held, home }
}

/// Set `key` for the rest of an isolated test.
pub fn set(_: &Isolated, key: &str, value: &str) {
    // SAFETY: under the lock the Isolated holds.
    unsafe { std::env::set_var(key, value) };
}

/// This crate's folio, built: what a test runs where folio runs itself (a hook, say), the test
/// being another program.
pub fn folio() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")));
    let built = target.join("debug").join("folio");
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "folio"])
            .status()
            .unwrap();
        assert!(status.success(), "building folio");
    });
    built
}

/// folio-standin, built: the stand-ins for what folio runs, each by the name it's run as (a
/// link to it named bottle-cli, ssh or security).
pub fn standin() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")));
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "folio-standin"])
            .status()
            .unwrap();
        assert!(status.success(), "building folio-standin");
    });
    target.join("debug").join("folio-standin")
}

/// git `args`, which must succeed; its stdout, trimmed.
pub fn git(args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo at `path`, on main, with a first commit.
pub fn repo(path: &Path) -> PathBuf {
    let p = path.to_str().unwrap();
    git(&["init", "-q", "-b", "main", p]);
    git(&["-C", p, "commit", "-q", "--allow-empty", "-m", "initial"]);
    path.to_path_buf()
}
