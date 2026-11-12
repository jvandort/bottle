//! The bottle's network: there's no network device, only a dummy interface with the bottle's
//! address and its default route, and an nftables rule sending every TCP connection that doesn't
//! stay in the bottle to the agent, which carries each to the host, over vsock, where it's decided
//! (see bottle_agent_proto::net). Names are looked up through the host too: the agent answers DNS
//! on 127.0.0.53, which /etc/resolv.conf names. Nothing else leaves: there's nowhere for it to go.
//! In a VM, the clock is set from the host's too.
//!
//! All set up through netlink, the kernel's own interface to it: an image needs no tools for it.

use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use bottle_agent_proto::net::{CONNECTED, PORT, Request, Resolved, Time};

use crate::socket::Socket;

/// Where the agent takes the connections nftables sends it.
const LISTEN: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1);
/// Where it answers DNS.
const DNS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 53), 53);
const INTERFACE: &str = "bottle0";
/// Threads that only pass bytes on need little stack.
const STACK: usize = 64 * 1024;
/// How often a VM's clock is set from the host's.
const CLOCK_EVERY: Duration = Duration::from_secs(60);

/// The bottle's network, as the host gives it (see init's Config).
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The bottle's address, and its prefix's length: "100.96.0.2/30".
    pub address: String,
    /// How to reach the host: "vsock", or, out of a VM, "unix:PATH".
    pub host: String,
}

/// Brings loopback up: everything has it, network or not.
pub fn loopback_up() -> io::Result<()> {
    link_up(&mut Netlink::open(libc::NETLINK_ROUTE)?, index("lo")?)
}

/// Sets the network up, and serves it, on threads of its own, for as long as this process runs.
pub fn start(config: &Config) -> io::Result<()> {
    let (address, prefix) = parse_address(&config.address)?;
    let host = config.host.clone();
    let mut route = Netlink::open(libc::NETLINK_ROUTE)?;
    link_up(&mut route, index("lo")?)?;
    create_dummy(&mut route, INTERFACE)?;
    let interface = index(INTERFACE)?;
    link_up(&mut route, interface)?;
    add_address(&mut route, interface, address, prefix)?;
    add_default_route(&mut route, interface)?;
    let listener = TcpListener::bind(LISTEN)?;
    redirect_tcp(
        &mut Netlink::open(libc::NETLINK_NETFILTER)?,
        index("lo")?,
        LISTEN.port(),
    )?;
    let dns = UdpSocket::bind(DNS)?;
    let _ = fs::remove_file("/etc/resolv.conf");
    fs::create_dir_all("/etc")?;
    fs::write("/etc/resolv.conf", format!("nameserver {}\n", DNS.ip()))?;

    if crate::BOOTED.load(std::sync::atomic::Ordering::Relaxed) {
        // Before anything runs, then every so often: the RTC the kernel started the clock from
        // counts whole seconds, so it starts behind the host's, which mints the certificates the
        // bottle checks, and nothing else keeps it right. Out of a VM, the clock is the host's.
        let _ = set_clock(&host);
        let h = host.clone();
        spawn(move || {
            loop {
                thread::sleep(CLOCK_EVERY);
                let _ = set_clock(&h);
            }
        });
    }
    let h = host.clone();
    spawn(move || {
        for conn in listener.incoming().flatten() {
            let h = h.clone();
            spawn(move || {
                let _ = forward(conn, &h);
            });
        }
    });
    spawn(move || {
        let mut buf = [0; 512];
        while let Ok((n, from)) = dns.recv_from(&mut buf) {
            let (query, host, dns) = (buf[..n].to_vec(), host.clone(), dns.try_clone());
            spawn(move || {
                if let (Some(answer), Ok(dns)) = (answer(&query, &host), dns) {
                    let _ = dns.send_to(&answer, from);
                }
            });
        }
    });
    Ok(())
}

