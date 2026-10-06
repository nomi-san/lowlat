//! What the client's tests share: a reflexive server on loopback.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::thread;
use std::time::Duration;

use lowlat_core::stun::{self, Message};

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
