//! The host a credential is for: reached (public addresses only), over TLS, and relayed to.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, LazyLock};

use rustix::event::{PollFd, PollFlags, poll};

use crate::serve::http::Refused;

use super::encoding::base64_decode;
use super::{A_MINUTE, CONNECT_TIMEOUT, STATUS_PEEK};

/// Whether a credential's host may be reached at `ip`: anywhere but this machine itself and its
/// link. A private address is expected: naming the host is what configuring the credential means.
pub(super) fn reachable(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback() || v4.is_link_local() || v4.is_multicast() || v4.is_unspecified())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// The addresses `host` resolves to that a credential's host may be at; refused if none.
/// $FOLIO_SERVE_RESOLVE (`name=ip:port,...`, the tests') answers for its names, wherever they are.
pub(super) fn allowed(host: &str, port: u16) -> Result<Vec<SocketAddr>, Refused> {
    if let Ok(given) = std::env::var("FOLIO_SERVE_RESOLVE") {
        let found: Vec<SocketAddr> = given
            .split(',')
            .filter_map(|e| e.split_once('='))
            .filter(|(name, _)| name.eq_ignore_ascii_case(host))
            .filter_map(|(_, at)| at.parse().ok())
            .collect();
        if !found.is_empty() {
            return Ok(found);
        }
    }
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| Refused::new(502, "Bad Gateway: cannot resolve host", "unresolved"))?
        .collect();
    let allowed: Vec<SocketAddr> = addresses
        .into_iter()
        .filter(|a| reachable(a.ip()))
        .collect();
    if allowed.is_empty() {
        return Err(Refused::new(
            403,
            "Forbidden: destination not allowed",
            "denied",
        ));
    }
    Ok(allowed)
}

pub(super) static TLS: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    // The tests' own CA, where they say.
    if let Some(pem) = std::env::var_os("FOLIO_SERVE_ROOTS").and_then(|p| std::fs::read(p).ok()) {
        for der in pem_certificates(&pem) {
            let _ = roots.add(rustls::pki_types::CertificateDer::from(der));
        }
    }
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
});

/// The certificates in a PEM file, as DER.
pub(super) fn pem_certificates(pem: &[u8]) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(pem);
    let mut out = Vec::new();
    let mut inside: Option<String> = None;
    for line in text.lines() {
        match (line.trim(), &mut inside) {
            ("-----BEGIN CERTIFICATE-----", _) => inside = Some(String::new()),
            ("-----END CERTIFICATE-----", Some(b64)) => {
                if let Some(der) = base64_decode(b64) {
                    out.push(der);
                }
                inside = None;
            }
            (l, Some(b64)) => b64.push_str(l),
            _ => {}
        }
    }
    out
}

/// A verified TLS connection to `host` at one of `addresses`, already judged, so a second lookup
/// can't swap them: always TLS, as a credential is never put on a request that leaves in the clear,
/// and one without it was TLS when the bottle sent it.
pub(super) fn open(
    host: &str,
    port: u16,
    addresses: &[SocketAddr],
) -> Result<(IpAddr, rustls::ClientConnection, TcpStream), Refused> {
    let server = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
        Refused::new(
            502,
            &format!("Bad Gateway: TLS to {host} failed"),
            "tls-failed",
        )
    })?;
    for address in addresses {
        let address = SocketAddr::new(
            address.ip(),
            if address.port() == 0 {
                port
            } else {
                address.port()
            },
        );
        let Ok(mut tcp) = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) else {
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(CONNECT_TIMEOUT));
        let mut conn =
            rustls::ClientConnection::new(TLS.clone(), server.clone()).map_err(|_| {
                Refused::new(
                    502,
                    &format!("Bad Gateway: TLS to {host} failed"),
                    "tls-failed",
                )
            })?;
        while conn.is_handshaking() {
            if conn.complete_io(&mut tcp).is_err() {
                return Err(Refused::new(
                    502,
                    &format!("Bad Gateway: TLS to {host} failed"),
                    "tls-failed",
                ));
            }
        }
        let _ = tcp.set_read_timeout(None);
        return Ok((address.ip(), conn, tcp));
    }
    Err(Refused::new(502, "Bad Gateway: cannot connect", "failed"))
}

