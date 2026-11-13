//! The host's side of a bottle's network (see bottle_agent_proto::net): each connection the bottle
//! makes arrives as a stream from its agent, and is decided before it goes anywhere, by its
//! machine's network policy (see policy.rs): connected from here directly, answered here from the
//! cache (see cache.rs), given to the service its policy routes it to, or refused. Names the bottle
//! looks up are resolved here, by this machine's own resolver, at the time.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::cache;
use crate::policy::{Local, Policy};

pub use bottle_agent_proto::net::PORT;
use bottle_agent_proto::net::{CONNECTED, REFUSED, Request, Resolved};

/// The most of a connection's first bytes read at once to decide it, and how long they're waited
/// for, when its policy needs them: a protocol whose server speaks first sends none.
const FIRST_BYTES: usize = 8192;
const FIRST_WAIT: Duration = Duration::from_millis(100);
/// How long more of an opening whose name decides it is waited for, and the most of it read.
const OPENING_WAIT: Duration = Duration::from_secs(3);
const MAX_OPENING: usize = 5 + (1 << 14);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How a bottle's connections are decided: its machine's network policy (its network.json).
pub struct Network {
    policy: Policy,
    /// The machine's name: whose caching CA a cached or routed host's TLS is terminated with (see
    /// ca.rs), and who a service is told its connections are from.
    pub machine: Option<String>,
    cache: OnceLock<cache::Context>,
}

impl Network {
    pub fn new(policy: Policy, machine: Option<String>) -> Network {
        Network {
            policy,
            machine,
            cache: OnceLock::new(),
        }
    }

    /// What serving a cached or routed host needs, made the first time it's needed.
    fn cache(&self) -> &cache::Context {
        self.cache
            .get_or_init(|| cache::Context::new(self.machine.clone()))
    }
}

