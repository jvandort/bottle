//! folio's logs, in its home's logs/: a line per record, stamped as folio's Python stamped its own:
//! the local time to the millisecond, the process, and the part of folio that says it.
//!
//! folio serve writes its records on stderr, which whoever starts it points at serve.log. A command
//! writes its own in folio.log (to_file): what it was asked, what it made, started, stopped, rolled
//! back and deleted, what it warned of (said on stderr too: warn), and how it ended, and how long it
//! took. Only their owner can read the logs, or list the home they're in: they name the hosts the
//! bottles reached, and the home their repos.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// The file a command writes its records in; none: stderr.
static FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// A log in folio's home, `name`.log.
pub fn path(name: &str) -> PathBuf {
    crate::home().join("logs").join(format!("{name}.log"))
}

/// The log `name`, opened for appending, private (folio's home and its logs/ too).
pub fn open(name: &str) -> std::io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    for dir in [crate::home(), crate::home().join("logs")] {
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path(name))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// This command's records, from now on, in the log `name`. Never fails a command: a log it can't
/// open, it goes without.
pub fn to_file(name: &str) {
    if let Ok(file) = open(name) {
        let _ = FILE.set(Mutex::new(file));
    }
}

/// `message`, said on stderr as it is, and recorded from `name`.
pub fn warn(name: &str, message: &str) {
    eprintln!("{message}");
    write(name, message);
}

/// "2026-11-14 09:15:02,123": now, local, as Python's logging writes it.
fn now() -> String {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: gettimeofday and localtime_r write the structures they're given.
    unsafe {
        libc::gettimeofday(&mut tv, std::ptr::null_mut());
        libc::localtime_r(&tv.tv_sec, &mut tm);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02},{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        tv.tv_usec / 1000
    )
}

/// Writes one record, from `name`, whole: lines from several threads never interleave.
pub fn write(name: &str, message: &str) {
    let line = format!("{} {} {name} {message}\n", now(), std::process::id());
    match FILE.get() {
        Some(file) => {
            let _ = file
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .write_all(line.as_bytes());
        }
        None => {
            let _ = std::io::stderr().lock().write_all(line.as_bytes());
        }
    }
}

/// `log!("bottle.daemon", "bound {}", bottle)`: a record, as write has it.
macro_rules! log {
    ($name:expr, $($arg:tt)*) => {
        $crate::log::write($name, &format!($($arg)*))
    };
}
pub(crate) use log;

/// `say!("bottles", "{} is ready", name)`: said on stderr, and recorded, as warn has it.
macro_rules! say {
    ($name:expr, $($arg:tt)*) => {
        $crate::log::warn($name, &format!($($arg)*))
    };
}
pub(crate) use say;