/// Sends `head` (and `buffered`, what the bottle sent past it already read) to the host, then
/// relays both ways until the host is done, in one thread (a TLS connection can't be split): the
/// bottle finishing only says so to the host (it may still be answering); the host finishing ends
/// it all, once the bottle has what it sent. Keeps the start of the response in `peek`. The bytes
/// each way.
///
/// Neither side waits on the other: what one hasn't room for yet waits here, and while it does,
/// what would add to it isn't read, but the other way goes on. A host that answers while it's
/// still being sent, to a bottle that reads the answer only once it's sent it all, gets all of it,
/// as directly. What waits is bounded: rustls keeps at most 64 KiB unsent, and the bottle's side
/// at most what one read of the host brings.
pub(super) fn relay_tls(
    client: &UnixStream,
    mut conn: rustls::ClientConnection,
    mut tcp: TcpStream,
    head: &[u8],
    buffered: &[u8],
    peek: &mut Vec<u8>,
) -> io::Result<(u64, u64)> {
    let mut client = client.try_clone()?;
    client.set_nonblocking(true)?;
    tcp.set_nonblocking(true)?;
    // For the host, not yet given to rustls; for the bottle, not yet taken.
    let mut to_host: VecDeque<u8> = head.iter().chain(buffered).copied().collect();
    let mut to_bottle: VecDeque<u8> = VecDeque::new();
    let (mut up, mut down) = (to_host.len() as u64, 0u64);
    let (mut client_open, mut closing, mut host_done) = (true, false, false);
    let mut buf = vec![0; 64 * 1024];
    loop {
        // To the host: what rustls has, then more for it while it's sent all it had.
        loop {
            while conn.wants_write() {
                match conn.write_tls(&mut tcp) {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
            }
            if conn.wants_write() || to_host.is_empty() {
                break;
            }
            let (front, _) = to_host.as_slices();
            let n = conn.writer().write(front)?;
            to_host.drain(..n);
        }
        if !client_open && !closing && to_host.is_empty() {
            closing = true;
            conn.send_close_notify();
            continue;
        }
        // To the bottle: what it has room for.
        while !to_bottle.is_empty() {
            let (front, _) = to_bottle.as_slices();
            match client.write(front) {
                Ok(n) => {
                    to_bottle.drain(..n);
                    down += n as u64;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        if host_done && to_bottle.is_empty() {
            let _ = client.shutdown(std::net::Shutdown::Both);
            return Ok((up, down));
        }

        // More from the bottle once what it sent before is with rustls, and rustls has sent it
        // (the host may take its time); more from the host once the bottle's taken the last.
        let reading_bottle = client_open && !host_done && to_host.is_empty() && !conn.wants_write();
        let reading_host = !host_done && to_bottle.is_empty();
        let mut host_events = PollFlags::empty();
        if reading_host {
            host_events |= PollFlags::IN;
        }
        if conn.wants_write() {
            host_events |= PollFlags::OUT;
        }
        let mut bottle_events = PollFlags::empty();
        if reading_bottle {
            bottle_events |= PollFlags::IN;
        }
        if !to_bottle.is_empty() {
            bottle_events |= PollFlags::OUT;
        }
        // Each side only for what's wanted of it: a hangup is left for then, or it'd be reported
        // again and again.
        let mut fds = Vec::with_capacity(2);
        if !host_events.is_empty() {
            fds.push(PollFd::new(&tcp, host_events));
        }
        if !bottle_events.is_empty() {
            fds.push(PollFd::new(&client, bottle_events));
        }
        match poll(&mut fds, Some(&A_MINUTE)) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        // The host's first, when it's there; the bottle's last.
        let host = match host_events.is_empty() {
            true => PollFlags::empty(),
            false => fds[0].revents(),
        };
        let bottle = match bottle_events.is_empty() {
            true => PollFlags::empty(),
            false => fds[fds.len() - 1].revents(),
        };
        drop(fds);
        let readable = PollFlags::IN | PollFlags::HUP | PollFlags::ERR;
        if reading_bottle && bottle.intersects(readable) {
            match client.read(&mut buf[..16 * 1024]) {
                Ok(0) => client_open = false,
                Ok(n) => {
                    to_host.extend(&buf[..n]);
                    up += n as u64;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => client_open = false,
            }
        }
        if reading_host && host.intersects(readable) {
            let read = match conn.read_tls(&mut tcp) {
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                Err(_) => 0,
            };
            if read > 0 {
                conn.process_new_packets().map_err(io::Error::other)?;
            }
            host_done = read == 0;
            loop {
                match conn.reader().read(&mut buf) {
                    Ok(0) => {
                        host_done = true;
                        break;
                    }
                    Ok(n) => {
                        if peek.len() < STATUS_PEEK {
                            let take = n.min(STATUS_PEEK - peek.len());
                            peek.extend_from_slice(&buf[..take]);
                        }
                        to_bottle.extend(&buf[..n]);
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    // The host closed without saying so (no close_notify): it's done all the same.
                    Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                        host_done = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

/// The status of an HTTP response that starts with `response`, e.g. "401"; interim responses (100
/// Continue) skipped for the final one, when it's within what was kept.
pub(super) fn status_of(response: &[u8]) -> Option<String> {
    let (mut status, mut rest) = (None, response);
    while !rest.is_empty() {
        let line_end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .unwrap_or(rest.len());
        let parts: Vec<&[u8]> = rest[..line_end].split(|&b| b == b' ').collect();
        if parts.len() < 2
            || !parts[0].starts_with(b"HTTP/")
            || !parts[1].iter().all(u8::is_ascii_digit)
            || parts[1].is_empty()
        {
            break;
        }
        let code = String::from_utf8_lossy(parts[1]).into_owned();
        let interim = code.starts_with('1');
        status = Some(code);
        let Some(head_end) = rest.windows(4).position(|w| w == b"\r\n\r\n") else {
            break;
        };
        if !interim {
            break;
        }
        rest = &rest[head_end + 4..];
    }
    status
}

// --- small things ---------------------------------------------------------------------