/// The most files the process carrying a bottle's network should have open: each connection the
/// bottle makes takes several here (libkrun's end of it, ours, the far end, their clones), and
/// macOS gives a process a shell starts 256.
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
/// fails. Without a network, every connection goes directly: for trying a machine out, not for
/// bottles.
pub fn serve(listener: UnixListener, network: Option<Network>) -> io::Result<()> {
    let network = network.map(Arc::new);
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
        let network = network.clone();
        thread::spawn(move || {
            if let Err(e) = handle(conn, network.as_deref()) {
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

fn handle(mut conn: UnixStream, network: Option<&Network>) -> io::Result<()> {
    match Request::read(&mut conn)? {
        Request::Resolve(name) => resolve(&name).write(&mut conn),
        Request::Connect(to) => connect(conn, to, network),
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

/// Decides a connection, then carries it as decided: as soon as it can be, by what's arrived of it.
/// One its address alone connects from here goes at once, before anything's read; else what the
/// bottle sends first is read as it comes (a routed one's, to see whether it's TLS), and the
/// opening, if a name in it may decide (a ClientHello, an HTTP request's head), until it's all
/// there.
fn connect(mut conn: UnixStream, to: SocketAddr, network: Option<&Network>) -> io::Result<()> {
    let ips = |name: &str| match resolve(name) {
        Resolved::Addresses(ips) => ips,
        _ => Vec::new(),
    };
    let Some(network) = network else {
        return direct(conn, to, &[]);
    };
    let policy = &network.policy;
    // Refused by its address alone, it may yet be allowed by a name it gives (allow_private).
    let mut first = Vec::new();
    let mut local = policy.decide(to, &first, ips);
    if local != Local::Direct {
        first = first_bytes(&mut conn)?;
        local = policy.decide(to, &first, ips);
    }
    if local == Local::Unread {
        read_opening(&mut conn, &mut first)?;
        local = policy.decide_opened(to, &first, ips, true);
    }
    match local {
        Local::Direct => direct(conn, to, &first),
        Local::Cache(i) => {
            let name = crate::policy::name_in(&first).unwrap_or_default();
            let plain = first.first() != Some(&0x16);
            let ctx = network.cache();
            if !plain && !ctx.terminates(&name) {
                eprintln!(
                    "bottle-cli: {name} is cached, but {} has no caching CA for it: not cached",
                    network.machine.as_deref().unwrap_or("this machine")
                );
                return direct(conn, to, &first);
            }
            conn.write_all(&[CONNECTED])?;
            let reachable = |host: &str| policy.reachable(host, ips);
            cache::serve(conn, first, &name, &policy.cache[i], plain, ctx, &reachable)
        }
        Local::Route(i) => route(conn, to, first, policy.routes[i].socket(), network),
        // Unread even after waiting for it: as it stands, it's refused.
        Local::Deny | Local::Unread => {
            let name = crate::policy::name_in(&first);
            eprintln!(
                "bottle-cli: refused {to}{}: not allowed",
                name.map_or(String::new(), |n| format!(" ({n})"))
            );
            refuse(conn)
        }
    }
}

/// Connects `to` from here, and passes the bottle's bytes, `first` first, both ways.
fn direct(mut conn: UnixStream, to: SocketAddr, first: &[u8]) -> io::Result<()> {
    let Ok(mut far) = TcpStream::connect_timeout(&to, CONNECT_TIMEOUT) else {
        return refuse(conn);
    };
    // What the bottle writes goes when it's written: its own TCP has decided when that is.
    let _ = far.set_nodelay(true);
    far.write_all(first)?;
    conn.write_all(&[CONNECTED])?;
    splice(conn, far)
}

/// Gives a routed connection to its service at `socket`: a PROXY protocol header first (see
/// proxy_header), then its bytes, its TLS unwrapped here, with the machine's caching CA, when it
/// opens with a ClientHello. Refused when the service isn't there.
fn route(
    mut conn: UnixStream,
    to: SocketAddr,
    first: Vec<u8>,
    socket: &std::path::Path,
    network: &Network,
) -> io::Result<()> {
    let name = crate::policy::name_in(&first);
    let tls = first.first() == Some(&0x16);
    let described = format!(
        "{to}{}",
        name.as_ref().map_or(String::new(), |n| format!(" ({n})"))
    );
    let config = if tls {
        let ctx = network.cache();
        match name.as_deref().filter(|n| ctx.terminates(n)) {
            Some(n) => Some(ctx.server_config(n)?),
            None => {
                eprintln!("bottle-cli: refused {described}: no certificate for it to be routed");
                return refuse(conn);
            }
        }
    } else {
        None
    };
    let mut service = match UnixStream::connect(socket) {
        Ok(service) => service,
        Err(e) => {
            eprintln!("bottle-cli: refused {described}: {}: {e}", socket.display());
            return refuse(conn);
        }
    };
    service.write_all(&proxy_header(
        to,
        name.as_deref(),
        tls,
        network.machine.as_deref(),
    ))?;
    conn.write_all(&[CONNECTED])?;
    match config {
        None => {
            service.write_all(&first)?;
            splice(conn, service)
        }
        Some(config) => unwrapped(conn, first, config, service),
    }
}

/// The PROXY protocol's version 2 signature (haproxy's proxy-protocol.txt).
const PROXY_SIGNATURE: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";
/// Its fields: the name the bottle asked for (SNI, or an HTTP request's Host); that it spoke TLS,
/// unwrapped here; and, in the range kept for applications, the machine's name.
const PP2_TYPE_AUTHORITY: u8 = 0x02;
const PP2_TYPE_SSL: u8 = 0x20;
const PP2_CLIENT_SSL: u8 = 0x01;
pub const PP2_TYPE_MACHINE: u8 = 0xE0;

/// What a service reads before a routed connection's bytes: a PROXY protocol v2 header, its
/// destination the address and port the bottle connected to (its source unknown: 0.0.0.0:0),
/// with the name it asked for, whether it spoke TLS, and the machine it's from.
pub fn proxy_header(
    to: SocketAddr,
    name: Option<&str>,
    tls: bool,
    machine: Option<&str>,
) -> Vec<u8> {
    let mut body = Vec::new();
    let family = match to.ip() {
        IpAddr::V4(ip) => {
            body.extend_from_slice(&[0; 4]);
            body.extend_from_slice(&ip.octets());
            0x11 // TCP over IPv4
        }
        IpAddr::V6(ip) => {
            body.extend_from_slice(&[0; 16]);
            body.extend_from_slice(&ip.octets());
            0x21 // TCP over IPv6
        }
    };
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(&to.port().to_be_bytes());
    let mut tlv = |kind: u8, value: &[u8]| {
        body.push(kind);
        body.extend_from_slice(&(value.len() as u16).to_be_bytes());
        body.extend_from_slice(value);
    };
    if let Some(name) = name {
        tlv(PP2_TYPE_AUTHORITY, name.as_bytes());
    }
    if tls {
        // The client's bits, then "verify": 0, as no client certificate was asked for.
        tlv(PP2_TYPE_SSL, &[PP2_CLIENT_SSL, 0, 0, 0, 0]);
    }
    if let Some(machine) = machine {
        tlv(PP2_TYPE_MACHINE, machine.as_bytes());
    }
    let mut header = PROXY_SIGNATURE.to_vec();
    header.push(0x21); // version 2, PROXY
    header.push(family);
    header.extend_from_slice(&(body.len() as u16).to_be_bytes());
    header.extend(body);
    header
}

/// Passes a bottle's TLS connection, which opened with `first`, to `service` unwrapped, and the
/// service's answers back wrapped, until both have ended.
fn unwrapped(
    guest: UnixStream,
    first: Vec<u8>,
    config: Arc<rustls::ServerConfig>,
    service: UnixStream,
) -> io::Result<()> {
    let tls = Arc::new(Mutex::new(
        rustls::ServerConnection::new(config).map_err(io::Error::other)?,
    ));
    // Only the TLS state is shared, never held while reading or writing either side.
    let pending = |tls: &mut rustls::ServerConnection| -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        while tls.wants_write() {
            tls.write_tls(&mut out)?;
        }
        Ok(out)
    };
    let (mut guest_r, guest_w) = (guest.try_clone()?, guest);
    let (mut service_r, mut service_w) = (service.try_clone()?, service);
    let up = {
        let (tls, mut guest_w) = (Arc::clone(&tls), guest_w.try_clone()?);
        thread::spawn(move || -> io::Result<()> {
            let mut received = first;
            let mut buf = vec![0; 16 * 1024];
            loop {
                if received.is_empty() {
                    let n = guest_r.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    received.extend_from_slice(&buf[..n]);
                }
                let (plain, out, closed) = {
                    let mut tls = tls.lock().unwrap_or_else(|e| e.into_inner());
                    let (mut plain, mut closed) = (Vec::new(), false);
                    let mut rest = &received[..];
                    while !rest.is_empty() {
                        tls.read_tls(&mut rest)?;
                        let state = tls.process_new_packets().map_err(io::Error::other)?;
                        let mut chunk = vec![0; state.plaintext_bytes_to_read()];
                        tls.reader().read_exact(&mut chunk)?;
                        plain.extend(chunk);
                        closed |= state.peer_has_closed();
                    }
                    (plain, pending(&mut tls)?, closed)
                };
                received.clear();
                guest_w.write_all(&out)?;
                service_w.write_all(&plain)?;
                if closed {
                    break;
                }
            }
            let _ = service_w.shutdown(Shutdown::Write);
            Ok(())
        })
    };
    let mut guest_w = guest_w;
    let mut buf = vec![0; 16 * 1024];
    loop {
        let n = service_r.read(&mut buf).unwrap_or(0);
        let out = {
            let mut tls = tls.lock().unwrap_or_else(|e| e.into_inner());
            if n == 0 {
                tls.send_close_notify();
            } else {
                tls.writer().write_all(&buf[..n])?;
            }
            pending(&mut tls)?
        };
        if guest_w.write_all(&out).is_err() || n == 0 {
            break;
        }
    }
    let _ = guest_w.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

/// Reads more of an opening whose name may decide it: the rest of a TLS record, or the first of
/// one that hadn't come, for up to OPENING_WAIT.
fn read_opening(conn: &mut UnixStream, first: &mut Vec<u8>) -> io::Result<()> {
    let deadline = Instant::now() + OPENING_WAIT;
    let complete = |data: &[u8]| {
        data.len() >= 5 && data.len() >= 5 + usize::from(u16::from_be_bytes([data[3], data[4]]))
    };
    let mut buf = vec![0; FIRST_BYTES];
    while !(first.first() == Some(&0x16) && complete(first))
        && !(first.first().is_some_and(|&b| b != 0x16)
            && first.windows(4).any(|w| w == b"\r\n\r\n"))
        && first.len() < MAX_OPENING
    {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        conn.set_read_timeout(Some(left))?;
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => first.extend_from_slice(&buf[..n]),
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
    Ok(())
}

fn refuse(mut conn: UnixStream) -> io::Result<()> {
    conn.write_all(&[REFUSED])
}

/// What the bottle sends first, as soon as some of it comes (up to FIRST_BYTES of it), if it comes
/// within FIRST_WAIT; nothing, if not. The rest of an opening, if it's needed, is read_opening's.
fn first_bytes(conn: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut first = vec![0; FIRST_BYTES];
    let mut got = 0;
    conn.set_read_timeout(Some(FIRST_WAIT))?;
    loop {
        match conn.read(&mut first) {
            Ok(n) => {
                got = n;
                break;
            }
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
fn splice<Far: Stream>(conn: UnixStream, far: Far) -> io::Result<()> {
    let (mut conn_r, mut far_w) = (conn.try_clone()?, far.clone_stream()?);
    let up = thread::spawn(move || {
        let _ = io::copy(&mut conn_r, &mut far_w);
        let _ = far_w.shut_write();
    });
    let (mut far_r, mut conn_w) = (far, conn);
    let _ = io::copy(&mut far_r, &mut conn_w);
    let _ = conn_w.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

/// A connection's far end: a host's, or a service's.
trait Stream: Read + Write + Send + Sized + 'static {
    fn clone_stream(&self) -> io::Result<Self>;
    fn shut_write(&self) -> io::Result<()>;
}

impl Stream for TcpStream {
    fn clone_stream(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn shut_write(&self) -> io::Result<()> {
        self.shutdown(Shutdown::Write)
    }
}

impl Stream for UnixStream {
    fn clone_stream(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn shut_write(&self) -> io::Result<()> {
        self.shutdown(Shutdown::Write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    #[test]
    fn a_connection_goes_directly_without_a_network() {
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
    fn what_comes_first_is_read_as_soon_as_it_comes() {
        let (mut agent, mut host) = pair();
        agent.write_all(b"a ClientHello").unwrap();
        let start = Instant::now();
        assert_eq!(first_bytes(&mut host).unwrap(), b"a ClientHello");
        assert!(start.elapsed() < FIRST_WAIT / 2, "{:?}", start.elapsed());
        // Nothing, from a protocol whose server speaks first: waited for, but not forever.
        let start = Instant::now();
        assert_eq!(first_bytes(&mut host).unwrap(), b"");
        assert!(start.elapsed() >= FIRST_WAIT);
    }

    #[test]
    fn a_direct_connection_is_made_before_the_bottle_sends_anything() {
        // Its server speaks first, so the bottle sends nothing until it has.
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let to = server.local_addr().unwrap();
        thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            s.write_all(b"SSH-2.0-far").unwrap();
            thread::sleep(Duration::from_secs(1));
        });
        let (mut agent, host) = pair();
        thread::spawn(move || handle(host, None));
        let start = Instant::now();
        Request::Connect(to).write(&mut agent).unwrap();
        let mut answer = [0; 12];
        agent.read_exact(&mut answer).unwrap();
        assert_eq!(&answer, b"\0SSH-2.0-far");
        assert!(start.elapsed() < FIRST_WAIT / 2, "{:?}", start.elapsed());
    }

    #[test]
    fn a_cached_hosts_connection_is_answered_here() {
        crate::ca::in_home(|home| {
            let kept = home.join("cache/http/deb.example/dists/InRelease");
            std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
            std::fs::write(&kept, "release").unwrap();
            let policy = r#"{"cache": [{"host": "deb.example", "ports": [80]}]}"#;
            let d = Network::new(serde_json::from_str(policy).unwrap(), Some("m1".into()));
            let (mut agent, host) = pair();
            thread::spawn(move || handle(host, Some(&d)));
            Request::Connect("93.184.216.34:80".parse().unwrap())
                .write(&mut agent)
                .unwrap();
            agent
                .write_all(b"GET /dists/InRelease HTTP/1.1\r\nHost: deb.example\r\nConnection: close\r\n\r\n")
                .unwrap();
            let mut answer = String::new();
            agent.read_to_string(&mut answer).unwrap();
            assert!(answer.starts_with("\0HTTP/1.1 200 OK\r\n"), "{answer:?}");
            assert!(answer.ends_with("\r\n\r\nrelease"), "{answer:?}");
        });
    }

    /// What a test service read of a connection: its destination, its PROXY fields, its bytes.
    type Served = (SocketAddr, Vec<(u8, Vec<u8>)>, Vec<u8>);

    /// A service at a socket of its own: what it read of each connection's PROXY header (the
    /// destination, and its fields by type) and of its bytes, up to an empty line, answered with
    /// `reply`.
    fn service(
        dir: &std::path::Path,
        reply: &'static [u8],
    ) -> (std::path::PathBuf, std::sync::mpsc::Receiver<Served>) {
        std::fs::create_dir_all(dir).unwrap();
        let socket = dir.join("service.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let mut conn = conn.unwrap();
                let mut head = [0; 16];
                conn.read_exact(&mut head).unwrap();
                assert_eq!(&head[..12], PROXY_SIGNATURE);
                assert_eq!((head[12], head[13]), (0x21, 0x11));
                let mut body = vec![0; u16::from_be_bytes([head[14], head[15]]) as usize];
                conn.read_exact(&mut body).unwrap();
                let to = SocketAddr::new(
                    IpAddr::from(<[u8; 4]>::try_from(&body[4..8]).unwrap()),
                    u16::from_be_bytes([body[10], body[11]]),
                );
                let (mut fields, mut at) = (Vec::new(), 12);
                while at < body.len() {
                    let len = u16::from_be_bytes([body[at + 1], body[at + 2]]) as usize;
                    fields.push((body[at], body[at + 3..at + 3 + len].to_vec()));
                    at += 3 + len;
                }
                let mut bytes = Vec::new();
                let mut byte = [0];
                while !bytes.ends_with(b"\r\n\r\n") && conn.read(&mut byte).unwrap() == 1 {
                    bytes.push(byte[0]);
                }
                conn.write_all(reply).unwrap();
                tx.send((to, fields, bytes)).unwrap();
            }
        });
        (socket, rx)
    }

    #[test]
    fn a_routed_connection_goes_to_its_service_after_a_proxy_header() {
        let dir = std::env::temp_dir().join(format!("bottle-route-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (socket, served) = service(&dir, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi");
        let policy = format!(
            r#"{{"routes": [{{"address": "192.0.2.1", "port": 80, "socket": "{}"}},
                            {{"address": "192.0.2.2", "port": 80, "socket": "{}/none.sock"}}]}}"#,
            socket.display(),
            dir.display()
        );
        let network = Arc::new(Network::new(
            serde_json::from_str(&policy).unwrap(),
            Some("gpu".into()),
        ));
        let connect = |to: &str, first: &[u8]| {
            let (mut agent, host) = pair();
            let network = Arc::clone(&network);
            thread::spawn(move || handle(host, Some(&network)));
            Request::Connect(to.parse().unwrap())
                .write(&mut agent)
                .unwrap();
            agent.write_all(first).unwrap();
            agent
        };
        let request = b"GET /git/x HTTP/1.1\r\nHost: bottle.host\r\n\r\n";
        let mut agent = connect("192.0.2.1:80", request);
        let mut answer = Vec::new();
        agent.read_to_end(&mut answer).unwrap();
        assert_eq!(answer[0], CONNECTED);
        assert!(answer.ends_with(b"\r\n\r\nhi"));
        let (to, fields, bytes) = served.recv().unwrap();
        assert_eq!(to, "192.0.2.1:80".parse().unwrap());
        assert_eq!(
            fields,
            [
                (PP2_TYPE_AUTHORITY, b"bottle.host".to_vec()),
                (PP2_TYPE_MACHINE, b"gpu".to_vec())
            ]
        );
        assert_eq!(bytes, request);
        // A service that isn't there: refused.
        let mut agent = connect("192.0.2.2:80", request);
        let mut answer = [0];
        agent.read_exact(&mut answer).unwrap();
        assert_eq!(answer[0], REFUSED);
        // What the machine's policy can't decide (IPv6, which the bottle has no route to): refused.
        let mut agent = connect("[2001:db8::1]:443", b"");
        agent.read_exact(&mut answer).unwrap();
        assert_eq!(answer[0], REFUSED);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_routed_tls_connection_reaches_its_service_unwrapped() {
        crate::ca::in_home(|home| {
            let (socket, served) = service(home, b"HTTP/1.1 204 No Content\r\n\r\n");
            let policy = format!(
                r#"{{"routes": [{{"host": "api.example", "port": 443, "socket": "{}"}}]}}"#,
                socket.display()
            );
            let policy: Policy = serde_json::from_str(&policy).unwrap();
            let pem = crate::ca::certificate("gpu", &policy.terminated_hosts())
                .unwrap()
                .unwrap();
            let network = Network::new(policy, Some("gpu".into()));
            let (mut agent, host) = pair();
            thread::spawn(move || handle(host, Some(&network)));
            Request::Connect("1.2.3.4:443".parse().unwrap())
                .write(&mut agent)
                .unwrap();
            // A client trusting the machine's CA alone: its ClientHello first, which decides it.
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(rustls::pki_types::pem::PemObject::from_pem_slice(pem.as_bytes()).unwrap())
                .unwrap();
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let mut client =
                rustls::ClientConnection::new(Arc::new(config), "api.example".try_into().unwrap())
                    .unwrap();
            client.write_tls(&mut agent).unwrap();
            let mut answer = [0];
            agent.read_exact(&mut answer).unwrap();
            assert_eq!(answer[0], CONNECTED);
            let mut tls = rustls::StreamOwned::new(client, agent);
            tls.write_all(b"GET /v1/models HTTP/1.1\r\nHost: api.example\r\n\r\n")
                .unwrap();
            let mut reply = Vec::new();
            let _ = tls.read_to_end(&mut reply);
            assert_eq!(reply, b"HTTP/1.1 204 No Content\r\n\r\n");
            let (to, fields, bytes) = served.recv().unwrap();
            assert_eq!(to, "1.2.3.4:443".parse().unwrap());
            assert_eq!(
                fields,
                [
                    (PP2_TYPE_AUTHORITY, b"api.example".to_vec()),
                    (PP2_TYPE_SSL, vec![PP2_CLIENT_SSL, 0, 0, 0, 0]),
                    (PP2_TYPE_MACHINE, b"gpu".to_vec())
                ]
            );
            assert_eq!(
                bytes,
                b"GET /v1/models HTTP/1.1\r\nHost: api.example\r\n\r\n"
            );
        });
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
