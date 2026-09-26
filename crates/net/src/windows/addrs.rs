//! The interface walk behind the host candidates.

use core::net::Ipv4Addr;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_FRIENDLY_NAME,
    GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

/// Every IPv4 address on an interface that is up, as the system lists them;
/// which of them to offer is decided above.
pub(crate) fn interface_v4() -> Vec<Ipv4Addr> {
    let flags = GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER
        | GAA_FLAG_SKIP_FRIENDLY_NAME;
    // The list's size is not known ahead: the system says what it needs, and
    // an interface can appear between the asking and the filling, so the fill
    // is tried a few times.
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
            return Vec::new();
        }
        return walk(storage.as_ptr().cast());
    }
    Vec::new()
}

/// The addresses in a list the system filled, which the caller keeps alive.
fn walk(first: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<Ipv4Addr> {
    let mut found = Vec::new();
    let mut adapter = first;
    while !adapter.is_null() {
        // SAFETY: the walk stops at null, so this is a record the system
        // wrote, in storage the caller holds for the length of the walk.
        let entry = unsafe { &*adapter };
        adapter = entry.Next;
        if entry.OperStatus != IfOperStatusUp {
            continue;
        }
        let mut unicast = entry.FirstUnicastAddress;
        while !unicast.is_null() {
            // SAFETY: as above, for the interface's address records.
            let address = unsafe { &*unicast };
            unicast = address.Next;
            let sockaddr = address.Address.lpSockaddr;
            if sockaddr.is_null() {
                continue;
            }
            // SAFETY: a non-null address carries its family field.
            if unsafe { (*sockaddr).sa_family } != AF_INET {
                continue;
            }
            // SAFETY: the family says this is a SOCKADDR_IN.
            let sin = unsafe { &*sockaddr.cast::<SOCKADDR_IN>() };
            // SAFETY: every view of the address union is plain bytes; the
            // word is in network order, so its bytes are the octets.
            let word = unsafe { sin.sin_addr.S_un.S_addr };
            found.push(Ipv4Addr::from(word.to_ne_bytes()));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loopback interface is always up and always carries its address,
    /// so a walk that reads the wrong family, the wrong status or the wrong
    /// word fails here on any machine.
    #[test]
    fn the_walk_finds_an_address_on_an_interface_that_is_up() {
        let found = interface_v4();
        assert!(
            found.contains(&Ipv4Addr::LOCALHOST),
            "the loopback address was not listed: {found:?}"
        );
    }
}
