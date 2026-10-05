//! PCP: the gateway's announcement and a mapping, over UDP to the gateway's
//! port.
//!
//! Three properties are easy to get wrong:
//!
//! **A mapping is named by its nonce.** The request carries twelve random
//! bytes, the answer carries them back, and a renewal or a delete must carry
//! the same twelve: a gateway refuses either under another nonce, so a mapping
//! whose nonce is lost cannot be deleted and waits out its lifetime.
//!
//! **Every address is sixteen bytes**, an IPv4 address in its IPv4-mapped
//! form. "Any address", which a delete names, is the unspecified address of
//! the mapping's family, so for IPv4 it is the mapped form of 0.0.0.0, not
//! `::`.
//!
//! **An answer in version 0 is NAT-PMP's**: the gateway speaks only that.
//!
//! Options may follow an answer's fixed part, each padded to four bytes. Their
//! framing is checked and their content is not used.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::{Error, Result};

/// The gateway's port, for this protocol and for NAT-PMP.
pub const PORT: u16 = 5351;
/// The version this side speaks.
pub const VERSION: u8 = 2;
/// The longest message the protocol allows.
pub const MAX_LEN: usize = 1100;

/// The announcement's opcode: a request that asks nothing, answered by a
/// gateway that speaks the protocol.
pub const ANNOUNCE: u8 = 0;
/// The mapping's opcode.
pub const MAP: u8 = 1;
/// Set on an answer's opcode.
const OP_ANSWER: u8 = 0x80;
const HEADER_LEN: usize = 24;
const MAP_LEN: usize = 36;
const PROTOCOL_UDP: u8 = 17;

/// The twelve bytes that name a mapping, from the caller's entropy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nonce(pub [u8; 12]);

/// A result code, as a gateway sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultCode(pub u8);

impl ResultCode {
    pub const SUCCESS: Self = Self(0);
    pub const UNSUPPORTED_VERSION: Self = Self(1);
    /// The gateway has the protocol and has it switched off, or the request
    /// names a mapping under another nonce.
    pub const NOT_AUTHORIZED: Self = Self(2);
    pub const MALFORMED_REQUEST: Self = Self(3);
    pub const UNSUPPORTED_OPCODE: Self = Self(4);
    pub const UNSUPPORTED_OPTION: Self = Self(5);
    pub const MALFORMED_OPTION: Self = Self(6);
    pub const NETWORK_FAILURE: Self = Self(7);
    pub const NO_RESOURCES: Self = Self(8);
    pub const UNSUPPORTED_PROTOCOL: Self = Self(9);
    pub const USER_EXCEEDED_QUOTA: Self = Self(10);
    pub const CANNOT_PROVIDE_EXTERNAL: Self = Self(11);
    /// The address this side named as its own is not the one the gateway saw:
    /// another translator sits in between.
    pub const ADDRESS_MISMATCH: Self = Self(12);
    pub const EXCESSIVE_REMOTE_PEERS: Self = Self(13);
}

/// A gateway's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    /// The request's opcode: an announcement or a mapping.
    pub opcode: u8,
    pub result: ResultCode,
    pub lifetime_s: u32,
    pub epoch_s: u32,
    /// The mapping, on an answer to one that carries its fields; a refusal
    /// may not.
    pub map: Option<Mapping>,
}

/// A mapping as the gateway states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub nonce: Nonce,
    pub protocol: u8,
    pub internal_port: u16,
    pub external_port: u16,
    pub external_address: IpAddr,
}

/// An announcement from `client`.
pub fn announce_request(client: IpAddr) -> [u8; HEADER_LEN] {
    header(ANNOUNCE, 0, client)
}

