//! What the client's tests share: a reflexive server on loopback.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::thread;
use std::time::Duration;

use lowlat_core::stun::{self, Message};

/// Where a second server listens beside one at 127.0.0.1, and the address it
/// reports seeing the socket at. Linux and Windows answer on all of 127/8,
/// and a server at 127.0.0.2 sees the socket at 127.0.0.1 as the first does.
/// macOS holds 127.0.0.1 alone, so there the second listens at an address the
/// machine holds beyond the loopback and reports 127.0.0.1, as a translator
/// in front of the port shows every destination one public address.
#[allow(dead_code, reason = "not every test binary has a second server")]
pub(crate) fn second_server() -> (Ipv4Addr, Option<Ipv4Addr>) {
    #[cfg(not(target_os = "macos"))]
    let second = (Ipv4Addr::new(127, 0, 0, 2), None);
    #[cfg(target_os = "macos")]
    let second = (
        lowlat_net::host_addresses(true)
            .into_iter()
            .find_map(|ip| match ip {
                std::net::IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
                _ => None,
            })
            .expect("a second server needs an address beyond the loopback"),
        Some(Ipv4Addr::LOCALHOST),
    );
    second
}

/// A reflexive server at `at` that reports the source it saw -- at `report`
/// instead of its address when given one, and moved by `shift` ports: a
/// translator in a few lines -- for `limit` requests, then falls silent.
pub(crate) fn server(
    at: Ipv4Addr,
    report: Option<Ipv4Addr>,
    shift: u16,
    limit: usize,
) -> (SocketAddrV4, thread::JoinHandle<()>) {
    let socket = UdpSocket::bind(SocketAddrV4::new(at, 0)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let SocketAddr::V4(addr) = socket.local_addr().unwrap() else {
        unreachable!("bound to an IPv4 address")
    };
    let thread = thread::spawn(move || {
        let mut buf = [0u8; 256];
        for _ in 0..limit {
            let Ok((len, from)) = socket.recv_from(&mut buf) else {
                return;
            };
            let Ok(request) = Message::parse(&buf[..len]) else {
                continue;
            };
            let ip = report.map_or(from.ip(), std::net::IpAddr::V4);
            let seen = SocketAddr::new(ip, from.port().wrapping_add(shift));
            let mut out = [0u8; 256];
            let len =
                stun::encode_binding_response(&mut out, request.transaction_id(), seen, "any")
                    .unwrap();
            let _ = socket.send_to(&out[..len], from);
        }
    });
    (addr, thread)
}
