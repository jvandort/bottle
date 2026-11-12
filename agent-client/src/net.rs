//! The host's side of a bottle's network (see bottle_agent_proto::net): each connection the bottle
//! makes arrives as a stream from its agent, and is decided before it goes anywhere: connected from
//! here directly, handed to bottled, which is then its far end, or refused. Decided here, by the
//! policy bottled gives once (see policy.rs), unless it's one bottled must see; asked of bottled's
//! decider (see bottle/decider.py), as smolvm asked it, when it is, or when bottled hasn't given a
//! policy. Names the bottle looks up are resolved here, by this machine's own resolver, at the time.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::policy::{Local, Policy};

pub use bottle_agent_proto::net::PORT;
use bottle_agent_proto::net::{CONNECTED, REFUSED, Request, Resolved};

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
    policy: PolicyState,
}

/// The policy bottled gave, once it has; when it was last asked for.
type PolicyState = Arc<Mutex<(Option<Arc<Policy>>, Option<Instant>)>>;

/// What the decider said of a flow.
enum Decision {
    Direct,
    Redirect(TcpStream),
    Deny,
}

const MAGIC: &[u8] = b"SMOLMEG2";
const POLICY_MAGIC: &[u8] = b"BOTTLPOL";
/// How long bottled gets to give its policy (one that doesn't know the question waits for more,
/// then hangs up), and how long after it didn't before it's asked again.
const POLICY_WAIT: Duration = Duration::from_secs(3);
const POLICY_RETRY: Duration = Duration::from_secs(30);

impl Decider {
    pub fn new(address: SocketAddr, token: [u8; 32], machine: [u8; 16]) -> Decider {
        Decider {
            address,
            token,
            machine,
            policy: Default::default(),
        }
    }

    /// The policy bottled gives, asked for the first time it's needed, and again after
    /// POLICY_RETRY if it wasn't given; None meanwhile, when every connection is asked of bottled.
    fn policy(&self) -> Option<Arc<Policy>> {
        let mut state = self.policy.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(policy) = &state.0 {
            return Some(Arc::clone(policy));
        }
        if state.1.is_some_and(|asked| asked.elapsed() < POLICY_RETRY) {
            return None;
        }
        state.1 = Some(Instant::now());
        match self.ask_policy() {
            Ok(policy) => {
                let policy = Arc::new(policy);
                state.0 = Some(Arc::clone(&policy));
                Some(policy)
            }
            Err(e) => {
                eprintln!(
                    "bottle-cli: no policy from bottled ({e}): asking it of every connection"
                );
                None
            }
        }
    }

    fn ask_policy(&self) -> io::Result<Policy> {
        let mut conn = TcpStream::connect_timeout(&self.address, CONNECT_TIMEOUT)?;
        conn.set_read_timeout(Some(POLICY_WAIT))?;
        conn.write_all(&[POLICY_MAGIC, &self.token, &self.machine].concat())?;
        let mut length = [0; 4];
        conn.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 {
            return Err(io::Error::other("refused"));
        }
        let mut json = vec![0; length];
        conn.read_exact(&mut json)?;
        serde_json::from_slice(&json).map_err(io::Error::other)
    }

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

/// The most files the process carrying a bottle's network should have open: each connection the
/// bottle makes takes several here (libkrun's end of it, ours, the far end, their clones, the
/// decider's), and macOS gives a process a shell starts 256.
const OPEN_FILES: u64 = 1 << 20;

/// Raises this process's limit on open files toward OPEN_FILES, as far as the system lets it:
/// macOS refuses more than kern.maxfilesperproc whatever the hard limit says, so it's halved until
/// it's taken. The limit it has after.
pub fn raise_open_files() -> u64 {
    // SAFETY: getrlimit writes an rlimit; setrlimit reads one.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return 0;
        }
        let soft = limit.rlim_cur as u64;
        let mut target = OPEN_FILES.min(limit.rlim_max as u64);
        while target > soft {
            limit.rlim_cur = target as libc::rlim_t;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                return target;
            }
            target /= 2;
        }
        soft
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
            // Out of descriptors or memory for now: it passes. Ending would leave the bottle with
            // no network until it's started again.
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EMFILE | libc::ENFILE | libc::ENOMEM | libc::ENOBUFS)
                ) =>
            {
                eprintln!("bottle-cli: the bottle's network: {e}; waiting");
                thread::sleep(Duration::from_millis(50));
                continue;
            }
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
        Request::Resolve(name) => resolve(&name).write(&mut conn),
        Request::Connect(to) => connect(conn, to, decider),
    }
}

/// `name`'s addresses, IPv4 only: the bottle has no IPv6 route. A lookup that fails for now is
/// said to have failed, not that there's no such name, so the bottle's resolver asks again rather
/// than giving up on the name (and Java caching that).
fn resolve(name: &str) -> Resolved {
    let Ok(c_name) = std::ffi::CString::new(name) else {
        return Resolved::NoSuchName;
    };
    // SAFETY: hints zeroed but for the family and socket type; getaddrinfo writes a list we free.
    unsafe {
        let mut hints: libc::addrinfo = std::mem::zeroed();
        hints.ai_family = libc::AF_INET;
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut list = std::ptr::null_mut();
        match libc::getaddrinfo(c_name.as_ptr(), std::ptr::null(), &hints, &mut list) {
            0 => {}
            libc::EAI_NONAME => return Resolved::NoSuchName,
            // The name is there, with no address of this kind.
            libc::EAI_NODATA => return Resolved::Addresses(Vec::new()),
            _ => return Resolved::Failed,
        }
        let mut ips = Vec::new();
        let mut entry = list;
        while !entry.is_null() {
            let addr = (*entry).ai_addr;
            if !addr.is_null() && i32::from((*addr).sa_family) == libc::AF_INET {
                let v4 = &*(addr as *const libc::sockaddr_in);
                let ip = IpAddr::from(u32::from_be(v4.sin_addr.s_addr).to_be_bytes());
                if !ips.contains(&ip) {
                    ips.push(ip);
                }
            }
            entry = (*entry).ai_next;
        }
        libc::freeaddrinfo(list);
        Resolved::Addresses(ips)
    }
}

