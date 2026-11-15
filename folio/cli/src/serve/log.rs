//! folio serve's log: a line per record on stderr, which whoever starts it points at its log file,
//! stamped as folio's Python stamped its own: the local time to the millisecond,
//! the process, and the part of folio serve that says it.

use std::io::Write;

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
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

/// `log!("bottle.daemon", "bound {}", bottle)`: a record, as write has it.
macro_rules! log {
    ($name:expr, $($arg:tt)*) => {
        $crate::serve::log::write($name, &format!($($arg)*))
    };
}
pub(crate) use log;
