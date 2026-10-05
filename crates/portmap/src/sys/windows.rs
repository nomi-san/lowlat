//! The default gateway and the interface a search leaves by, on Windows.

#![allow(unsafe_code)]

use core::net::{Ipv4Addr, SocketAddrV4};
use std::io;
use std::net::UdpSocket;
use std::os::windows::io::AsRawSocket;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, IP_MULTICAST_IF, IPPROTO_IP, SOCKADDR, SOCKADDR_IN, SOCKET, setsockopt,
};

/// Beyond the local network, so the system answers with the address it
/// gives the default route. Nothing is sent to it.
const BEYOND: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 9);

/// The gateway of the adapter that carries the default route: the one that
/// holds the address the system picks for a destination beyond the local
/// network. An adapter can list a gateway without carrying the route.
pub(crate) fn gateway() -> Option<Ipv4Addr> {
    let local = crate::mapper::local_toward(BEYOND)?;
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER
        | GAA_FLAG_SKIP_FRIENDLY_NAME;
    // The list's size is not known ahead: the system says what it needs, and
    // an adapter can appear between the asking and the filling.
    let mut size: u32 = 16 * 1024;
    for _ in 0..3 {
        // Eight-byte words, for the alignment the list's records need.
        let words = usize::try_from(size).unwrap_or(0).div_ceil(8);
        let mut storage: Vec<u64> = vec![0; words];
        // SAFETY: the storage is at least `size` bytes, writable and aligned
        // for the records; the call writes no more than that and otherwise
        // says how much it needs.
        let rc = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_INET),
                flags,
                core::ptr::null(),
                storage.as_mut_ptr().cast(),
                &raw mut size,
            )
        };
        if rc == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if rc != NO_ERROR {
            return None;
        }
        return gateway_of(storage.as_ptr().cast(), local);
    }
    None
}

/// The first IPv4 gateway of the adapter that is up and holds `local`, in a
/// list the system filled and the caller keeps alive.
fn gateway_of(first: *const IP_ADAPTER_ADDRESSES_LH, local: Ipv4Addr) -> Option<Ipv4Addr> {
    let mut adapter = first;
    while !adapter.is_null() {
        // SAFETY: the walk stops at null, so this is a record the system
        // wrote, in storage the caller holds for the length of the walk.
        let entry = unsafe { &*adapter };
        adapter = entry.Next;
        if entry.OperStatus != IfOperStatusUp {
            continue;
        }
        let mut holds = false;
        let mut unicast = entry.FirstUnicastAddress;
        while !unicast.is_null() {
            // SAFETY: as above, for the adapter's address records.
            let address = unsafe { &*unicast };
            unicast = address.Next;
            holds |= ipv4(address.Address.lpSockaddr) == Some(local);
        }
        if !holds {
            continue;
        }
        let mut gateway = entry.FirstGatewayAddress;
        while !gateway.is_null() {
            // SAFETY: as above, for the adapter's gateway records.
            let record = unsafe { &*gateway };
            gateway = record.Next;
            if let Some(found) = ipv4(record.Address.lpSockaddr) {
                return Some(found);
            }
        }
    }
    None
}

/// The IPv4 address a socket address the system wrote holds, if it is one.
fn ipv4(sockaddr: *const SOCKADDR) -> Option<Ipv4Addr> {
    if sockaddr.is_null() {
        return None;
    }
    // SAFETY: a non-null address carries its family field.
    if unsafe { (*sockaddr).sa_family } != AF_INET {
        return None;
    }
    // SAFETY: the family says this is a SOCKADDR_IN.
    let sin = unsafe { &*sockaddr.cast::<SOCKADDR_IN>() };
    // SAFETY: every view of the address union is plain bytes; the word is in
    // network order, so its bytes are the octets.
    let word = unsafe { sin.sin_addr.S_un.S_addr };
    Some(Ipv4Addr::from(word.to_ne_bytes()))
}

/// Send `socket`'s multicast out of the interface that holds `local`.
pub(crate) fn multicast_from(socket: &UdpSocket, local: Ipv4Addr) -> io::Result<()> {
    // The interface named by its address, as the network-order word the
    // option takes.
    let word = u32::from_ne_bytes(local.octets());
    let handle = SOCKET::try_from(socket.as_raw_socket()).map_err(io::Error::other)?;
    let len = i32::try_from(core::mem::size_of_val(&word)).map_err(io::Error::other)?;
    // SAFETY: a live socket, and an option value of `len` bytes read during
    // the call only.
    let rc = unsafe {
        setsockopt(
            handle,
            IPPROTO_IP,
            IP_MULTICAST_IF,
            (&raw const word).cast(),
            len,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine with a route beyond its network has an adapter carrying it,
    /// and that adapter names a gateway that is neither loopback nor nothing.
    #[test]
    fn the_adapter_carrying_the_default_route_names_its_gateway() {
        if crate::mapper::local_toward(BEYOND).is_none() {
            return;
        }
        let gateway = gateway().expect("a default route with no gateway found");
        assert!(
            !gateway.is_loopback() && !gateway.is_unspecified(),
            "{gateway}"
        );
    }
}
