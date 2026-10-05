//! The URLs a gateway hands out: where its description is, and where the
//! description says its control services are.
//!
//! Plain HTTP to an IPv4 address and nothing else. A name would need resolving
//! and a secure scheme a TLS stack, and neither is taken; discovery runs over
//! IPv4, so nothing a gateway hands back here names an IPv6 host.
//!
//! **A reference is resolved by the general rules for URI references**,
//! because gateways write every form of one: absolute, from the root, relative
//! to the description's own path, against a base element or without one.
//!
//! **Every byte that reaches a request line is printable and not a space**, so
//! nothing a device writes into a URL can end the line or add a header to it.

use core::net::{Ipv4Addr, SocketAddrV4};

use crate::{Error, Result};

const DEFAULT_PORT: u16 = 80;

/// An HTTP URL whose host is an IPv4 address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// The host and port connected to.
    pub addr: SocketAddrV4,
    /// The path and query as a request line carries them: from the root,
    /// never empty, without a fragment.
    pub path: String,
}

impl Url {
    /// Read an absolute URL.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let (scheme, rest) = s.split_once(':').ok_or(Error::Url)?;
        if !scheme.eq_ignore_ascii_case("http") {
            return Err(Error::Url);
        }
        Self::from_authority(rest.strip_prefix("//").ok_or(Error::Url)?)
    }

    /// Resolve `reference` with this URL as its base.
    pub fn join(&self, reference: &str) -> Result<Self> {
        let reference = reference.trim();
        let reference = reference
            .split_once('#')
            .map_or(reference, |(head, _)| head);
        if has_scheme(reference) {
            return Self::parse(reference);
        }
        if let Some(rest) = reference.strip_prefix("//") {
            return Self::from_authority(rest);
        }
        let base = self
            .path
            .split_once('?')
            .map_or(self.path.as_str(), |(path, _)| path);
        let path = if reference.is_empty() {
            self.path.clone()
        } else if reference.starts_with('/') {
            reference.to_string()
        } else if reference.starts_with('?') {
            format!("{base}{reference}")
        } else {
            // Relative to the base's directory: up to and with its last slash.
            let directory = base.rfind('/').and_then(|i| base.get(..=i)).unwrap_or("/");
            format!("{directory}{reference}")
        };
        Ok(Self {
            addr: self.addr,
            path: normalize(&path)?,
        })
    }

    /// The authority, then the path; `rest` follows the `//`.
    fn from_authority(rest: &str) -> Result<Self> {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let path = tail.split_once('#').map_or(tail, |(head, _)| head);
        Ok(Self {
            addr: address(authority)?,
            path: normalize(path)?,
        })
    }
}

/// An IPv4 address and an optional port; nothing else is a host here.
fn address(authority: &str) -> Result<SocketAddrV4> {
    if authority.contains(['@', '[']) {
        return Err(Error::Url);
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, "")) => (host, DEFAULT_PORT),
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => {
            match port.parse::<u16>() {
                Ok(port) if port != 0 => (host, port),
                _ => return Err(Error::Url),
            }
        }
        Some(_) => return Err(Error::Url),
        None => (authority, DEFAULT_PORT),
    };
    let ip: Ipv4Addr = host.parse().map_err(|_| Error::Url)?;
    Ok(SocketAddrV4::new(ip, port))
}

/// A scheme: a letter, then letters, digits, `+`, `-` or `.`, then a colon.
fn has_scheme(reference: &str) -> bool {
    let Some((scheme, _)) = reference.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The path from the root with its dot segments removed, the query kept as
/// it is, and every byte checked.
fn normalize(path_and_query: &str) -> Result<String> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query, None),
    };
    let mut out = remove_dot_segments(path);
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    if !out.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::Url);
    }
    Ok(out)
}

