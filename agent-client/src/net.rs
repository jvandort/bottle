//! The host's side of a bottle's network (see bottle_agent_proto::net): each connection the bottle
//! makes arrives as a stream from its agent, and is asked of bottled's decider, as smolvm asked it
//! (see bottle/decider.py), before it goes anywhere: connected from here directly, handed to bottled,
//! which is then its far end, or refused. Names the bottle looks up are resolved here, by this
//! machine's own resolver, at the time; and the bottle's clock is set from this machine's.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::{Duration, Instant};

pub use bottle_agent_proto::net::PORT;
use bottle_agent_proto::net::{CONNECTED, REFUSED, Request, Resolved, Time};

/// The most of a connection's first bytes the decider is given, and how long they're waited for:
/// a protocol whose server speaks first gives none.
const FIRST_BYTES: usize = 8192;
const FIRST_WAIT: Duration = Duration::from_millis(100);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the decider gets to answer.
const DECIDE_TIMEOUT: Duration = Duration::from_secs(30);

/// Where flows are asked about, and as whom: the decider's address, and the token bottled gave the
/// bottle. Each launch is a machine of its own, with an identity minted for it.
#[derive(Clone)]
pub struct Decider {
    pub address: SocketAddr,
    pub token: [u8; 32],
    pub machine: [u8; 16],
}

/// What the decider said of a flow.
enum Decision {
    Direct,
    Redirect(TcpStream),
    Deny,
}

const MAGIC: &[u8] = b"SMOLMEG2";

impl Decider {
    fn decide(&self, to: SocketAddr, first: &[u8]) -> io::Result<Decision> {
        let mut conn = TcpStream::connect_timeout(&self.address, CONNECT_TIMEOUT)?;
        conn.set_read_timeout(Some(DECIDE_TIMEOUT))?;
        let mut prelude = Vec::with_capacity(96 + first.len());
        prelude.extend_from_slice(MAGIC);
        prelude.extend_from_slice(&self.token);
        prelude.extend_from_slice(&self.machine);
        prelude.extend_from_slice(&[0; 16]);
        match to.ip() {
            IpAddr::V4(ip) => {
                prelude.push(4);
                prelude.extend_from_slice(&to.port().to_be_bytes());
                prelude.extend_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                prelude.push(6);
                prelude.extend_from_slice(&to.port().to_be_bytes());
                prelude.extend_from_slice(&ip.octets());
            }
        }
        prelude.extend_from_slice(&(first.len() as u16).to_be_bytes());
        prelude.extend_from_slice(first);
        conn.write_all(&prelude)?;
        let mut answer = [0];
        conn.read_exact(&mut answer)?;
        conn.set_read_timeout(None)?;
        Ok(match answer[0] {
            0 => Decision::Direct,
            2 => Decision::Redirect(conn),
            _ => Decision::Deny,
        })
    }
}

/// Serves the bottle's network on `listener`, each stream on a thread of its own, until accepting
/// fails. Without a decider, every connection goes directly: for trying a machine out, not for
/// bottles.
pub fn serve(listener: UnixListener, decider: Option<Decider>) -> io::Result<()> {
    loop {
        let (conn, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let decider = decider.clone();
        thread::spawn(move || {
            if let Err(e) = handle(conn, decider.as_ref()) {
                // A connection reset or refused is the bottle's business, not this process's log.
                if !matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                ) {
                    eprintln!("bottle-cli: the bottle's network: {e}");
                }
            }
        });
    }
}

fn handle(mut conn: UnixStream, decider: Option<&Decider>) -> io::Result<()> {
    match Request::read(&mut conn)? {
        Request::Resolve(name) => Resolved(resolve(&name)).write(&mut conn),
        Request::Connect(to) => connect(conn, to, decider),
        Request::Time => Time(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default(),
        )
        .write(&mut conn),
    }
}

/// `name`'s addresses, IPv4 only: the bottle has no IPv6 route.
fn resolve(name: &str) -> Option<Vec<IpAddr>> {
    let addrs = (name, 0).to_socket_addrs().ok()?;
    let mut ips: Vec<IpAddr> = addrs.map(|a| a.ip()).filter(IpAddr::is_ipv4).collect();
    ips.dedup();
    Some(ips)
}

fn connect(mut conn: UnixStream, to: SocketAddr, decider: Option<&Decider>) -> io::Result<()> {
    let first = first_bytes(&mut conn)?;
    let decision = match decider {
        None => Decision::Direct,
        Some(decider) => decider.decide(to, &first).unwrap_or(Decision::Deny),
    };
    let far = match decision {
        Decision::Direct => match TcpStream::connect_timeout(&to, CONNECT_TIMEOUT) {
            Ok(mut far) => {
                far.write_all(&first)?;
                far
            }
            Err(_) => return refuse(conn),
        },
        // bottled has the first bytes, from the prelude: the rest follows.
        Decision::Redirect(far) => far,
        Decision::Deny => return refuse(conn),
    };
    conn.write_all(&[CONNECTED])?;
    splice(conn, far)
}

