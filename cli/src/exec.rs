//! `bottle-cli exec`: runs one command through a bottle's agent, as `smolvm machine exec -i` does,
//! and exits with its status (128 and the signal for one a signal killed), or 255 if it couldn't be
//! run.
//!
//!     bottle-cli exec --socket SOCKET [-t] [-u USER] [-w DIR] [-e NAME=VALUE]... [--] COMMAND [ARG]...
//!
//! With -t the command gets a terminal, as big as this one; this terminal, if stdin is one, is
//! raw while it runs, and set back after. SIGINT, SIGTERM and SIGHUP sent here are passed on to
//! the command.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{Sender, channel};
use std::thread;

use bottle_agent_client::{Control, Start, WinSize, run, status};
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;

use crate::FAILED;

pub const USAGE: &str =
    "exec --socket SOCKET [-t] [-u USER] [-w DIR] [-e NAME=VALUE]... [--] COMMAND [ARG]...";

/// The size of a terminal that can't be asked.
const DEFAULT_SIZE: WinSize = WinSize { rows: 24, cols: 80 };

/// The signals passed on to the command; SIGWINCH is this terminal's size changing.
const FORWARDED: [i32; 3] = [SIGINT, SIGTERM, SIGHUP];

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
        Err(e) => return crate::own_usage_error(&e, USAGE),
    };
    // Caught from here on (a handler that only wakes the thread below), and read there.
    let signals = match Signals::new(FORWARDED.into_iter().chain([SIGWINCH])) {
        Ok(signals) => signals,
        Err(e) => {
            eprintln!("bottle-cli: catching signals: {e}");
            return ExitCode::from(FAILED);
        }
    };
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

fn pass_signals_on(mut signals: Signals, tty: bool, controls: &Sender<Control>) {
    for signal in signals.forever() {
        let control = match signal {
            SIGWINCH if tty => Control::Resize(window_size().unwrap_or(DEFAULT_SIZE)),
            SIGWINCH => continue,
            signal => Control::Signal(signal),
        };
        if controls.send(control).is_err() {
            return;
        }
    }
}

/// This terminal's size: stdout's, else stdin's.
fn window_size() -> Option<WinSize> {
    [tcgetwinsize(io::stdout()), tcgetwinsize(io::stdin())]
        .into_iter()
        .flatten()
        .find(|size| size.ws_row > 0)
        .map(|size| WinSize {
            rows: size.ws_row,
            cols: size.ws_col,
        })
}

/// Stdin's terminal made raw, everything typed going to the command's terminal as it is, until
/// dropped.
struct RawMode(Termios);

impl RawMode {
    /// None if stdin isn't a terminal.
    fn enter() -> Option<RawMode> {
        let saved = tcgetattr(io::stdin()).ok()?;
        let mut raw = saved.clone();
        raw.make_raw();
        tcsetattr(io::stdin(), OptionalActions::Now, &raw).ok()?;
        Some(RawMode(saved))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(io::stdin(), OptionalActions::Now, &self.0);
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
