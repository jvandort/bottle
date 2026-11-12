//! `bottle-cli exec`: runs one command through a bottle's agent, as `smolvm machine exec -i` does,
//! and exits with its status (128 and the signal for one a signal killed), or 255 if it couldn't be
//! run.
//!
//!     bottle-cli exec SOCKET [-t] [-u USER] [-w DIR] [-e NAME=VALUE]... [--] COMMAND [ARG]...
//!
//! With -t the command gets a terminal, as big as this one; this terminal, if stdin is one, is
//! raw while it runs, and set back after. SIGINT, SIGTERM and SIGHUP sent here are passed on to
//! the command.

use std::io;
use std::mem::zeroed;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{Sender, channel};
use std::thread;

use bottle_agent_client::{Control, Start, WinSize, run, status};

use crate::FAILED;

pub const USAGE: &str =
    "exec SOCKET [-t] [-u USER] [-w DIR] [-e NAME=VALUE]... [--] COMMAND [ARG]...";

/// The size of a terminal that can't be asked.
const DEFAULT_SIZE: WinSize = WinSize { rows: 24, cols: 80 };

/// The signals passed on to the command; SIGWINCH is this terminal's size changing.
const FORWARDED: [i32; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

struct Args {
    socket: PathBuf,
    start: Start,
    tty: bool,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let socket = args.next().ok_or("no SOCKET")?.into();
    let mut start = Start::default();
    let mut tty = false;
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "-t" => tty = true,
            "-u" => start.user = Some(value("-u")?),
            "-w" => start.cwd = Some(value("-w")?),
            "-e" => {
                let pair = value("-e")?;
                let (name, val) = pair
                    .split_once('=')
                    .ok_or(format!("-e {pair}: not NAME=VALUE"))?;
                start.env.push((name.into(), val.into()));
            }
            "--" => break,
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            _ => {
                start.argv.push(arg);
                break;
            }
        }
    }
    start.argv.extend(args);
    if start.argv.is_empty() {
        return Err("no COMMAND".into());
    }
    Ok(Args { socket, start, tty })
}

/// Runs `bottle-cli exec` with `args`, those after `exec`.
pub fn main(args: impl Iterator<Item = String>) -> ExitCode {
    let mut args = match parse(args) {
        Ok(args) => args,
        Err(e) => return crate::usage_error(&e),
    };
    // Before any thread starts, so they all inherit it: these signals are waited for, below, and
    // never interrupt anything.
    let signals = block_signals();
    let (controls, received) = channel();
    if args.tty {
        args.start.tty = Some(window_size().unwrap_or(DEFAULT_SIZE));
    }
    let tty = args.tty;
    thread::spawn(move || pass_signals_on(signals, tty, &controls));

    let raw = if args.tty { RawMode::enter() } else { None };
    let result = run(
        &args.socket,
        args.start,
        io::stdin(),
        &mut io::stdout(),
        &mut io::stderr(),
        received,
    );
    drop(raw);
    match result {
        Ok(exit) => ExitCode::from(status(exit) as u8),
        Err(e) => {
            eprintln!("bottle-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

/// Does nothing: SIGWINCH's handler, so that it's kept pending for sigwait. macOS discards a signal
/// whose action is to be ignored, as SIGWINCH's default is, even while it's blocked.
extern "C" fn nothing(_: libc::c_int) {}

fn block_signals() -> libc::sigset_t {
    // SAFETY: a handler that does nothing set for SIGWINCH; a sigset made empty, then filled in,
    // and this thread's mask set from it.
    unsafe {
        libc::signal(
            libc::SIGWINCH,
            nothing as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        let mut set: libc::sigset_t = zeroed();
        libc::sigemptyset(&mut set);
        for signal in FORWARDED.into_iter().chain([libc::SIGWINCH]) {
            libc::sigaddset(&mut set, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

fn pass_signals_on(set: libc::sigset_t, tty: bool, controls: &Sender<Control>) {
    loop {
        let mut signal = 0;
        // SAFETY: sigwait on a set of signals this process blocks.
        if unsafe { libc::sigwait(&set, &mut signal) } != 0 {
            continue;
        }
        let control = match signal {
            libc::SIGWINCH if tty => Control::Resize(window_size().unwrap_or(DEFAULT_SIZE)),
            libc::SIGWINCH => continue,
            signal => Control::Signal(signal),
        };
        if controls.send(control).is_err() {
            return;
        }
    }
}

/// This terminal's size: stdout's, else stdin's.
fn window_size() -> Option<WinSize> {
    [libc::STDOUT_FILENO, libc::STDIN_FILENO]
        .into_iter()
        .find_map(|fd| {
            // SAFETY: TIOCGWINSZ writes a winsize.
            let mut size: libc::winsize = unsafe { zeroed() };
            let ok =
                unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_row > 0;
            ok.then_some(WinSize {
                rows: size.ws_row,
                cols: size.ws_col,
            })
        })
}

/// Stdin's terminal made raw, everything typed going to the command's terminal as it is, until
/// dropped.
struct RawMode(libc::termios);

impl RawMode {
    /// None if stdin isn't a terminal.
    fn enter() -> Option<RawMode> {
        // SAFETY: tcgetattr writes the termios; cfmakeraw changes a copy, which tcsetattr reads.
        unsafe {
            let mut saved: libc::termios = zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode(saved))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: tcsetattr reads the termios saved before.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Result<Args, String> {
        parse(line.split(' ').map(String::from))
    }

    #[test]
    fn parses_options_then_the_command() {
        let a = args("/s -t -u genie -w /workspace -e A=b=c -e B= -- ls -l").unwrap();
        assert_eq!(a.socket, PathBuf::from("/s"));
        assert!(a.tty);
        assert_eq!(a.start.user.as_deref(), Some("genie"));
        assert_eq!(a.start.cwd.as_deref(), Some("/workspace"));
        assert_eq!(
            a.start.env,
            vec![("A".into(), "b=c".into()), ("B".into(), "".into())]
        );
        assert_eq!(a.start.argv, vec!["ls", "-l"]);
    }

    #[test]
    fn the_command_starts_at_the_first_word_that_isnt_an_option() {
        let a = args("/s echo -t -- x").unwrap();
        assert!(!a.tty);
        assert_eq!(a.start.argv, vec!["echo", "-t", "--", "x"]);
    }

    #[test]
    fn refuses_what_it_cant_run() {
        assert_eq!(args("/s").err().unwrap(), "no COMMAND");
        assert_eq!(args("/s -t --").err().unwrap(), "no COMMAND");
        assert_eq!(args("/s -x ls").err().unwrap(), "unknown option -x");
        assert_eq!(args("/s -u").err().unwrap(), "-u needs a value");
        assert_eq!(args("/s -e AB ls").err().unwrap(), "-e AB: not NAME=VALUE");
    }
}