fn connect(mut conn: UnixStream, to: SocketAddr, decider: Option<&Decider>) -> io::Result<()> {
    let first = first_bytes(&mut conn)?;
    let decision = match decider {
        None => Decision::Direct,
        Some(decider) => match decider.policy().map(|policy| {
            policy.decide(to, &first, |name| match resolve(name) {
                Resolved::Addresses(ips) => ips,
                _ => Vec::new(),
            })
        }) {
            Some(Local::Direct) => Decision::Direct,
            Some(Local::Deny) => {
                let name = crate::policy::name_in(&first);
                eprintln!(
                    "bottle-cli: refused {to}{}: not allowed",
                    name.map_or(String::new(), |n| format!(" ({n})"))
                );
                Decision::Deny
            }
            Some(Local::Hand) | None => decider.decide(to, &first).unwrap_or(Decision::Deny),
        },
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
            // Asked for a policy first: none is given, so the flow is asked about.
            let (mut s, _) = decider.accept().unwrap();
            let mut question = [0; 8 + 32 + 16];
            s.read_exact(&mut question).unwrap();
            assert_eq!(&question[..8], POLICY_MAGIC);
            s.write_all(&[0; 4]).unwrap();
            let (mut s, _) = decider.accept().unwrap();
            let mut fixed = [0; 8 + 32 + 32 + 1 + 2 + 4 + 2];
            s.read_exact(&mut fixed).unwrap();
            let mut first = vec![0; u16::from_be_bytes([fixed[79], fixed[80]]) as usize];
            s.read_exact(&mut first).unwrap();
            s.write_all(&[1]).unwrap();
            (fixed, first)
        });
        let d = Decider::new(address, [7; 32], [9; 16]);
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
    fn with_a_policy_only_what_bottled_must_see_is_asked_of_it() {
        let far = TcpListener::bind("127.0.0.1:0").unwrap();
        let decider = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = decider.local_addr().unwrap();
        let asked = thread::spawn(move || {
            let (mut s, _) = decider.accept().unwrap();
            let mut question = [0; 8 + 32 + 16];
            s.read_exact(&mut question).unwrap();
            assert_eq!(
                (&question[..8], &question[8..40]),
                (POLICY_MAGIC, &[7; 32][..])
            );
            let policy =
                br#"{"allow_private": [], "hand": [{"name": "handed.example", "port": 443}]}"#;
            s.write_all(&(policy.len() as u32).to_be_bytes()).unwrap();
            s.write_all(policy).unwrap();
            // Then only the handed flow is asked about.
            let (mut s, _) = decider.accept().unwrap();
            let mut magic = [0; 8];
            s.read_exact(&mut magic).unwrap();
            s.write_all(&[1]).unwrap();
            magic
        });
        let d = Decider::new(address, [7; 32], [9; 16]);
        let connect = |to: SocketAddr, first: &[u8]| {
            let (mut agent, host) = pair();
            let d = d.clone();
            thread::spawn(move || handle(host, Some(&d)));
            Request::Connect(to).write(&mut agent).unwrap();
            agent.write_all(first).unwrap();
            let mut answer = [0];
            agent.read_exact(&mut answer).unwrap();
            answer[0]
        };
        // Private, by no name allowed private: refused here. (The loopback stands in for the internet.)
        assert_eq!(
            connect(
                "10.0.0.1:80".parse().unwrap(),
                b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"
            ),
            REFUSED
        );
        let handed = b"GET / HTTP/1.1\r\nHost: handed.example\r\n\r\n";
        assert_eq!(
            connect("93.184.216.34:443".parse().unwrap(), handed),
            REFUSED
        );
        assert_eq!(&asked.join().unwrap(), MAGIC);
        drop(far);
    }

    #[test]
    fn the_limit_on_open_files_is_raised_as_far_as_it_goes() {
        let raised = raise_open_files();
        let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
        // SAFETY: getrlimit writes an rlimit.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        assert_eq!(limit.rlim_cur as u64, raised);
        assert!(raised >= 1024 && raised <= OPEN_FILES.max(limit.rlim_max as u64));
    }

    #[test]
    fn names_resolve_here() {
        let (mut agent, host) = pair();
        thread::spawn(move || handle(host, None));
        Request::Resolve("localhost".into())
            .write(&mut agent)
            .unwrap();
        let Resolved::Addresses(addrs) = Resolved::read(&mut agent).unwrap() else {
            panic!("localhost didn't resolve");
        };
        assert!(addrs.contains(&"127.0.0.1".parse().unwrap()));
        assert!(addrs.iter().all(IpAddr::is_ipv4));
    }

    #[test]
    fn a_name_that_isnt_there_is_said_to_be_so() {
        assert_eq!(resolve("no-such-name.invalid"), Resolved::NoSuchName);
    }
}