fn refuse(mut conn: UnixStream) -> io::Result<()> {
    conn.write_all(&[REFUSED])
}

/// What the bottle sends first: up to FIRST_BYTES, as long as it comes within FIRST_WAIT.
fn first_bytes(conn: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut first = vec![0; FIRST_BYTES];
    let mut got = 0;
    let deadline = Instant::now() + FIRST_WAIT;
    while got < FIRST_BYTES {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        conn.set_read_timeout(Some(left))?;
        match conn.read(&mut first[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    conn.set_read_timeout(None)?;
    first.truncate(got);
    Ok(first)
}

/// Passes bytes both ways until both have ended, each end's end passed on as a shutdown.
fn splice(conn: UnixStream, far: TcpStream) -> io::Result<()> {
    let (mut conn_r, mut far_w) = (conn.try_clone()?, far.try_clone()?);
    let up = thread::spawn(move || {
        let _ = io::copy(&mut conn_r, &mut far_w);
        let _ = far_w.shutdown(Shutdown::Write);
    });
    let (mut far_r, mut conn_w) = (far, conn);
    let _ = io::copy(&mut far_r, &mut conn_w);
    let _ = conn_w.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    #[test]
    fn a_connection_goes_directly_without_a_decider() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let to = server.local_addr().unwrap();
        thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            let mut got = [0; 5];
            s.read_exact(&mut got).unwrap();
            assert_eq!(&got, b"hello");
            s.write_all(b"world").unwrap();
        });
        let (mut agent, host) = pair();
        thread::spawn(move || handle(host, None));
        Request::Connect(to).write(&mut agent).unwrap();
        agent.write_all(b"hello").unwrap();
        let mut answer = [0; 6];
        agent.read_exact(&mut answer).unwrap();
        assert_eq!(&answer, b"\0world");
    }

    #[test]
    fn the_decider_is_asked_with_the_first_bytes_and_can_refuse() {
        let decider = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = decider.local_addr().unwrap();
        let asked = thread::spawn(move || {
            let (mut s, _) = decider.accept().unwrap();
            let mut fixed = [0; 8 + 32 + 32 + 1 + 2 + 4 + 2];
            s.read_exact(&mut fixed).unwrap();
            let mut first = vec![0; u16::from_be_bytes([fixed[79], fixed[80]]) as usize];
            s.read_exact(&mut first).unwrap();
            s.write_all(&[1]).unwrap();
            (fixed, first)
        });
        let d = Decider {
            address,
            token: [7; 32],
            machine: [9; 16],
        };
        let (mut agent, host) = pair();
        thread::spawn(move || handle(host, Some(&d)));
        Request::Connect("192.0.2.9:443".parse().unwrap())
            .write(&mut agent)
            .unwrap();
        agent.write_all(b"GET / HTTP/1.1\r\n").unwrap();
        let mut answer = [0];
        agent.read_exact(&mut answer).unwrap();
        assert_eq!(answer[0], REFUSED);
        let (fixed, first) = asked.join().unwrap();
        assert_eq!(&fixed[..8], MAGIC);
        assert_eq!(&fixed[8..40], &[7; 32]);
        assert_eq!(&fixed[40..56], &[9; 16]);
        assert_eq!(fixed[72], 4);
        assert_eq!(u16::from_be_bytes([fixed[73], fixed[74]]), 443);
        assert_eq!(&fixed[75..79], &[192, 0, 2, 9]);
        assert_eq!(first, b"GET / HTTP/1.1\r\n");
    }

    #[test]
    fn the_time_is_this_machines() {
        let (mut agent, host) = pair();
        thread::spawn(move || handle(host, None));
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        Request::Time.write(&mut agent).unwrap();
        let Time(now) = Time::read(&mut agent).unwrap();
        assert!(
            now >= before && now - before < Duration::from_secs(1),
            "{now:?}"
        );
    }

    #[test]
    fn names_resolve_here() {
        let (mut agent, host) = pair();
        thread::spawn(move || handle(host, None));
        Request::Resolve("localhost".into())
            .write(&mut agent)
            .unwrap();
        let Resolved(addrs) = Resolved::read(&mut agent).unwrap();
        assert!(addrs.unwrap().contains(&"127.0.0.1".parse().unwrap()));
    }
}