/// A request for a UDP mapping of `internal_port` on `client`, suggesting
/// `suggested_port` at any external address.
pub fn map_request(
    client: IpAddr,
    nonce: Nonce,
    internal_port: u16,
    suggested_port: u16,
    lifetime_s: u32,
) -> [u8; HEADER_LEN + MAP_LEN] {
    let mut out = [0u8; HEADER_LEN + MAP_LEN];
    let (head, map) = out.split_at_mut(HEADER_LEN);
    head.copy_from_slice(&header(MAP, lifetime_s, client));
    let (nonce_field, rest) = map.split_at_mut(12);
    nonce_field.copy_from_slice(&nonce.0);
    let [i0, i1] = internal_port.to_be_bytes();
    let [s0, s1] = suggested_port.to_be_bytes();
    let any = match client {
        IpAddr::V4(_) => mapped(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.octets(),
    };
    let fields = [PROTOCOL_UDP, 0, 0, 0, i0, i1, s0, s1];
    let (fixed, address) = rest.split_at_mut(fields.len());
    fixed.copy_from_slice(&fields);
    address.copy_from_slice(&any);
    out
}

/// A request deleting the mapping `nonce` names: no lifetime, no suggested
/// port, any address.
pub fn delete_request(
    client: IpAddr,
    nonce: Nonce,
    internal_port: u16,
) -> [u8; HEADER_LEN + MAP_LEN] {
    map_request(client, nonce, internal_port, 0, 0)
}

/// Read a datagram from the gateway's port.
///
/// Another version is refused with that version, so an answer read by the
/// wrong protocol's parser says which protocol the gateway speaks.
pub fn parse(data: &[u8]) -> Result<Reply> {
    let Some(&version) = data.first() else {
        return Err(Error::Short);
    };
    if version != VERSION {
        return Err(Error::Version(version));
    }
    if data.len() > MAX_LEN {
        return Err(Error::Long);
    }
    if data.len() < HEADER_LEN {
        return Err(Error::Short);
    }
    let [_, opcode, _, result, l0, l1, l2, l3] = chunk::<8>(data, 0)?;
    let [e0, e1, e2, e3] = chunk::<4>(data, 8)?;
    if opcode & OP_ANSWER == 0 {
        return Err(Error::Malformed);
    }
    let opcode = opcode & !OP_ANSWER;
    let result = ResultCode(result);
    let mut reply = Reply {
        opcode,
        result,
        lifetime_s: u32::from_be_bytes([l0, l1, l2, l3]),
        epoch_s: u32::from_be_bytes([e0, e1, e2, e3]),
        map: None,
    };
    let options = match opcode {
        ANNOUNCE => HEADER_LEN,
        MAP if data.len() >= HEADER_LEN + MAP_LEN => {
            let nonce = Nonce(chunk::<12>(data, HEADER_LEN)?);
            let [protocol, _, _, _, i0, i1, x0, x1] = chunk::<8>(data, HEADER_LEN + 12)?;
            reply.map = Some(Mapping {
                nonce,
                protocol,
                internal_port: u16::from_be_bytes([i0, i1]),
                external_port: u16::from_be_bytes([x0, x1]),
                external_address: address(chunk::<16>(data, HEADER_LEN + 20)?),
            });
            HEADER_LEN + MAP_LEN
        }
        // A refusal of a mapping may stop after the header.
        MAP if result != ResultCode::SUCCESS => data.len(),
        MAP => return Err(Error::Short),
        _ => return Err(Error::Malformed),
    };
    check_options(data.get(options..).unwrap_or_default())?;
    Ok(reply)
}

/// Options: a code, a reserved byte, a length, then that many bytes padded to
/// four. A last option missing only its padding is taken.
fn check_options(mut rest: &[u8]) -> Result<()> {
    while !rest.is_empty() {
        let [_, _, n0, n1] = chunk::<4>(rest, 0)?;
        let len = usize::from(u16::from_be_bytes([n0, n1]));
        let end = 4 + len;
        if rest.len() < end {
            return Err(Error::Malformed);
        }
        rest = rest.get(end.next_multiple_of(4)..).unwrap_or_default();
    }
    Ok(())
}

fn header(opcode: u8, lifetime_s: u32, client: IpAddr) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    let [l0, l1, l2, l3] = lifetime_s.to_be_bytes();
    let (fixed, address) = out.split_at_mut(8);
    fixed.copy_from_slice(&[VERSION, opcode, 0, 0, l0, l1, l2, l3]);
    address.copy_from_slice(&match client {
        IpAddr::V4(v4) => mapped(v4),
        IpAddr::V6(v6) => v6.octets(),
    });
    out
}

fn mapped(v4: Ipv4Addr) -> [u8; 16] {
    v4.to_ipv6_mapped().octets()
}

fn address(bytes: [u8; 16]) -> IpAddr {
    let v6 = Ipv6Addr::from(bytes);
    match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    }
}

