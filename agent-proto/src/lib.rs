//! What bottle and the agent in a bottle's VM say to each other: one connection per command run.
//!
//! The host opens a connection and sends [`ToGuest::Start`]; then stdin, window sizes and signals go
//! one way, output the other, until the guest sends [`ToHost::Exit`], once the command has exited
//! and all of its output is sent, and closes. Each message is a frame: its kind (one byte), the
//! length of its payload (four bytes, big-endian), and the payload, which is raw bytes for stdin
//! and output and JSON for the rest.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// The largest payload a frame may have. Output is sent in pieces well under it.
pub const MAX_PAYLOAD: usize = 1 << 20;

/// A command to run, as the host asks for it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    /// The program and its arguments, run as they are: no shell.
    pub argv: Vec<String>,
    /// Set on top of the agent's own environment, after HOME, USER and LOGNAME for `user`.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Who to run it as, by name; the agent's own user if None.
    #[serde(default)]
    pub user: Option<String>,
    /// Where to run it; the user's home if None, or / if that isn't there.
    #[serde(default)]
    pub cwd: Option<String>,
    /// A terminal of this size, which stdin, stdout and stderr all are, if Some.
    #[serde(default)]
    pub tty: Option<WinSize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
}

/// How a command ended: its exit code, or the signal that killed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Started {
    pid: u32,
}

/// What the host sends.
#[derive(Debug, PartialEq, Eq)]
pub enum ToGuest {
    Start(Start),
    Stdin(Vec<u8>),
    /// No more stdin: the command's stdin is closed.
    StdinEof,
    /// The terminal's new size. Without a terminal, ignored.
    Resize(WinSize),
    /// A signal for the command.
    Signal(i32),
    /// Instead of a Start, as a connection's first frame: stop the VM. The agent stops everything
    /// else running, makes the bottle's disk clean (synced and remounted read-only: what a build's
    /// upper has to be, to be a stage), says Exit, and exits, which ends the VM.
    Stop,
}

/// What the guest sends.
#[derive(Debug, PartialEq, Eq)]
pub enum ToHost {
    Started {
        pid: u32,
    },
    /// With a terminal, all of the command's output.
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The last frame, sent once the command has exited and its output is all sent.
    Exit(Exit),
    /// The command couldn't be run, and won't be: the last frame.
    Error(String),
}

const START: u8 = 1;
const STDIN: u8 = 2;
const STDIN_EOF: u8 = 3;
const RESIZE: u8 = 4;
const SIGNAL: u8 = 5;
const STOP: u8 = 6;
const STARTED: u8 = 64;
const STDOUT: u8 = 65;
const STDERR: u8 = 66;
const EXIT: u8 = 67;
const ERROR: u8 = 68;

fn invalid(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

fn json<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("the protocol's types serialize")
}

fn parse<'a, T: Deserialize<'a>>(payload: &'a [u8]) -> io::Result<T> {
    serde_json::from_slice(payload).map_err(invalid)
}

/// Writes one frame.
pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(invalid(format!("a frame of {} bytes", payload.len())));
    }
    let mut header = [0; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&header)?;
    w.write_all(payload)?;
    w.flush()
}

