//! NAT-PMP: the gateway's external address and a mapping, over UDP to the
//! gateway's port.
//!
//! Fixed binary messages in version 0, on the same port as PCP. A gateway may
//! speak either protocol or both, and one that speaks only this one answers a
//! PCP request in this version. Two properties are easy to get wrong:
//!
//! **A refusal is shorter than an answer.** It carries the version, the
//! opcode, the result and the epoch, and may end there, so a successful answer
//! must be whole while a refusal need only reach its epoch.
//!
//! **A mapping is deleted by asking for it again with no lifetime**, and the
//! suggested external port must then be zero.

use core::net::Ipv4Addr;

use crate::{Error, Result};

/// The gateway's port, for this protocol and for PCP.
pub const PORT: u16 = 5351;
/// The only version this protocol has.
pub const VERSION: u8 = 0;

const OP_ADDRESS: u8 = 0;
const OP_MAP_UDP: u8 = 1;
/// An answer's opcode is its request's with this bit set.
const OP_ANSWER: u8 = 0x80;

/// A result code, as a gateway sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultCode(pub u16);

impl ResultCode {
    pub const SUCCESS: Self = Self(0);
    pub const UNSUPPORTED_VERSION: Self = Self(1);
    /// The gateway has the protocol and has it switched off.
    pub const NOT_AUTHORIZED: Self = Self(2);
    pub const NETWORK_FAILURE: Self = Self(3);
    pub const OUT_OF_RESOURCES: Self = Self(4);
    pub const UNSUPPORTED_OPCODE: Self = Self(5);
}

/// A gateway's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// The gateway's external address, as it states it: a gateway with none
    /// sends the unspecified address.
    Address { epoch_s: u32, address: Ipv4Addr },
    /// A UDP mapping made, renewed or, with no lifetime, deleted.
    Map {
        epoch_s: u32,
        internal_port: u16,
        external_port: u16,
        lifetime_s: u32,
    },
    /// The request refused, and why. `opcode` is the request's.
    Refused {
        opcode: u8,
        result: ResultCode,
        epoch_s: u32,
    },
}

/// A request for the external address.
pub const fn address_request() -> [u8; 2] {
    [VERSION, OP_ADDRESS]
}

/// A request for a UDP mapping of `internal_port`, suggesting `external_port`.
pub const fn map_request(internal_port: u16, external_port: u16, lifetime_s: u32) -> [u8; 12] {
    let [i0, i1] = internal_port.to_be_bytes();
    let [e0, e1] = external_port.to_be_bytes();
    let [l0, l1, l2, l3] = lifetime_s.to_be_bytes();
    [VERSION, OP_MAP_UDP, 0, 0, i0, i1, e0, e1, l0, l1, l2, l3]
}