fn chunk<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N]> {
    data.get(at..)
        .and_then(|rest| rest.first_chunk::<N>())
        .copied()
        .ok_or(Error::Short)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 166));
    const NONCE: Nonce = Nonce([0xa5, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0x5a]);

    /// A gateway's answer, encoded as one would: the half this side never sends.
    fn answer(reply: &Reply, options: &[u8]) -> Vec<u8> {
        let mut out = vec![VERSION, reply.opcode | 0x80, 0, reply.result.0];
        out.extend_from_slice(&reply.lifetime_s.to_be_bytes());
        out.extend_from_slice(&reply.epoch_s.to_be_bytes());
        out.extend_from_slice(&[0; 12]);
        if let Some(map) = reply.map {
            out.extend_from_slice(&map.nonce.0);
            out.extend_from_slice(&[map.protocol, 0, 0, 0]);
            out.extend_from_slice(&map.internal_port.to_be_bytes());
            out.extend_from_slice(&map.external_port.to_be_bytes());
            out.extend_from_slice(&match map.external_address {
                IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
                IpAddr::V6(v6) => v6.octets(),
            });
        }
        out.extend_from_slice(options);
        out
    }

    #[test]
    fn an_announcement_names_the_client_in_the_mapped_form() {
        let mut expected = [0u8; 24];
        expected[0] = 2;
        expected[18..24].copy_from_slice(&[0xff, 0xff, 192, 168, 1, 166]);
        assert_eq!(announce_request(CLIENT), expected);
    }

    #[test]
    fn a_map_request_is_sixty_bytes_in_order() {
        let request = map_request(CLIENT, NONCE, 24137, 24137, 7200);
        assert_eq!(&request[..8], &[2, 1, 0, 0, 0, 0, 0x1c, 0x20]);
        assert_eq!(
            &request[8..24],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 192, 168, 1, 166]
        );
        assert_eq!(&request[24..36], &NONCE.0);
        assert_eq!(&request[36..44], &[17, 0, 0, 0, 0x5e, 0x49, 0x5e, 0x49]);
        // Any external address, in the client's family: the mapped 0.0.0.0.
        assert_eq!(
            &request[44..60],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0]
        );
    }

    #[test]
    fn a_delete_keeps_the_nonce_and_asks_nothing_else() {
        let request = delete_request(CLIENT, NONCE, 24137);
        assert_eq!(&request[4..8], &[0, 0, 0, 0]);
        assert_eq!(&request[24..36], &NONCE.0);
        assert_eq!(&request[40..44], &[0x5e, 0x49, 0, 0]);
        assert_eq!(
            &request[44..60],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0]
        );
        // From an IPv6 client, any address is the unspecified IPv6 address.
        let v6 = delete_request(
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 7)),
            NONCE,
            24137,
        );
        assert_eq!(&v6[44..60], &[0; 16]);
    }

    #[test]
    fn answers_round_trip() {
        let addresses = [
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        ];
        let mut seen = 0;
        for (i, port) in [1u16, 1024, 24000, 47915, u16::MAX].into_iter().enumerate() {
            for lifetime_s in [0u32, 120, 7200, u32::MAX] {
                for external_address in addresses {
                    let mut nonce = NONCE;
                    nonce.0[i] ^= port as u8;
                    let map = Reply {
                        opcode: MAP,
                        result: ResultCode(i as u8),
                        lifetime_s,
                        epoch_s: lifetime_s ^ 0x5a5a,
                        map: Some(Mapping {
                            nonce,
                            protocol: 17,
                            internal_port: port,
                            external_port: port.rotate_right(5),
                            external_address,
                        }),
                    };
                    assert_eq!(parse(&answer(&map, &[])), Ok(map));
                    let announce = Reply {
                        opcode: ANNOUNCE,
                        map: None,
                        ..map
                    };
                    assert_eq!(parse(&answer(&announce, &[])), Ok(announce));
                    seen += 2;
                }
            }
        }
        assert_eq!(seen, 120);
    }

    #[test]
    fn a_version_zero_answer_is_named() {
        // A gateway that speaks only NAT-PMP, refusing the version.
        assert_eq!(parse(&[0, 0x80, 0, 1, 0, 0, 0, 9]), Err(Error::Version(0)));
        assert_eq!(parse(&[]), Err(Error::Short));
    }

    #[test]
    fn a_request_is_not_an_answer() {
        assert_eq!(parse(&announce_request(CLIENT)), Err(Error::Malformed));
        assert_eq!(
            parse(&map_request(CLIENT, NONCE, 1, 1, 1)),
            Err(Error::Malformed)
        );
    }

    #[test]
    fn lengths_are_bounded_both_ways() {
        let ok = Reply {
            opcode: MAP,
            result: ResultCode::SUCCESS,
            lifetime_s: 7200,
            epoch_s: 9,
            map: Some(Mapping {
                nonce: NONCE,
                protocol: 17,
                internal_port: 24137,
                external_port: 24137,
                external_address: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)),
            }),
        };
        let whole = answer(&ok, &[]);
        assert_eq!(parse(&whole[..23]), Err(Error::Short));
        // A successful mapping answer must carry the mapping.
        assert_eq!(parse(&whole[..59]), Err(Error::Short));
        // A refused one may stop after its header.
        let mut refused = whole[..24].to_vec();
        refused[3] = ResultCode::NOT_AUTHORIZED.0;
        assert_eq!(
            parse(&refused),
            Ok(Reply {
                result: ResultCode::NOT_AUTHORIZED,
                map: None,
                ..ok
            })
        );
        let mut long = whole.clone();
        long.resize(MAX_LEN + 1, 0);
        assert_eq!(parse(&long), Err(Error::Long));
        // An opcode never asked for.
        let mut peer = whole;
        peer[1] = 0x82;
        assert_eq!(parse(&peer), Err(Error::Malformed));
    }

    #[test]
    fn options_are_framed_or_refused() {
        let reply = Reply {
            opcode: ANNOUNCE,
            result: ResultCode::SUCCESS,
            lifetime_s: 0,
            epoch_s: 9,
            map: None,
        };
        // A five-byte option padded to eight, then an empty one.
        let padded = [0x81, 0, 0, 5, 1, 2, 3, 4, 5, 0, 0, 0, 0x82, 0, 0, 0];
        assert_eq!(parse(&answer(&reply, &padded)), Ok(reply));
        // The last option short of its padding only.
        assert_eq!(parse(&answer(&reply, &padded[..9])), Ok(reply));
        // An option longer than what is left.
        assert_eq!(
            parse(&answer(&reply, &[0x81, 0, 0, 9, 1, 2, 3])),
            Err(Error::Malformed)
        );
        // Two bytes cannot hold an option's head.
        assert_eq!(parse(&answer(&reply, &[0x81, 0])), Err(Error::Short));
    }
}