/// Reads one frame, or None at the end of the stream between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0; 5];
    let mut got = 0;
    while got < header.len() {
        match r.read(&mut header[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        return Err(invalid(format!("a frame of {len} bytes")));
    }
    let mut payload = vec![0; len];
    r.read_exact(&mut payload)?;
    Ok(Some((header[0], payload)))
}

impl ToGuest {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            ToGuest::Start(start) => write_frame(w, START, &json(start)),
            ToGuest::Stdin(data) => write_frame(w, STDIN, data),
            ToGuest::StdinEof => write_frame(w, STDIN_EOF, &[]),
            ToGuest::Resize(size) => write_frame(w, RESIZE, &json(size)),
            ToGuest::Signal(signal) => write_frame(w, SIGNAL, &json(signal)),
            ToGuest::Stop => write_frame(w, STOP, &[]),
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<Option<Self>> {
        let Some((kind, payload)) = read_frame(r)? else {
            return Ok(None);
        };
        Ok(Some(match kind {
            START => ToGuest::Start(parse(&payload)?),
            STDIN => ToGuest::Stdin(payload),
            STDIN_EOF => ToGuest::StdinEof,
            RESIZE => ToGuest::Resize(parse(&payload)?),
            SIGNAL => ToGuest::Signal(parse(&payload)?),
            STOP => ToGuest::Stop,
            other => return Err(invalid(format!("a frame of kind {other} from the host"))),
        }))
    }
}

impl ToHost {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            ToHost::Started { pid } => write_frame(w, STARTED, &json(&Started { pid: *pid })),
            ToHost::Stdout(data) => write_frame(w, STDOUT, data),
            ToHost::Stderr(data) => write_frame(w, STDERR, data),
            ToHost::Exit(exit) => write_frame(w, EXIT, &json(exit)),
            ToHost::Error(message) => write_frame(w, ERROR, &json(message)),
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<Option<Self>> {
        let Some((kind, payload)) = read_frame(r)? else {
            return Ok(None);
        };
        Ok(Some(match kind {
            STARTED => ToHost::Started {
                pid: parse::<Started>(&payload)?.pid,
            },
            STDOUT => ToHost::Stdout(payload),
            STDERR => ToHost::Stderr(payload),
            EXIT => ToHost::Exit(parse(&payload)?),
            ERROR => ToHost::Error(parse(&payload)?),
            other => return Err(invalid(format!("a frame of kind {other} from the guest"))),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_guest(message: ToGuest) {
        let mut wire = Vec::new();
        message.write(&mut wire).unwrap();
        let mut r = wire.as_slice();
        assert_eq!(ToGuest::read(&mut r).unwrap(), Some(message));
        assert_eq!(ToGuest::read(&mut r).unwrap(), None);
    }

    fn round_trip_host(message: ToHost) {
        let mut wire = Vec::new();
        message.write(&mut wire).unwrap();
        let mut r = wire.as_slice();
        assert_eq!(ToHost::read(&mut r).unwrap(), Some(message));
        assert_eq!(ToHost::read(&mut r).unwrap(), None);
    }

    #[test]
    fn every_message_round_trips() {
        round_trip_guest(ToGuest::Start(Start {
            argv: vec!["sh".into(), "-c".into(), "echo hi".into()],
            env: vec![("A".into(), "b c".into())],
            user: Some("genie".into()),
            cwd: Some("/workspace".into()),
            tty: Some(WinSize { rows: 24, cols: 80 }),
        }));
        round_trip_guest(ToGuest::Stdin(b"\0\xffbytes".to_vec()));
        round_trip_guest(ToGuest::StdinEof);
        round_trip_guest(ToGuest::Resize(WinSize {
            rows: 50,
            cols: 132,
        }));
        round_trip_guest(ToGuest::Signal(15));
        round_trip_guest(ToGuest::Stop);
        round_trip_host(ToHost::Started { pid: 42 });
        round_trip_host(ToHost::Stdout(b"out".to_vec()));
        round_trip_host(ToHost::Stderr(Vec::new()));
        round_trip_host(ToHost::Exit(Exit::Code(3)));
        round_trip_host(ToHost::Exit(Exit::Signal(9)));
        round_trip_host(ToHost::Error("no such user: nobody2".into()));
    }

    #[test]
    fn a_start_needs_only_argv() {
        let start: Start = serde_json::from_str(r#"{"argv":["true"]}"#).unwrap();
        assert_eq!(
            start,
            Start {
                argv: vec!["true".into()],
                ..Start::default()
            }
        );
    }

    #[test]
    fn a_frame_cut_short_is_an_error_not_the_end() {
        let mut wire = Vec::new();
        ToHost::Stdout(b"hello".to_vec()).write(&mut wire).unwrap();
        wire.truncate(7);
        let err = ToHost::read(&mut wire.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        let err = ToHost::read(&mut &wire[..3]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn an_oversized_frame_is_refused() {
        let mut header = vec![STDOUT];
        header.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_be_bytes());
        assert_eq!(
            ToHost::read(&mut header.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(write_frame(&mut Vec::new(), STDOUT, &vec![0; MAX_PAYLOAD + 1]).is_err());
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        let mut wire = Vec::new();
        write_frame(&mut wire, 200, b"").unwrap();
        assert!(ToHost::read(&mut wire.as_slice()).is_err());
        assert!(ToGuest::read(&mut wire.as_slice()).is_err());
    }
}

/// What the agent and the host say to each other about the bottle's network: one stream per
/// connection the bottle makes, or per name it looks up, from the agent to the host (over vsock,
/// which libkrun carries to a unix socket of the VM's process).
///
/// A connection's stream starts with [`net::Request::Connect`]; then everything the bottle sends
/// on it follows, as it is, from which the host takes the first bytes to decide by. The host
/// answers one byte, [`net::CONNECTED`] or [`net::REFUSED`], and then, connected, what the far end
/// sends. A lookup's stream is [`net::Request::Resolve`], answered by [`net::Resolved`].
pub mod net {
    use std::io::{self, Read, Write};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    /// The vsock port the host takes the bottle's network on.
    pub const PORT: u32 = 1025;

    pub const CONNECTED: u8 = 0;
    pub const REFUSED: u8 = 1;

    const CONNECT: u8 = 1;
    const RESOLVE: u8 = 2;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Request {
        /// A connection to there.
        Connect(SocketAddr),
        /// The addresses of a name.
        Resolve(String),
    }

    impl Request {
        pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
            let mut out = Vec::new();
            match self {
                Request::Connect(addr) => {
                    out.push(CONNECT);
                    put_ip(&mut out, addr.ip());
                    out.extend_from_slice(&addr.port().to_be_bytes());
                }
                Request::Resolve(name) => {
                    let len = u8::try_from(name.len()).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "a name too long")
                    })?;
                    out.extend_from_slice(&[RESOLVE, len]);
                    out.extend_from_slice(name.as_bytes());
                }
            }
            w.write_all(&out)?;
            w.flush()
        }

        pub fn read(r: &mut impl Read) -> io::Result<Request> {
            match byte(r)? {
                CONNECT => {
                    let ip = get_ip(r)?;
                    let mut port = [0; 2];
                    r.read_exact(&mut port)?;
                    Ok(Request::Connect(SocketAddr::new(
                        ip,
                        u16::from_be_bytes(port),
                    )))
                }
                RESOLVE => {
                    let mut name = vec![0; byte(r)? as usize];
                    r.read_exact(&mut name)?;
                    String::from_utf8(name)
                        .map(Request::Resolve)
                        .map_err(|_| super::invalid("a name that isn't UTF-8"))
                }
                other => Err(super::invalid(format!("a network request of kind {other}"))),
            }
        }
    }

    /// The answer to a [`Request::Resolve`].
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Resolved {
        /// The name's addresses: perhaps none, a name with none of the kind asked for.
        Addresses(Vec<IpAddr>),
        /// There's no such name.
        NoSuchName,
        /// It couldn't be looked up, for now: worth asking again.
        Failed,
    }

    const NO_SUCH_NAME: u8 = 0xff;
    const FAILED: u8 = 0xfe;

    impl Resolved {
        pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
            let mut out = Vec::new();
            match self {
                Resolved::NoSuchName => out.push(NO_SUCH_NAME),
                Resolved::Failed => out.push(FAILED),
                Resolved::Addresses(addrs) => {
                    let addrs = &addrs[..addrs.len().min(32)];
                    out.push(addrs.len() as u8);
                    for &ip in addrs {
                        put_ip(&mut out, ip);
                    }
                }
            }
            w.write_all(&out)?;
            w.flush()
        }

        pub fn read(r: &mut impl Read) -> io::Result<Resolved> {
            match byte(r)? {
                NO_SUCH_NAME => Ok(Resolved::NoSuchName),
                FAILED => Ok(Resolved::Failed),
                count => (0..count)
                    .map(|_| get_ip(r))
                    .collect::<io::Result<_>>()
                    .map(Resolved::Addresses),
            }
        }
    }

    fn byte(r: &mut impl Read) -> io::Result<u8> {
        let mut b = [0];
        r.read_exact(&mut b)?;
        Ok(b[0])
    }

    fn put_ip(out: &mut Vec<u8>, ip: IpAddr) {
        match ip {
            IpAddr::V4(v4) => {
                out.push(4);
                out.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                out.push(6);
                out.extend_from_slice(&v6.octets());
            }
        }
    }

    fn get_ip(r: &mut impl Read) -> io::Result<IpAddr> {
        match byte(r)? {
            4 => {
                let mut o = [0; 4];
                r.read_exact(&mut o)?;
                Ok(IpAddr::V4(Ipv4Addr::from(o)))
            }
            6 => {
                let mut o = [0; 16];
                r.read_exact(&mut o)?;
                Ok(IpAddr::V6(Ipv6Addr::from(o)))
            }
            other => Err(super::invalid(format!("an address of family {other}"))),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn requests_and_answers_round_trip() {
            for request in [
                Request::Connect("93.184.216.34:443".parse().unwrap()),
                Request::Connect("[2001:db8::1]:80".parse().unwrap()),
                Request::Resolve("example.com".into()),
            ] {
                let mut wire = Vec::new();
                request.write(&mut wire).unwrap();
                assert_eq!(Request::read(&mut wire.as_slice()).unwrap(), request);
            }
            for answer in [
                Resolved::NoSuchName,
                Resolved::Failed,
                Resolved::Addresses(vec![]),
                Resolved::Addresses(vec!["1.2.3.4".parse().unwrap(), "::1".parse().unwrap()]),
            ] {
                let mut wire = Vec::new();
                answer.write(&mut wire).unwrap();
                assert_eq!(Resolved::read(&mut wire.as_slice()).unwrap(), answer);
            }
        }
    }
}
