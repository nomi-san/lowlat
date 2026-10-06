//! SSDP: the search that finds a gateway's description.
//!
//! A search is one datagram, to the group or to the gateway itself, and every
//! device that matches answers with an HTTP-shaped datagram naming where its
//! description lives. Three properties are easy to get wrong:
//!
//! **The answer's search target is not evidence.** A gateway may echo back
//! whatever version it was asked for, so the device is read from its
//! description, never from the answer.
//!
//! **Only the location is required.** An answer with no server or unique-name
//! header, or an empty one, is still an answer.
//!
//! **An answer may come from any port**, not the one searched, and a gateway
//! may answer the group's search and not its own address's, so both are sent.

use core::net::{Ipv4Addr, SocketAddrV4};

use crate::{Error, Result};

/// The port a search is sent to.
pub const PORT: u16 = 1900;
/// The group a search goes to over IPv4.
pub const GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), PORT);

/// The gateway device, in each version.
pub const GATEWAY_1: &str = "urn:schemas-upnp-org:device:InternetGatewayDevice:1";
pub const GATEWAY_2: &str = "urn:schemas-upnp-org:device:InternetGatewayDevice:2";

/// The longest answer read, whatever its datagram's size.
pub const MAX_LEN: usize = 2048;
const MAX_HEADERS: usize = 32;

/// A search for `target`, addressed to `to`: the group, or the gateway for a
/// search of its own. `wait_s` bounds how long a device may delay its answer.
pub fn search(to: SocketAddrV4, target: &str, wait_s: u8) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {to}\r\nMAN: \"ssdp:discover\"\r\nMX: {wait_s}\r\nST: {target}\r\n\r\n"
    )
}

/// What an answer says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer<'a> {
    /// Where the device's description is, unresolved.
    pub location: &'a str,
    /// The search target the device answered with, for the log only.
    pub target: Option<&'a str>,
}

/// Read an answer to a search.
pub fn parse(datagram: &[u8]) -> Result<Answer<'_>> {
    if datagram.len() > MAX_LEN {
        return Err(Error::Long);
    }
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut response = httparse::Response::new(&mut headers);
    match response.parse(datagram) {
        Ok(httparse::Status::Complete(_)) => {}
        // A datagram is all there is: a header block that has not ended by
        // its last byte never will.
        Ok(httparse::Status::Partial) => return Err(Error::Short),
        Err(_) => return Err(Error::Http),
    }
    match response.code {
        Some(200) => {}
        Some(code) => return Err(Error::Status(code)),
        None => return Err(Error::Http),
    }
    // Only the two headers read are read as text: any other may carry bytes
    // that are not, as the protocol allows.
    let mut location = None;
    let mut target = None;
    for header in response.headers.iter() {
        let text = || {
            core::str::from_utf8(header.value)
                .map(str::trim)
                .map_err(|_| Error::Malformed)
        };
        if header.name.eq_ignore_ascii_case("location") {
            location = Some(text()?);
        } else if header.name.eq_ignore_ascii_case("st") {
            target = Some(text()?);
        }
    }
    match location {
        Some(location) if !location.is_empty() => Ok(Answer { location, target }),
        _ => Err(Error::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_is_written_out() {
        assert_eq!(
            search(GROUP, GATEWAY_1, 2),
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n\
             ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n"
        );
        let gateway = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), PORT);
        assert!(
            search(gateway, GATEWAY_2, 1)
                .starts_with("M-SEARCH * HTTP/1.1\r\nHOST: 192.168.1.1:1900\r\n")
        );
    }

    #[test]
    fn only_the_location_is_required() {
        let bare = b"HTTP/1.1 200 OK\r\nlocation:  http://192.168.1.1:5000/rootDesc.xml \r\n\r\n";
        assert_eq!(
            parse(bare),
            Ok(Answer {
                location: "http://192.168.1.1:5000/rootDesc.xml",
                target: None
            })
        );
        let empty =
            b"HTTP/1.1 200 OK\r\nSERVER:\r\nUSN:\r\nLOCATION: http://192.168.1.1/d.xml\r\n\r\n";
        assert_eq!(parse(empty).unwrap().location, "http://192.168.1.1/d.xml");
        // A header not read may hold any byte the protocol allows.
        let latin =
            b"HTTP/1.1 200 OK\r\nSERVER: Caf\xe9 Router\r\nLOCATION: http://192.168.1.1/d.xml\r\n\r\n";
        assert_eq!(parse(latin).unwrap().location, "http://192.168.1.1/d.xml");
    }

    #[test]
    fn what_is_not_an_answer() {
        assert_eq!(
            parse(b"HTTP/1.1 200 OK\r\nST: x\r\n\r\n"),
            Err(Error::Malformed)
        );
        assert_eq!(
            parse(b"HTTP/1.1 200 OK\r\nLOCATION:\r\n\r\n"),
            Err(Error::Malformed)
        );
        assert_eq!(
            parse(b"HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.1/\xff\r\n\r\n"),
            Err(Error::Malformed)
        );
        assert_eq!(
            parse(b"HTTP/1.1 404 Not Found\r\n\r\n"),
            Err(Error::Status(404))
        );
        // Another device's announcement is not an answer.
        let notify =
            b"NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nLOCATION: http://192.168.1.9/\r\n\r\n";
        assert_eq!(parse(notify), Err(Error::Http));
        assert_eq!(
            parse(b"HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.1/\r\n"),
            Err(Error::Short)
        );
        assert_eq!(parse(&[b'x'; MAX_LEN + 1]), Err(Error::Long));
    }
}

#[cfg(test)]
mod captured {
    use super::*;

    #[test]
    fn each_gateways_answer() {
        for (wire, location, target) in [
            (
                &include_bytes!("../tests/data/openwrt/ssdp-gateway-1.http")[..],
                "http://192.168.1.1:5000/rootDesc.xml",
                GATEWAY_1,
            ),
            (
                include_bytes!("../tests/data/f670y/ssdp-gateway-1.http"),
                "http://192.168.1.1:52869/gatedesc.xml",
                GATEWAY_1,
            ),
            (
                include_bytes!("../tests/data/debian/ssdp-gateway-2.http"),
                "http://192.168.10.1:45345/rootDesc.xml",
                GATEWAY_2,
            ),
        ] {
            assert_eq!(
                parse(wire),
                Ok(Answer {
                    location,
                    target: Some(target)
                })
            );
        }
    }

    #[test]
    fn an_answer_echoes_the_version_asked_for() {
        // A device of version 1, asked for version 9. Its description says 1.
        let answer = parse(include_bytes!("../tests/data/openwrt/ssdp-gateway-9.http")).unwrap();
        assert_eq!(
            answer.target,
            Some("urn:schemas-upnp-org:device:InternetGatewayDevice:9")
        );
        assert_eq!(answer.location, "http://192.168.1.1:5000/rootDesc.xml");
    }
}
