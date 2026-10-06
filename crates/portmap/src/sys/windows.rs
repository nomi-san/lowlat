//! The default gateway, the interface a search leaves by, a connection a stop
//! can cut short, and the machine's name and identifier, on Windows.

#![allow(unsafe_code)]

use core::net::{Ipv4Addr, SocketAddrV4};
use core::time::Duration;
use std::io;
use std::net::{TcpStream, UdpSocket};
use std::os::windows::io::{AsRawSocket, FromRawSocket, OwnedSocket, RawSocket};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, FD_SET, FIONBIO, INVALID_SOCKET, IP_MULTICAST_IF, IPPROTO_IP, IPPROTO_TCP, SO_ERROR,
    SOCK_STREAM, SOCKADDR, SOCKADDR_IN, SOCKET, SOCKET_ERROR, SOL_SOCKET, TIMEVAL,
    WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSADATA, WSAEWOULDBLOCK, WSAGetLastError,
    WSASocketW, WSAStartup, connect, getsockopt, ioctlsocket, select, setsockopt,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};
use windows_sys::Win32::System::WindowsProgramming::GetComputerNameW;

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
            // A tunnel that sends everything to its far end lists 0.0.0.0: no
            // gateway at all.
            if let Some(found) = ipv4(record.Address.lpSockaddr)
                && !found.is_unspecified()
            {
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

/// Start the socket library once per process, before the first socket of our
/// own. The reference it takes is never given back: a library inside someone
/// else's process cannot know when its last socket has closed.
fn start() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let code = *STARTED.get_or_init(|| {
        let mut data = WSADATA::default();
        // SAFETY: version 2.2 and writable storage of the right type.
        unsafe { WSAStartup(0x0202, &raw mut data) }
    });
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

/// The last socket error, as an error.
fn last_error() -> io::Error {
    // SAFETY: reads the calling thread's own error value.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// A TCP connection being made, waited for in pieces so that a stop is seen
/// between them: the system's own connect waits whole.
#[derive(Debug)]
pub(crate) struct Connecting {
    socket: OwnedSocket,
}

impl Connecting {
    /// Begin connecting to `to`. Nothing waits here.
    pub(crate) fn start(to: SocketAddrV4) -> io::Result<Self> {
        start()?;
        let flags = WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT;
        // SAFETY: plain arguments and no protocol record; the socket returned
        // is owned below.
        let raw = unsafe {
            WSASocketW(
                i32::from(AF_INET),
                SOCK_STREAM,
                IPPROTO_TCP,
                core::ptr::null(),
                0,
                flags,
            )
        };
        if raw == INVALID_SOCKET {
            return Err(last_error());
        }
        let handle = RawSocket::try_from(raw).map_err(io::Error::other)?;
        // SAFETY: a socket this call opened and nothing else holds.
        let socket = unsafe { OwnedSocket::from_raw_socket(handle) };
        let mut nonblocking: u32 = 1;
        // SAFETY: a live socket and the flag's word, written during the call.
        if unsafe { ioctlsocket(raw, FIONBIO, &raw mut nonblocking) } != 0 {
            return Err(last_error());
        }
        let mut address = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: to.port().to_be(),
            ..SOCKADDR_IN::default()
        };
        address.sin_addr.S_un.S_addr = u32::from_ne_bytes(to.ip().octets());
        let len = i32::try_from(core::mem::size_of_val(&address)).map_err(io::Error::other)?;
        // SAFETY: a live socket and an address of `len` bytes read during the
        // call only.
        let rc = unsafe { connect(raw, (&raw const address).cast(), len) };
        if rc == SOCKET_ERROR {
            let error = last_error();
            if error.raw_os_error() != Some(WSAEWOULDBLOCK) {
                return Err(error);
            }
        }
        Ok(Self { socket })
    }

    /// Wait up to `wait` for the connection: the stream once it is made, a
    /// blocking one as the caller's reads expect, or this again while it is
    /// still being made. A failed connect is in the exception set, which is
    /// why this selects rather than polls: the poll call reports no failed
    /// connect before Windows 10 2004.
    pub(crate) fn wait(self, wait: Duration) -> io::Result<Result<TcpStream, Self>> {
        let raw = SOCKET::try_from(self.socket.as_raw_socket()).map_err(io::Error::other)?;
        let mut writable = FD_SET {
            fd_count: 1,
            ..FD_SET::default()
        };
        let mut failed = FD_SET {
            fd_count: 1,
            ..FD_SET::default()
        };
        if let (Some(w), Some(f)) = (writable.fd_array.first_mut(), failed.fd_array.first_mut()) {
            *w = raw;
            *f = raw;
        }
        let micros = wait.as_micros().max(1);
        let timeout = TIMEVAL {
            tv_sec: i32::try_from(micros / 1_000_000).unwrap_or(i32::MAX),
            tv_usec: i32::try_from(micros % 1_000_000).unwrap_or(0),
        };
        // SAFETY: two sets naming one live socket and a timeout, all live for
        // the call; the first argument is ignored on this system.
        let rc = unsafe {
            select(
                0,
                core::ptr::null_mut(),
                &raw mut writable,
                &raw mut failed,
                &raw const timeout,
            )
        };
        if rc == SOCKET_ERROR {
            return Err(last_error());
        }
        if rc == 0 {
            return Ok(Err(self));
        }
        // Connected or failed: the connect's own outcome says which.
        let mut outcome: i32 = 0;
        let mut len = i32::try_from(core::mem::size_of_val(&outcome)).map_err(io::Error::other)?;
        // SAFETY: a live socket, and an integer and its length written during
        // the call only.
        let rc = unsafe {
            getsockopt(
                raw,
                SOL_SOCKET,
                SO_ERROR,
                (&raw mut outcome).cast(),
                &raw mut len,
            )
        };
        if rc == SOCKET_ERROR {
            return Err(last_error());
        }
        if outcome != 0 {
            return Err(io::Error::from_raw_os_error(outcome));
        }
        if failed.fd_count != 0 {
            return Err(io::Error::from(io::ErrorKind::ConnectionRefused));
        }
        let stream = TcpStream::from(self.socket);
        stream.set_nonblocking(false)?;
        Ok(Ok(stream))
    }
}

/// The installation's identifier, `MachineGuid`, from the system's 64-bit
/// view whatever this build is. It must not leave the machine as it is: the
/// caller reduces it with a keyed hash first.
pub(crate) fn machine_id() -> Option<Vec<u8>> {
    let key: Vec<u16> = "SOFTWARE\\Microsoft\\Cryptography\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "MachineGuid\0".encode_utf16().collect();
    // Room for the identifier's 36 characters and more.
    let mut text = [0u16; 128];
    let mut size = u32::try_from(core::mem::size_of_val(&text)).ok()?;
    // SAFETY: two terminated wide strings, a buffer of `size` bytes and its
    // length, all live for the call; the type is not asked for.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            core::ptr::null_mut(),
            text.as_mut_ptr().cast(),
            &raw mut size,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let end = text.iter().position(|&unit| unit == 0)?;
    let id = String::from_utf16(text.get(..end)?).ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.as_bytes().to_vec())
}

/// The machine's network name, as the system reports it: in capitals, and at
/// most fifteen characters.
pub(crate) fn machine_name() -> Option<String> {
    // Room for the longest such name and its terminator, and more.
    let mut name = [0u16; 64];
    let mut len = u32::try_from(name.len()).ok()?;
    // SAFETY: a buffer of `len` wide characters, written by the call, and its
    // length, which the call sets to the characters written; both live for it.
    let ok = unsafe { GetComputerNameW(name.as_mut_ptr(), &raw mut len) };
    if ok == 0 {
        return None;
    }
    String::from_utf16(name.get(..usize::try_from(len).ok()?)?).ok()
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
