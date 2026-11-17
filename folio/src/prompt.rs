//! What folio asks of the person at the terminal.

use std::io::{BufRead, Write};

/// A line typed after `prompt`; None at the end of input, or if Ctrl-C ends it: a prompt interrupted
/// is a prompt not answered.
///
/// Ctrl-C is recorded (interrupt::install, as every command but a runner's has it), not acted on
/// by a read: stdin's polled, a while at a time, between looks at whether it came. One that ends
/// a prompt is forgotten after: it ended the prompt, not the command.
pub fn input(prompt: &str) -> Option<String> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let before = crate::interrupt::happened();
    let interrupted = || !before && crate::interrupt::happened();
    let stdin = std::io::stdin();
    let a_while = Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    let mut line = Vec::new();
    let answered = loop {
        if interrupted() {
            break false;
        }
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        match poll(&mut fds, Some(&a_while)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => {}
            Err(_) => break false,
        }
        // Readable: a byte, or the end, there now. One at a time, from the descriptor itself, so
        // nothing past the line is taken from whatever reads stdin next.
        let mut byte = [0u8];
        match rustix::io::read(&stdin, &mut byte) {
            Ok(1) if byte[0] == b'\n' => break true,
            Ok(1) => line.push(byte[0]),
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
            _ => break false,
        }
    };
    if interrupted() {
        crate::interrupt::forget();
    }
    if !answered {
        println!();
        return None;
    }
    let line = String::from_utf8_lossy(&line).into_owned();
    Some(line.trim_end_matches('\r').to_string())
}

/// A line from the terminal, unechoed, after `prompt`.
pub fn getpass(prompt: &str) -> Result<String, String> {
    use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let saved = tcgetattr(&stdin).map_err(|_| "can't read the terminal's settings".to_string())?;
    let mut quiet = saved.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    let _ = tcsetattr(&stdin, OptionalActions::Flush, &quiet);
    eprint!("{prompt}");
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    let _ = tcsetattr(&stdin, OptionalActions::Flush, &saved);
    eprintln!();
    read.map_err(|e| e.to_string())?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}