/// Sets this machine's clock to the host's, as it was half the asking's round trip ago.
fn set_clock(how: &str) -> io::Result<()> {
    let asked = Instant::now();
    let mut host = Host::dial(how)?;
    Request::Time.write(&mut host)?;
    let Time(then) = Time::read(&mut host)?;
    let now = then + asked.elapsed() / 2;
    let time = libc::timespec {
        tv_sec: now.as_secs() as libc::time_t,
        tv_nsec: now.subsec_nanos() as libc::c_long,
    };
    // SAFETY: clock_settime reads a timespec.
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn spawn(f: impl FnOnce() + Send + 'static) {
    let _ = thread::Builder::new().stack_size(STACK).spawn(f);
}

/// A stream to the host.
enum Host {
    Vsock(Socket),
    Unix(UnixStream),
}

impl Host {
    fn dial(how: &str) -> io::Result<Host> {
        match how.strip_prefix("unix:") {
            Some(path) => Ok(Host::Unix(UnixStream::connect(path)?)),
            None => Ok(Host::Vsock(Socket::connect_vsock(
                libc::VMADDR_CID_HOST,
                PORT,
            )?)),
        }
    }

    fn try_clone(&self) -> io::Result<Host> {
        Ok(match self {
            Host::Vsock(s) => Host::Vsock(s.try_clone()?),
            Host::Unix(s) => Host::Unix(s.try_clone()?),
        })
    }

    fn shutdown_write(&self) {
        match self {
            Host::Vsock(s) => s.shutdown_write(),
            Host::Unix(s) => {
                let _ = s.shutdown(Shutdown::Write);
            }
        }
    }
}

impl Read for Host {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Host::Vsock(s) => s.read(buf),
            Host::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Host {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Host::Vsock(s) => s.write(buf),
            Host::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Carries one redirected connection to the host: where it was going, then everything both ways.
/// Refused, the connection is reset, as a refused one would be.
fn forward(conn: TcpStream, how: &str) -> io::Result<()> {
    let to = original_destination(&conn)?;
    let mut host = match Host::dial(how) {
        Ok(host) => host,
        Err(e) => {
            reset(&conn);
            return Err(e);
        }
    };
    Request::Connect(SocketAddr::V4(to)).write(&mut host)?;
    let (mut from_bottle, mut to_host) = (conn.try_clone()?, host.try_clone()?);
    let up = thread::Builder::new().stack_size(STACK).spawn(move || {
        let _ = io::copy(&mut from_bottle, &mut to_host);
        to_host.shutdown_write();
    })?;
    let mut answer = [0];
    if host.read_exact(&mut answer).is_err() || answer[0] != CONNECTED {
        reset(&conn);
        // Reading ended, which sends nothing (Write would send a FIN, a close, not a reset), so
        // the copy up ends and lets go of its descriptor; closing the last one resets.
        let _ = conn.shutdown(Shutdown::Read);
        let _ = up.join();
        return Ok(());
    }
    let mut to_bottle = conn.try_clone()?;
    let _ = io::copy(&mut host, &mut to_bottle);
    let _ = to_bottle.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

/// Makes closing `conn` reset it.
fn reset(conn: &TcpStream) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: setsockopt with a linger struct of its size.
    unsafe {
        libc::setsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&linger as *const libc::linger).cast(),
            size_of::<libc::linger>() as libc::socklen_t,
        );
    }
}

/// Where a connection nftables redirected was going: conntrack remembers.
fn original_destination(conn: &TcpStream) -> io::Result<SocketAddrV4> {
    // SAFETY: getsockopt SO_ORIGINAL_DST writes a sockaddr_in, at most its size.
    unsafe {
        let mut addr: libc::sockaddr_in = zeroed();
        let mut len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
        if libc::getsockopt(
            conn.as_raw_fd(),
            libc::SOL_IP,
            libc::SO_ORIGINAL_DST,
            (&mut addr as *mut libc::sockaddr_in).cast(),
            &mut len,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(SocketAddrV4::new(
            Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)),
            u16::from_be(addr.sin_port),
        ))
    }
}