/// A request deleting the UDP mapping of `internal_port`.
pub const fn delete_request(internal_port: u16) -> [u8; 12] {
    map_request(internal_port, 0, 0)
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
    let [_, opcode, r0, r1, e0, e1, e2, e3] = chunk::<8>(data, 0)?;
    if opcode & OP_ANSWER == 0 {
        return Err(Error::Malformed);
    }
    let request = opcode & !OP_ANSWER;
    let result = ResultCode(u16::from_be_bytes([r0, r1]));
    let epoch_s = u32::from_be_bytes([e0, e1, e2, e3]);
    if result != ResultCode::SUCCESS {
        return Ok(Reply::Refused {
            opcode: request,
            result,
            epoch_s,
        });
    }
    match request {
        OP_ADDRESS => Ok(Reply::Address {
            epoch_s,
            address: Ipv4Addr::from(chunk::<4>(data, 8)?),
        }),
        OP_MAP_UDP => {
            let [i0, i1, x0, x1, l0, l1, l2, l3] = chunk::<8>(data, 8)?;
            Ok(Reply::Map {
                epoch_s,
                internal_port: u16::from_be_bytes([i0, i1]),
                external_port: u16::from_be_bytes([x0, x1]),
                lifetime_s: u32::from_be_bytes([l0, l1, l2, l3]),
            })
        }
        _ => Err(Error::Malformed),
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

    /// A gateway's answer, encoded as one would: the half this side never sends.
    fn answer(reply: &Reply) -> Vec<u8> {
        let mut out = vec![VERSION];
        match *reply {
            Reply::Address { epoch_s, address } => {
                out.extend_from_slice(&[0x80, 0, 0]);
                out.extend_from_slice(&epoch_s.to_be_bytes());
                out.extend_from_slice(&address.octets());
            }
            Reply::Map {
                epoch_s,
                internal_port,
                external_port,
                lifetime_s,
            } => {
                out.extend_from_slice(&[0x81, 0, 0]);
                out.extend_from_slice(&epoch_s.to_be_bytes());
                out.extend_from_slice(&internal_port.to_be_bytes());
                out.extend_from_slice(&external_port.to_be_bytes());
                out.extend_from_slice(&lifetime_s.to_be_bytes());
            }
            Reply::Refused {
                opcode,
                result,
                epoch_s,
            } => {
                out.push(opcode | 0x80);
                out.extend_from_slice(&result.0.to_be_bytes());
                out.extend_from_slice(&epoch_s.to_be_bytes());
            }
        }
        out
    }

    #[test]
    fn a_map_request_is_twelve_bytes_in_order() {
        assert_eq!(
            map_request(24137, 24138, 7200),
            [0, 1, 0, 0, 0x5e, 0x49, 0x5e, 0x4a, 0, 0, 0x1c, 0x20]
        );
        assert_eq!(address_request(), [0, 0]);
    }

    #[test]
    fn a_delete_suggests_no_port_and_asks_no_lifetime() {
        assert_eq!(
            delete_request(24137),
            [0, 1, 0, 0, 0x5e, 0x49, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn a_refusal_may_end_after_its_epoch() {
        assert_eq!(
            parse(&[0, 0x81, 0, 2, 0, 0, 1, 0]),
            Ok(Reply::Refused {
                opcode: 1,
                result: ResultCode::NOT_AUTHORIZED,
                epoch_s: 256
            })
        );
    }

    #[test]
    fn a_successful_answer_must_be_whole() {
        assert_eq!(
            parse(&[0, 0x80, 0, 0, 0, 0, 0, 9, 203, 0, 113]),
            Err(Error::Short)
        );
        assert_eq!(
            parse(&[
                0, 0x81, 0, 0, 0, 0, 0, 9, 0x5e, 0x49, 0x5e, 0x49, 0, 0, 0x1c
            ]),
            Err(Error::Short)
        );
        assert_eq!(parse(&[0, 0x80, 0, 0]), Err(Error::Short));
        assert_eq!(parse(&[]), Err(Error::Short));
    }

    #[test]
    fn another_version_is_named_not_misread() {
        // A PCP answer to an announcement: version 2, the answer bit, success.
        let pcp = [
            2, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(parse(&pcp), Err(Error::Version(2)));
    }

    #[test]
    fn a_request_is_not_an_answer() {
        assert_eq!(
            parse(&map_request(24137, 24137, 7200)),
            Err(Error::Malformed)
        );
        assert_eq!(
            parse(&[0, 0x83, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0]),
            Err(Error::Malformed)
        );
    }

    #[test]
    fn answers_round_trip() {
        let mut seen = 0;
        for (i, port) in [1u16, 1024, 24000, 25999, 47913, u16::MAX]
            .into_iter()
            .enumerate()
        {
            for lifetime_s in [0u32, 120, 7200, u32::MAX] {
                let epoch_s = (i as u32).wrapping_mul(0x9e37_79b9) ^ lifetime_s;
                let replies = [
                    Reply::Map {
                        epoch_s,
                        internal_port: port,
                        external_port: port.rotate_left(3),
                        lifetime_s,
                    },
                    Reply::Address {
                        epoch_s,
                        address: Ipv4Addr::from(epoch_s ^ u32::from(port)),
                    },
                    Reply::Refused {
                        opcode: (i % 3) as u8,
                        result: ResultCode(port),
                        epoch_s,
                    },
                ];
                for reply in replies {
                    if matches!(reply, Reply::Refused { result, .. } if result == ResultCode::SUCCESS)
                    {
                        continue;
                    }
                    assert_eq!(parse(&answer(&reply)), Ok(reply));
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, 72);
    }
}

#[cfg(test)]
mod captured {
    use super::*;

    #[test]
    fn a_gateways_external_address() {
        assert_eq!(
            parse(include_bytes!("../tests/data/openwrt/natpmp-address.bin")),
            Ok(Reply::Address {
                epoch_s: 306_899,
                address: Ipv4Addr::new(203, 0, 113, 7)
            })
        );
    }

    #[test]
    fn a_mapping_made_then_deleted() {
        assert_eq!(
            parse(include_bytes!("../tests/data/debian/natpmp-map.bin")),
            Ok(Reply::Map {
                epoch_s: 28,
                internal_port: 47914,
                external_port: 47914,
                lifetime_s: 120
            })
        );
        assert_eq!(
            parse(include_bytes!("../tests/data/debian/natpmp-delete.bin")),
            Ok(Reply::Map {
                epoch_s: 28,
                internal_port: 47914,
                external_port: 0,
                lifetime_s: 0
            })
        );
    }

    #[test]
    fn a_pcp_answer_says_so() {
        assert_eq!(
            parse(include_bytes!("../tests/data/debian/pcp-map.bin")),
            Err(Error::Version(2))
        );
    }
}
