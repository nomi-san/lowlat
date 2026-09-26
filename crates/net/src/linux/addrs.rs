//! The interface walk behind the host candidates.

use core::net::Ipv4Addr;

/// Every IPv4 address on an interface that is up, as the system lists them;
/// which of them to offer is decided above.
pub(crate) fn interface_v4() -> Vec<Ipv4Addr> {
    let mut list: *mut libc::ifaddrs = core::ptr::null_mut();
    // SAFETY: getifaddrs writes one pointer to a list it allocates and owns.
    // A failure leaves nothing to release.
    if unsafe { libc::getifaddrs(&raw mut list) } != 0 {
        return Vec::new();
    }

    let mut found = Vec::new();
    let mut node = list;
    while !node.is_null() {
        // SAFETY: the walk stops at null, so this is a node getifaddrs built,
        // and the list stays alive until freeifaddrs below.
        let entry = unsafe { &*node };
        node = entry.ifa_next;

        if entry.ifa_addr.is_null() || entry.ifa_flags & (libc::IFF_UP as u32) == 0 {
            continue;
        }
        // SAFETY: a non-null ifa_addr points at a sockaddr, and the family
        // field is present for every family.
        if i32::from(unsafe { (*entry.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // SAFETY: the family says AF_INET, so the address is a sockaddr_in.
        let sin = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
        found.push(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
    }

    // SAFETY: `list` came from the successful getifaddrs above, has not been
    // released, and the walk copied out of it rather than keeping pointers in.
    unsafe { libc::freeifaddrs(list) };
    found
}