/// The answer to the DNS query `query`, the host asked for an A record's addresses; AAAA and the
/// rest have none (the bottle has no IPv6 route). None for what isn't a query.
fn answer(query: &[u8], how: &str) -> Option<Vec<u8>> {
    if query.len() < 12 || query[2] & 0x80 != 0 || u16::from_be_bytes([query[4], query[5]]) != 1 {
        return None;
    }
    // The question: labels to a zero, then its type and class.
    let mut i = 12;
    let mut labels = Vec::new();
    loop {
        let len = *query.get(i)? as usize;
        i += 1;
        if len == 0 {
            break;
        }
        if len > 63 {
            return None;
        }
        labels.push(String::from_utf8_lossy(query.get(i..i + len)?).into_owned());
        i += len;
    }
    let qtype = u16::from_be_bytes([*query.get(i)?, *query.get(i + 1)?]);
    let question_end = i + 4;
    query.get(..question_end)?;
    let name = labels.join(".");

    let (rcode, addrs) = if qtype == 1 {
        let resolved = Host::dial(how).and_then(|mut host| {
            Request::Resolve(name).write(&mut host)?;
            Resolved::read(&mut host)
        });
        match resolved {
            Ok(Resolved(Some(addrs))) => (0, addrs),
            Ok(Resolved(None)) => (3, Vec::new()),
            Err(_) => (2, Vec::new()),
        }
    } else {
        (0, Vec::new())
    };
    let v4: Vec<Ipv4Addr> = addrs
        .into_iter()
        .filter_map(|ip| match ip {
            std::net::IpAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .collect();
    let mut out = Vec::with_capacity(question_end + 16 * v4.len());
    out.extend_from_slice(&query[..2]);
    // A response, the query's opcode and recursion-desired, recursion available, and rcode.
    out.push(0x80 | (query[2] & 0x79));
    out.push(0x80 | rcode);
    out.extend_from_slice(&[0, 1]);
    out.extend_from_slice(&(v4.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&query[12..question_end]);
    for ip in v4 {
        // The question's name, by pointer; A, IN, a TTL of 30 s, 4 bytes.
        out.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4]);
        out.extend_from_slice(&ip.octets());
    }
    Some(out)
}

fn parse_address(cidr: &str) -> io::Result<(Ipv4Addr, u8)> {
    let bad = || io::Error::other(format!("{cidr:?}: not an IPv4 address/prefix"));
    let (ip, prefix) = cidr.split_once('/').ok_or_else(bad)?;
    let prefix: u8 = prefix.parse().map_err(|_| bad())?;
    if prefix > 32 {
        return Err(bad());
    }
    Ok((ip.parse().map_err(|_| bad())?, prefix))
}

fn index(name: &str) -> io::Result<u32> {
    let c = CString::new(name).unwrap();
    // SAFETY: if_nametoindex reads a C string.
    match unsafe { libc::if_nametoindex(c.as_ptr()) } {
        0 => Err(io::Error::other(format!("no interface {name}"))),
        i => Ok(i),
    }
}

// --- netlink ------------------------------------------------------------------------------------

const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_APPEND: u16 = 0x800;
const NLMSG_ERROR: u16 = 2;
const NLA_F_NESTED: u16 = 0x8000;

/// A netlink socket, and the messages it sends, each acknowledged.
struct Netlink {
    fd: OwnedFd,
    seq: u32,
}

impl Netlink {
    fn open(protocol: libc::c_int) -> io::Result<Netlink> {
        // SAFETY: socket, then bind to the kernel's address (all zero but the family).
        unsafe {
            let fd = libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                protocol,
            );
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);
            let mut addr: libc::sockaddr_nl = zeroed();
            addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
            if libc::bind(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_nl).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Netlink { fd, seq: 0 })
        }
    }

    /// A message of `kind`, `flags` (NLM_F_REQUEST added) and `payload`.
    fn message(&mut self, kind: u16, flags: u16, payload: &[u8]) -> Vec<u8> {
        self.seq += 1;
        let len = 16 + payload.len();
        let mut m = Vec::with_capacity(len);
        m.extend_from_slice(&(len as u32).to_ne_bytes());
        m.extend_from_slice(&kind.to_ne_bytes());
        m.extend_from_slice(&(flags | NLM_F_REQUEST).to_ne_bytes());
        m.extend_from_slice(&self.seq.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes());
        m.extend_from_slice(payload);
        m
    }

    /// Sends `messages` at once, and waits for the `acks` acknowledgements they asked for: the
    /// first error, if there is one.
    fn send(&mut self, messages: &[Vec<u8>], acks: usize) -> io::Result<()> {
        let all = messages.concat();
        // SAFETY: send of a buffer this owns.
        if unsafe { libc::send(self.fd.as_raw_fd(), all.as_ptr().cast(), all.len(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut acked = 0;
        let mut buf = vec![0u8; 16384];
        while acked < acks {
            // SAFETY: recv into a buffer this owns, at most its length.
            let n =
                unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut at = 0;
            while at + 16 <= n as usize {
                let len = u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
                let kind = u16::from_ne_bytes(buf[at + 4..at + 6].try_into().unwrap());
                if len < 16 {
                    break;
                }
                if kind == NLMSG_ERROR {
                    let error = i32::from_ne_bytes(buf[at + 16..at + 20].try_into().unwrap());
                    if error != 0 {
                        return Err(io::Error::from_raw_os_error(-error));
                    }
                    acked += 1;
                }
                at += (len + 3) & !3;
            }
        }
        Ok(())
    }
}

/// An attribute: its length, its type, its data, padded to four bytes.
fn attr(kind: u16, data: &[u8]) -> Vec<u8> {
    let len = 4 + data.len();
    let mut a = Vec::with_capacity((len + 3) & !3);
    a.extend_from_slice(&(len as u16).to_ne_bytes());
    a.extend_from_slice(&kind.to_ne_bytes());
    a.extend_from_slice(data);
    a.resize((len + 3) & !3, 0);
    a
}

fn nested(kind: u16, parts: &[Vec<u8>]) -> Vec<u8> {
    attr(kind | NLA_F_NESTED, &parts.concat())
}

fn string(s: &str) -> Vec<u8> {
    [s.as_bytes(), b"\0"].concat()
}

fn be32(n: u32) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

// rtnetlink

const RTM_NEWLINK: u16 = 16;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;

/// struct ifinfomsg: family, its padding, type, index, flags, change.
fn ifinfomsg(index: u32, flags: u32, change: u32) -> Vec<u8> {
    let mut m = vec![libc::AF_UNSPEC as u8, 0, 0, 0];
    m.extend_from_slice(&(index as i32).to_ne_bytes());
    m.extend_from_slice(&flags.to_ne_bytes());
    m.extend_from_slice(&change.to_ne_bytes());
    m
}

fn link_up(nl: &mut Netlink, index: u32) -> io::Result<()> {
    let up = libc::IFF_UP as u32;
    let m = nl.message(RTM_NEWLINK, NLM_F_ACK, &ifinfomsg(index, up, up));
    nl.send(&[m], 1)
}

fn create_dummy(nl: &mut Netlink, name: &str) -> io::Result<()> {
    let payload = [
        ifinfomsg(0, 0, 0),
        attr(libc::IFLA_IFNAME, &string(name)),
        nested(
            libc::IFLA_LINKINFO,
            &[attr(libc::IFLA_INFO_KIND, &string("dummy"))],
        ),
    ]
    .concat();
    let m = nl.message(RTM_NEWLINK, NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, &payload);
    nl.send(&[m], 1)
}

fn add_address(nl: &mut Netlink, index: u32, address: Ipv4Addr, prefix: u8) -> io::Result<()> {
    // struct ifaddrmsg: family, prefix length, flags, scope, index.
    let mut payload = vec![libc::AF_INET as u8, prefix, 0, 0];
    payload.extend_from_slice(&index.to_ne_bytes());
    payload.extend(attr(libc::IFA_LOCAL, &address.octets()));
    payload.extend(attr(libc::IFA_ADDRESS, &address.octets()));
    let m = nl.message(RTM_NEWADDR, NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, &payload);
    nl.send(&[m], 1)
}

fn add_default_route(nl: &mut Netlink, index: u32) -> io::Result<()> {
    // struct rtmsg: family, dst_len, src_len, tos, table, protocol, scope, type, flags.
    let mut payload = vec![
        libc::AF_INET as u8,
        0,
        0,
        0,
        libc::RT_TABLE_MAIN,
        libc::RTPROT_BOOT,
        libc::RT_SCOPE_LINK,
        libc::RTN_UNICAST,
    ];
    payload.extend_from_slice(&0u32.to_ne_bytes());
    payload.extend(attr(libc::RTA_OIF, &index.to_ne_bytes()));
    let m = nl.message(
        RTM_NEWROUTE,
        NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        &payload,
    );
    nl.send(&[m], 1)
}

// nftables

const NFNL_MSG_BATCH_BEGIN: u16 = 16;
const NFNL_MSG_BATCH_END: u16 = 17;
const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_NEWRULE: u16 = 6;
const NFPROTO_INET: u8 = 1;
const NF_INET_LOCAL_OUT: u32 = 3;
const NFT_REG_1: u32 = 1;
const NFT_META_OIF: u32 = 5;
const NFT_META_L4PROTO: u32 = 16;
const NFT_CMP_EQ: u32 = 0;
const NFT_CMP_NEQ: u32 = 1;

/// struct nfgenmsg: family, version, resource id (big-endian).
fn nfgenmsg(family: u8, res_id: u16) -> Vec<u8> {
    let mut m = vec![family, 0];
    m.extend_from_slice(&res_id.to_be_bytes());
    m
}

fn expression(name: &str, data: &[Vec<u8>]) -> Vec<u8> {
    nested(1, &[attr(1, &string(name)), nested(2, data)])
}

/// `table inet bottle { chain output { type nat hook output priority -100; meta oif != lo
/// meta l4proto tcp redirect to :PORT } }`: every TCP connection that doesn't stay in the bottle,
/// to the agent.
fn redirect_tcp(nl: &mut Netlink, lo: u32, port: u16) -> io::Result<()> {
    let ty = |op: u16| (NFNL_SUBSYS_NFTABLES << 8) | op;
    let begin = nl.message(NFNL_MSG_BATCH_BEGIN, 0, &nfgenmsg(0, NFNL_SUBSYS_NFTABLES));
    let table = nl.message(
        ty(NFT_MSG_NEWTABLE),
        NLM_F_ACK | NLM_F_CREATE,
        &[nfgenmsg(NFPROTO_INET, 0), attr(1, &string("bottle"))].concat(),
    );
    let chain = nl.message(
        ty(NFT_MSG_NEWCHAIN),
        NLM_F_ACK | NLM_F_CREATE,
        &[
            nfgenmsg(NFPROTO_INET, 0),
            attr(1, &string("bottle")),
            attr(3, &string("output")),
            nested(
                4,
                &[
                    attr(1, &be32(NF_INET_LOCAL_OUT)),
                    attr(2, &be32(-100i32 as u32)),
                ],
            ),
            attr(7, &string("nat")),
        ]
        .concat(),
    );
    let data = |bytes: &[u8]| nested(3, &[attr(1, bytes)]);
    let expressions = [
        expression(
            "meta",
            &[attr(1, &be32(NFT_REG_1)), attr(2, &be32(NFT_META_OIF))],
        ),
        expression(
            "cmp",
            &[
                attr(1, &be32(NFT_REG_1)),
                attr(2, &be32(NFT_CMP_NEQ)),
                data(&lo.to_ne_bytes()),
            ],
        ),
        expression(
            "meta",
            &[attr(1, &be32(NFT_REG_1)), attr(2, &be32(NFT_META_L4PROTO))],
        ),
        expression(
            "cmp",
            &[
                attr(1, &be32(NFT_REG_1)),
                attr(2, &be32(NFT_CMP_EQ)),
                data(&[libc::IPPROTO_TCP as u8]),
            ],
        ),
        expression(
            "immediate",
            &[
                attr(1, &be32(NFT_REG_1)),
                nested(2, &[attr(1, &port.to_be_bytes())]),
            ],
        ),
        expression("redir", &[attr(1, &be32(NFT_REG_1))]),
    ];
    let rule = nl.message(
        ty(NFT_MSG_NEWRULE),
        NLM_F_ACK | NLM_F_CREATE | NLM_F_APPEND,
        &[
            nfgenmsg(NFPROTO_INET, 0),
            attr(1, &string("bottle")),
            attr(2, &string("output")),
            nested(4, &expressions),
        ]
        .concat(),
    );
    let end = nl.message(NFNL_MSG_BATCH_END, 0, &nfgenmsg(0, NFNL_SUBSYS_NFTABLES));
    nl.send(&[begin, table, chain, rule, end], 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_are_padded_to_four_bytes() {
        assert_eq!(attr(3, b"lo\0"), [7, 0, 3, 0, b'l', b'o', 0, 0]);
        assert_eq!(
            nested(18, &[attr(1, b"ab")]),
            [12, 0, 18, 0x80, 6, 0, 1, 0, b'a', b'b', 0, 0]
        );
    }

    #[test]
    fn dns_queries_for_a_records_go_to_the_host_and_the_rest_have_no_answers() {
        // A query for AAAA of "a.b", id 0x1234, recursion desired: answered here, no records.
        let query = [
            0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, b'a', 1, b'b', 0, 0, 28, 0, 1,
        ];
        let answer = answer(&query, "unix:/nonexistent").unwrap();
        assert_eq!(
            &answer[..12],
            &[0x12, 0x34, 0x81, 0x80, 0, 1, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(&answer[12..], &query[12..]);
        // A, with no host to ask: a server failure.
        let mut a = query;
        a[18] = 1;
        let answer = super::answer(&a, "unix:/nonexistent").unwrap();
        assert_eq!(answer[3] & 0x0f, 2);
        // Not a query.
        assert!(super::answer(&[0; 5], "unix:/x").is_none());
    }

    #[test]
    fn addresses_are_an_ip_and_a_prefix() {
        assert_eq!(
            parse_address("100.96.0.2/30").unwrap(),
            (Ipv4Addr::new(100, 96, 0, 2), 30)
        );
        assert!(parse_address("100.96.0.2").is_err());
        assert!(parse_address("100.96.0.2/33").is_err());
    }
}