/// Dot segments removed from a path read from the root: `.` names the
/// directory it is in, `..` its parent, and neither climbs above the root.
fn remove_dot_segments(path: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut parts = path.strip_prefix('/').unwrap_or(path).split('/').peekable();
    // A path ending in a dot segment names a directory, so it ends in a slash.
    let mut directory = false;
    while let Some(part) = parts.next() {
        let last = parts.peek().is_none();
        match part {
            "." => directory = last,
            ".." => {
                kept.pop();
                directory = last;
            }
            _ => {
                kept.push(part);
                directory = false;
            }
        }
    }
    let mut out = String::from("/");
    out.push_str(&kept.join("/"));
    if directory && !kept.is_empty() {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn an_absolute_url_is_read() {
        let u = url("http://192.168.1.1:5000/rootDesc.xml");
        assert_eq!(
            u.addr,
            SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 5000)
        );
        assert_eq!(u.path, "/rootDesc.xml");
        assert_eq!(url("HTTP://192.168.1.1").addr.port(), 80);
        assert_eq!(url("http://192.168.1.1:").addr.port(), 80);
        assert_eq!(url("http://192.168.1.1").path, "/");
        assert_eq!(url("  http://192.168.1.1:52869\r\n").path, "/");
        assert_eq!(url("http://192.168.1.1?x=1#frag").path, "/?x=1");
        assert_eq!(url("http://192.168.1.1/a/./b/../c#frag").path, "/a/c");
    }

    #[test]
    fn only_plain_http_to_an_ipv4_address() {
        for refused in [
            "https://192.168.1.1/",
            "ftp://192.168.1.1/",
            "http://gateway.lan/rootDesc.xml",
            "http://[fe80::1%253]:5000/rootDesc.xml",
            "http://user@192.168.1.1/",
            "http://192.168.1.1:0/",
            "http://192.168.1.1:65536/",
            "http://192.168.1.1:50a0/",
            "http://192.168.1.256/",
            "http:192.168.1.1/",
            "192.168.1.1/rootDesc.xml",
            "",
        ] {
            assert_eq!(Url::parse(refused), Err(Error::Url), "{refused:?}");
        }
    }

    #[test]
    fn nothing_can_end_the_request_line() {
        let base = url("http://192.168.1.1:5000/rootDesc.xml");
        for refused in [
            "/ctl/IPConn HTTP/1.1\r\nX-Injected: 1",
            "/ctl/IP Conn",
            "/ctl/IP\nConn",
            "/ctl/\u{7f}",
            "/ctl/caf\u{e9}",
            "ctl/\tIPConn",
        ] {
            assert_eq!(base.join(refused), Err(Error::Url), "{refused:?}");
        }
        // Surrounding whitespace is trimmed, not refused.
        assert_eq!(base.join(" /ctl/IPConn\r\n").unwrap().path, "/ctl/IPConn");
    }

    #[test]
    fn references_resolve_by_the_general_rules() {
        // The standard's examples, on an IPv4 host.
        let base = url("http://192.0.2.1/b/c/d;p?q");
        for (reference, path) in [
            ("g", "/b/c/g"),
            ("./g", "/b/c/g"),
            ("g/", "/b/c/g/"),
            ("/g", "/g"),
            ("?y", "/b/c/d;p?y"),
            ("g?y", "/b/c/g?y"),
            ("#s", "/b/c/d;p?q"),
            ("g#s", "/b/c/g"),
            ("g?y#s", "/b/c/g?y"),
            (";x", "/b/c/;x"),
            ("g;x", "/b/c/g;x"),
            ("", "/b/c/d;p?q"),
            (".", "/b/c/"),
            ("./", "/b/c/"),
            ("..", "/b/"),
            ("../", "/b/"),
            ("../g", "/b/g"),
            ("../..", "/"),
            ("../../", "/"),
            ("../../g", "/g"),
            ("../../../g", "/g"),
            ("../../../../g", "/g"),
            ("/./g", "/g"),
            ("/../g", "/g"),
            ("g.", "/b/c/g."),
            (".g", "/b/c/.g"),
            ("g..", "/b/c/g.."),
            ("..g", "/b/c/..g"),
            ("./../g", "/b/g"),
            ("./g/.", "/b/c/g/"),
            ("g/./h", "/b/c/g/h"),
            ("g/../h", "/b/c/h"),
            ("g;x=1/./y", "/b/c/g;x=1/y"),
            ("g;x=1/../y", "/b/c/y"),
            ("g?y/./x", "/b/c/g?y/./x"),
            ("g?y/../x", "/b/c/g?y/../x"),
            ("g#s/./x", "/b/c/g"),
        ] {
            let joined = base.join(reference).unwrap();
            assert_eq!(joined.path, path, "{reference:?}");
            assert_eq!(joined.addr, base.addr);
        }
        // Another authority, by a network path or a whole URL.
        let other = base.join("//192.0.2.9:8080/x").unwrap();
        assert_eq!(
            (other.addr.to_string(), other.path),
            ("192.0.2.9:8080".into(), "/x".into())
        );
        assert_eq!(base.join("http://192.0.2.9/y").unwrap().path, "/y");
        // A scheme of another kind is refused rather than followed.
        assert_eq!(base.join("g:h"), Err(Error::Url));
    }

    #[test]
    fn the_forms_gateways_write() {
        let location = url("http://192.168.1.1:5000/rootDesc.xml");
        assert_eq!(location.join("/ctl/IPConn").unwrap().path, "/ctl/IPConn");
        assert_eq!(location.join("ctl/IPConn").unwrap().path, "/ctl/IPConn");
        let nested = url("http://192.168.1.1:49152/desc/igd.xml");
        assert_eq!(nested.join("ctl/IPConn").unwrap().path, "/desc/ctl/IPConn");
        let base = url("http://192.168.1.1:52869");
        assert_eq!(
            base.join("/upnp/control/WANIPConn1").unwrap().path,
            "/upnp/control/WANIPConn1"
        );
        let absolute = location
            .join("http://192.168.1.1:2869/upnphost/udhisapi.dll?control=uuid:x")
            .unwrap();
        assert_eq!(absolute.addr.port(), 2869);
        assert_eq!(absolute.path, "/upnphost/udhisapi.dll?control=uuid:x");
    }
}
