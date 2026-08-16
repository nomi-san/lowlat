//! The connect URL.
//!
//! Authentication is the query string on the upgrade request, not a header and
//! not a message: session, role, protocol version, build, and SDK version. The
//! service closes the socket without a reply when any of them is missing or
//! when the version is not `1`, so a malformed URL presents as an immediate
//! disconnect rather than as an error.

/// Protocol version carried on the upgrade. The service rejects anything else.
const VERSION: &str = "1";

/// Build the URL for one role on one session.
///
/// The root path before the query is required. Without it the request line is
/// `GET ?session_id=... HTTP/1.1`, which is malformed, and the edge in front of
/// the service answers 400 rather than upgrading. What must not appear is a
/// trailing slash on the `Host:` header, which is a property of the URL parser
/// rather than of the URL: a stack that mangles `wss://host/?query` into
/// `Host: host/` gets the same 400 from the other direction.
pub fn connect(
    server: &str,
    session_id: &str,
    role: Role,
    build: &str,
    sdk_version: u32,
) -> String {
    let mut url = String::with_capacity(server.len() + session_id.len() + build.len() + 64);
    url.push_str(server.trim_end_matches('/'));
    url.push_str("/?session_id=");
    encode_into(&mut url, session_id);
    url.push_str("&role=");
    url.push_str(role.as_str());
    url.push_str("&version=");
    url.push_str(VERSION);
    url.push_str("&build=");
    encode_into(&mut url, build);
    url.push_str("&sdk_version=");
    url.push_str(&sdk_version.to_string());
    url
}

/// Which side of the exchange we are. It decides which messages are ours to
/// send and which direction the service forwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Role {
    Host,
    Client,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Client => "client",
        }
    }
}

/// Percent-encode everything that is not unreserved.
///
/// A session identifier is opaque and is not guaranteed to be URL safe, and one
/// that is pasted through unencoded truncates the query at the first separator
/// it happens to contain.
fn encode_into(out: &mut String, value: &str) {
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex(byte >> 4));
            out.push(hex(byte & 0x0F));
        }
    }
}

fn hex(nibble: u8) -> char {
    char::from(match nibble {
        0..=9 => b'0' + nibble,
        _ => b'A' + nibble - 10,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_carries_every_field_the_service_checks() {
        let url = connect(
            "wss://example.test",
            "abc123",
            Role::Host,
            "150-104a",
            393_216,
        );
        assert_eq!(
            url,
            "wss://example.test/?session_id=abc123&role=host&version=1&build=150-104a&sdk_version=393216"
        );
    }

    /// Exactly one root path segment, however the server was configured. None
    /// makes the request line malformed and the edge answers 400; two is a
    /// path the service does not serve. *Named regression test.*
    #[test]
    fn the_query_hangs_off_exactly_one_root_path() {
        for server in ["wss://example.test", "wss://example.test/"] {
            let url = connect(server, "s", Role::Host, "b", 1);
            assert!(url.starts_with("wss://example.test/?"), "got {url}");
            assert!(!url.contains("//?"), "got {url}");
        }
    }

    #[test]
    fn an_opaque_session_is_percent_encoded() {
        let url = connect("wss://h", "a b&c=d", Role::Client, "b", 1);
        assert!(url.contains("/?session_id=a%20b%26c%3Dd"), "got {url}");
        assert!(url.contains("&role=client"));
    }
}
