//! The host's side of running a command in a bottle's VM: connect to the agent there (through the
//! unix socket its vsock port is bridged to), run one command, and pass its stdin, output, window
//! sizes and signals through until it exits.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

pub use bottle_agent_proto::{Exit, Start, WinSize};
use bottle_agent_proto::{ToGuest, ToHost};

pub mod net;
pub mod policy;

/// Stdin is sent in pieces of at most this much.
const CHUNK: usize = 64 * 1024;

/// What the caller has for the command while it runs, besides stdin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Resize(WinSize),
    Signal(i32),
}

/// Why a command didn't run to its exit.
#[derive(Debug)]
pub enum Error {
    /// Connecting to the agent, or talking to it, failed.
    Io(io::Error),
    /// The agent couldn't run the command, and said why.
    Agent(String),
    /// The agent closed the connection before the command's exit.
    NoExit,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Agent(message) => write!(f, "{message}"),
            Error::NoExit => write!(f, "the agent went away before the command exited"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Runs `start` through the agent listening at `socket`, until the command exits.
///
/// Stdin is read on a thread of its own, until it ends, and that end passed on; what comes from
/// `controls` is passed on as it comes. Neither thread is waited for: one blocked reading a
/// terminal would hold the exit up.
pub fn run(
    socket: &Path,
    start: Start,
    stdin: impl Read + Send + 'static,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    controls: Receiver<Control>,
) -> Result<Exit, Error> {
    let mut conn = UnixStream::connect(socket)?;
    ToGuest::Start(start).write(&mut conn)?;
    let outgoing = Arc::new(Mutex::new(conn.try_clone()?));
    {
        let outgoing = Arc::clone(&outgoing);
        thread::spawn(move || send_stdin(stdin, &outgoing));
    }
    {
        let outgoing = Arc::clone(&outgoing);
        thread::spawn(move || {
            for control in controls {
                let message = match control {
                    Control::Resize(size) => ToGuest::Resize(size),
                    Control::Signal(signal) => ToGuest::Signal(signal),
                };
                if message.write(&mut *outgoing.lock().unwrap()).is_err() {
                    return;
                }
            }
        });
    }
    let mut exit = None;
    while let Some(message) = ToHost::read(&mut conn)? {
        match message {
            ToHost::Started { .. } => {}
            ToHost::Stdout(data) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            ToHost::Stderr(data) => {
                stderr.write_all(&data)?;
                stderr.flush()?;
            }
            ToHost::Exit(e) => exit = Some(e),
            ToHost::Error(message) => return Err(Error::Agent(message)),
        }
    }
    exit.ok_or(Error::NoExit)
}

/// Stops the VM whose agent listens at `socket` (see ToGuest::Stop): once this returns, the
/// bottle's disk is clean, and the VM is ending.
pub fn stop(socket: &Path) -> Result<(), Error> {
    let mut conn = UnixStream::connect(socket)?;
    ToGuest::Stop.write(&mut conn)?;
    match ToHost::read(&mut conn)? {
        Some(ToHost::Exit(Exit::Code(0))) => Ok(()),
        Some(ToHost::Error(message)) => Err(Error::Agent(message)),
        _ => Err(Error::NoExit),
    }
}

fn send_stdin(mut stdin: impl Read, outgoing: &Mutex<UnixStream>) {
    let mut buf = vec![0; CHUNK];
    loop {
        let message = match stdin.read(&mut buf) {
            Ok(0) => ToGuest::StdinEof,
            Ok(n) => ToGuest::Stdin(buf[..n].to_vec()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // Stdin that can't be read is stdin that's ended.
            Err(_) => ToGuest::StdinEof,
        };
        let end = message == ToGuest::StdinEof;
        if message.write(&mut *outgoing.lock().unwrap()).is_err() || end {
            return;
        }
    }
}

/// The exit status a shell gives a command that ended so: its code, or 128 and the signal.
pub fn status(exit: Exit) -> i32 {
    match exit {
        Exit::Code(code) => code,
        Exit::Signal(signal) => 128 + signal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_is_the_code_or_128_and_the_signal() {
        assert_eq!(status(Exit::Code(0)), 0);
        assert_eq!(status(Exit::Code(7)), 7);
        assert_eq!(status(Exit::Signal(9)), 137);
    }
}