#[cfg(test)]
mod captured {
    use super::*;

    #[test]
    fn a_gateways_announcement() {
        assert_eq!(
            parse(include_bytes!("../tests/data/openwrt/pcp-announce.bin")),
            Ok(Reply {
                opcode: ANNOUNCE,
                result: ResultCode::SUCCESS,
                lifetime_s: 0,
                epoch_s: 306_899,
                map: None,
            })
        );
    }

    #[test]
    fn a_mapping_made_then_deleted_under_one_nonce() {
        let nonce = Nonce([
            0x9e, 0xe2, 0x37, 0x07, 0x5b, 0x5c, 0xd5, 0x35, 0x96, 0x06, 0x25, 0x26,
        ]);
        let gateway = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1));
        assert_eq!(
            parse(include_bytes!("../tests/data/debian/pcp-map.bin")),
            Ok(Reply {
                opcode: MAP,
                result: ResultCode::SUCCESS,
                lifetime_s: 120,
                epoch_s: 28,
                map: Some(Mapping {
                    nonce,
                    protocol: 17,
                    internal_port: 47915,
                    external_port: 47915,
                    external_address: gateway,
                }),
            })
        );
        assert_eq!(
            parse(include_bytes!("../tests/data/debian/pcp-delete.bin")),
            Ok(Reply {
                opcode: MAP,
                result: ResultCode::SUCCESS,
                lifetime_s: 0,
                epoch_s: 28,
                map: Some(Mapping {
                    nonce,
                    protocol: 17,
                    internal_port: 47915,
                    external_port: 0,
                    external_address: gateway,
                }),
            })
        );
    }

    #[test]
    fn a_nat_pmp_answer_says_so() {
        assert_eq!(
            parse(include_bytes!("../tests/data/openwrt/natpmp-address.bin")),
            Err(Error::Version(0))
        );
    }
}
