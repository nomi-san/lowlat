//! The default gateway, the interface a search leaves by, and the machine's
//! name, on Linux.

#![allow(unsafe_code)]

use core::net::Ipv4Addr;
use std::io;
use std::net::UdpSocket;
use std::os::fd::AsRawFd;

/// The default route's gateway, from the routing table.
pub(crate) fn gateway() -> Option<Ipv4Addr> {
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    crate::route::default_gateway(&table)
}

/// Send `socket`'s multicast out of the interface that holds `local`.
pub(crate) fn multicast_from(socket: &UdpSocket, local: Ipv4Addr) -> io::Result<()> {
    let address = libc::in_addr {
        s_addr: u32::from_ne_bytes(local.octets()),
    };
    let len =
        libc::socklen_t::try_from(core::mem::size_of_val(&address)).map_err(io::Error::other)?;
    // SAFETY: a live socket's descriptor, and an option whose value is the
    // address it names, read for its size during the call only.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            (&raw const address).cast(),
            len,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The machine's host name.
pub(crate) fn machine_name() -> Option<String> {
    // Longer than any host name the kernel holds, terminator included.
    let mut name = [0u8; 256];
    // SAFETY: a buffer of its own length, written by the call and live for it.
    let rc = unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) };
    if rc != 0 {
        return None;
    }
    let end = name.iter().position(|&byte| byte == 0)?;
    core::str::from_utf8(name.get(..end)?)
        .ok()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::SocketAddrV4;

    /// A machine with a route beyond its network names its gateway, neither
    /// loopback nor nothing.
    #[test]
    fn the_default_route_names_its_gateway() {
        let beyond = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 9);
        if crate::mapper::local_toward(beyond).is_none() {
            return;
        }
        let gateway = gateway().expect("a default route with no gateway found");
        assert!(
            !gateway.is_loopback() && !gateway.is_unspecified(),
            "{gateway}"
        );
    }
}
