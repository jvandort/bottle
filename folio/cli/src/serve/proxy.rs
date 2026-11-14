//! What bottle-cli says of a connection it routes here: a PROXY protocol header, version 2
//! (haproxy's proxy-protocol.txt), first: the address and port the bottle connected to, then, as
//! fields, the name it asked for (SNI or Host), whether it spoke TLS (which bottle-cli has taken
//! off, so what follows is the plain conversation), and, in a field of bottle-cli's own, the
//! machine it's from (agent-client's net.rs, PP2_TYPE_MACHINE).

use std::io::{self, Read};
use std::net::IpAddr;

const SIGNATURE: &[u8; 12] = b"\r\n\r\n\x00\r\nQUIT\n";
const VERSION_2_PROXY: u8 = 0x21;
const TCP_OVER_IPV4: u8 = 0x11;
const TCP_OVER_IPV6: u8 = 0x21;
const PP2_TYPE_AUTHORITY: u8 = 0x02;
const PP2_TYPE_SSL: u8 = 0x20;
const PP2_CLIENT_SSL: u8 = 0x01;
const PP2_TYPE_MACHINE: u8 = 0xE0;

/// What bottle-cli says of a routed connection.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub address: IpAddr,
    pub port: u16,
    pub name: Option<String>,
    pub tls: bool,
    pub machine: Option<String>,
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

/// A PROXY protocol v2 header, as bottle-cli writes one; InvalidData if it isn't one.
pub fn read(reader: &mut impl Read) -> io::Result<Header> {
    let mut fixed = [0; 16];
    reader.read_exact(&mut fixed)?;
    if &fixed[..12] != SIGNATURE
        || fixed[12] != VERSION_2_PROXY
        || !matches!(fixed[13], TCP_OVER_IPV4 | TCP_OVER_IPV6)
    {
        return Err(invalid("not a PROXY protocol v2 header for TCP"));
    }
    let mut body = vec![0; usize::from(u16::from_be_bytes([fixed[14], fixed[15]]))];
    reader.read_exact(&mut body)?;
    let size = if fixed[13] == TCP_OVER_IPV4 { 4 } else { 16 };
    if body.len() < 2 * size + 4 {
        return Err(invalid(
            "a PROXY protocol header too short for its addresses",
        ));
    }
    let address = if size == 4 {
        IpAddr::from(<[u8; 4]>::try_from(&body[size..2 * size]).unwrap())
    } else {
        IpAddr::from(<[u8; 16]>::try_from(&body[size..2 * size]).unwrap())
    };
    let port = u16::from_be_bytes([body[2 * size + 2], body[2 * size + 3]]);
    let (mut name, mut tls, mut machine) = (None, false, None);
    let mut at = 2 * size + 4;
    while at + 3 <= body.len() {
        let length = usize::from(u16::from_be_bytes([body[at + 1], body[at + 2]]));
        let value = &body[at + 3..(at + 3 + length).min(body.len())];
        match body[at] {
            PP2_TYPE_AUTHORITY => name = Some(String::from_utf8_lossy(value).into_owned()),
            PP2_TYPE_SSL => tls = value.first().is_some_and(|b| b & PP2_CLIENT_SSL != 0),
            PP2_TYPE_MACHINE => machine = Some(String::from_utf8_lossy(value).into_owned()),
            _ => {}
        }
        at += 3 + length;
    }
    Ok(Header {
        address,
        port,
        name: name.filter(|n| !n.is_empty()),
        tls,
        machine,
    })
}

#[cfg(test)]
pub fn header(
    address: [u8; 4],
    port: u16,
    name: Option<&str>,
    tls: bool,
    machine: &str,
) -> Vec<u8> {
    let mut fields = Vec::new();
    let mut field = |kind: u8, value: &[u8]| {
        fields.push(kind);
        fields.extend((value.len() as u16).to_be_bytes());
        fields.extend(value);
    };
    if let Some(name) = name {
        field(PP2_TYPE_AUTHORITY, name.as_bytes());
    }
    if tls {
        field(PP2_TYPE_SSL, &[PP2_CLIENT_SSL, 0, 0, 0, 0]);
    }
    field(PP2_TYPE_MACHINE, machine.as_bytes());
    let mut body = vec![127, 0, 0, 1];
    body.extend(address);
    body.extend(40000u16.to_be_bytes());
    body.extend(port.to_be_bytes());
    body.extend(fields);
    let mut out = SIGNATURE.to_vec();
    out.extend([VERSION_2_PROXY, TCP_OVER_IPV4]);
    out.extend((body.len() as u16).to_be_bytes());
    out.extend(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_says_where_what_and_whose() {
        let bytes = header([192, 0, 2, 1], 80, Some("bottle.host"), true, "m1");
        let h = read(&mut bytes.as_slice()).unwrap();
        assert_eq!(h.address.to_string(), "192.0.2.1");
        assert_eq!(
            (h.port, h.name.as_deref(), h.tls, h.machine.as_deref()),
            (80, Some("bottle.host"), true, Some("m1"))
        );
        assert!(read(&mut &b"GET / HTTP/1.1\r\n\r\n"[..]).is_err());
        let plain = read(&mut header([192, 0, 2, 1], 80, None, false, "m1").as_slice()).unwrap();
        assert_eq!((plain.name, plain.tls), (None, false));
        for end in [0, 10, 20, bytes.len() - 1] {
            assert!(read(&mut &bytes[..end]).is_err(), "cut at {end}");
        }
    }

    #[test]
    fn an_ipv6_address() {
        let mut bytes = SIGNATURE.to_vec();
        bytes.extend([VERSION_2_PROXY, TCP_OVER_IPV6, 0, 36]);
        bytes.extend([0; 16]);
        bytes.extend(
            "2001:db8::1"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets(),
        );
        bytes.extend([0, 0, 0, 22]);
        let h = read(&mut bytes.as_slice()).unwrap();
        assert_eq!((h.address.to_string(), h.port), ("2001:db8::1".into(), 22));
    }
}
