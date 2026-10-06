//! The HTTP subset: one request on a fresh connection, one response read to
//! its end, to the gateway's own address and nowhere else.
//!
//! What is left out is the point. No name resolution, proxy, TLS, compression
//! or redirect -- a redirect fails the exchange -- and `Connection: close` on
//! every request. Every size is capped before anything is held past it.
//!
//! **A body is read whatever the status.** A control action's fault arrives as
//! a server error with the fault in the body.
//!
//! **Chunks are taken although every request asks for a close**, because some
//! gateways send them anyway. A stated length and the connection's close are
//! the other two framings.

use core::net::SocketAddrV4;

use crate::{Error, Result};

/// The cap on a description's body.
pub const DESCRIPTION_CAP: usize = 128 * 1024;
/// The cap on a control answer's body.
pub const CONTROL_CAP: usize = 16 * 1024;
/// The cap on a response's header block.
pub const HEAD_CAP: usize = 8 * 1024;
const MAX_HEADERS: usize = 48;

/// The two methods this side sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// A request, whole, closing its connection.
///
/// `path` is a [`crate::url::Url`]'s, which holds printable characters only;
/// `headers` are this side's own.
pub fn request(
    method: Method,
    host: SocketAddrV4,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let verb = match method {
        Method::Get => "GET",
        Method::Post => "POST",
    };
    let mut head = format!("{verb} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if method == Method::Post {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

/// A response, read to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A response read as it arrives, in pieces of any size.
///
/// Everything received is kept, bounded, and read again from its start at
/// each piece: a gateway's answer arrives in a few reads, and reading it whole
/// each time leaves nothing to carry between them.
#[derive(Debug)]
pub struct Reader {
    cap: usize,
    received: Vec<u8>,
}

impl Reader {
    /// A reader whose body may not pass `cap` bytes.
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            received: Vec::new(),
        }
    }

    /// Take the next bytes; the response once it is whole.
    ///
    /// What is read depends on the bytes alone, never on how the reads cut
    /// them: a response that ends within the bound is whole whatever follows
    /// it, and one that has not ended by then never will be.
    pub fn push(&mut self, data: &[u8]) -> Result<Option<Response>> {
        // Room for the head, the body and its chunk framing, and one byte
        // more to tell a response that fits from one that does not.
        let bound = HEAD_CAP + 2 * self.cap;
        let room = (bound + 1).saturating_sub(self.received.len());
        self.received
            .extend_from_slice(data.get(..room).unwrap_or(data));
        match read(&self.received, self.cap, false)? {
            Some(response) => Ok(Some(response)),
            None if self.received.len() > bound => Err(Error::TooLarge),
            None => Ok(None),
        }
    }

    /// The connection closed: the end of a body the close frames, otherwise
    /// a truncation.
    pub fn finish(&self) -> Result<Response> {
        read(&self.received, self.cap, true)?.ok_or(Error::Truncated)
    }
}

enum Framing {
    Length(usize),
    Chunked,
    Close,
}

/// The response in `received`, if it has ended; `closed` once the peer has
/// closed the connection.
fn read(received: &[u8], cap: usize, closed: bool) -> Result<Option<Response>> {
    let mut rest = received;
    loop {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut response = httparse::Response::new(&mut headers);
        // A head is read from no more than it may hold and one byte, so a
        // fault further on is never seen before the head is known too large:
        // what is read never depends on how much had arrived.
        let head = match response.parse(rest.get(..=HEAD_CAP).unwrap_or(rest)) {
            Ok(httparse::Status::Complete(head)) => head,
            Ok(httparse::Status::Partial) if rest.len() > HEAD_CAP => return Err(Error::TooLarge),
            Ok(httparse::Status::Partial) => return pending(closed),
            Err(httparse::Error::TooManyHeaders) => return Err(Error::TooLarge),
            Err(_) => return Err(Error::Http),
        };
        if head > HEAD_CAP {
            return Err(Error::TooLarge);
        }
        let status = response.code.ok_or(Error::Http)?;
        let body = rest.get(head..).unwrap_or_default();
        match status {
            // An interim answer; the response proper follows it.
            100 | 102..=199 => {
                rest = body;
                continue;
            }
            101 => return Err(Error::Http),
            300..=399 => return Err(Error::Redirect(status)),
            204 => {
                return Ok(Some(Response {
                    status,
                    body: Vec::new(),
                }));
            }
            _ => {}
        }
        let body = match framing(response.headers)? {
            Framing::Length(len) if len > cap => return Err(Error::TooLarge),
            Framing::Length(len) => match body.get(..len) {
                Some(whole) => Some(whole.to_vec()),
                None => return pending(closed),
            },
            Framing::Chunked => dechunk(body, cap, closed)?,
            Framing::Close if body.len() > cap => return Err(Error::TooLarge),
            Framing::Close => closed.then(|| body.to_vec()),
        };
        return Ok(body.map(|body| Response { status, body }));
    }
}

/// More is needed: wait for it, unless nothing more will come.
fn pending<T>(closed: bool) -> Result<Option<T>> {
    if closed {
        Err(Error::Truncated)
    } else {
        Ok(None)
    }
}

/// The body's framing from the three headers that state it. Only those are
/// read as text: any other may carry bytes that are not, as the protocol
/// allows, and is none of this side's business.
fn framing(headers: &[httparse::Header<'_>]) -> Result<Framing> {
    let mut length = None;
    let mut chunked = false;
    for header in headers {
        let framing = ["transfer-encoding", "content-length", "content-encoding"]
            .iter()
            .any(|name| header.name.eq_ignore_ascii_case(name));
        if !framing {
            continue;
        }
        let value = core::str::from_utf8(header.value)
            .map_err(|_| Error::Framing)?
            .trim();
        if header.name.eq_ignore_ascii_case("transfer-encoding") {
            for coding in value.split(',').map(str::trim).filter(|c| !c.is_empty()) {
                if coding.eq_ignore_ascii_case("chunked") {
                    chunked = true;
                } else if !coding.eq_ignore_ascii_case("identity") {
                    return Err(Error::Framing);
                }
            }
        } else if header.name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Framing);
            }
            // Digits only, so the one failure left is a number too large.
            let len: usize = value.parse().map_err(|_| Error::TooLarge)?;
            if length.is_some_and(|seen| seen != len) {
                return Err(Error::Framing);
            }
            length = Some(len);
        } else if header.name.eq_ignore_ascii_case("content-encoding")
            && !value.is_empty()
            && !value.eq_ignore_ascii_case("identity")
        {
            return Err(Error::Framing);
        }
    }
    Ok(match (chunked, length) {
        (true, _) => Framing::Chunked,
        (false, Some(len)) => Framing::Length(len),
        (false, None) => Framing::Close,
    })
}

/// A chunked body, if it has ended.
fn dechunk(mut rest: &[u8], cap: usize, closed: bool) -> Result<Option<Vec<u8>>> {
    let mut body = Vec::new();
    loop {
        let (line, size) = match httparse::parse_chunk_size(rest) {
            Ok(httparse::Status::Complete(found)) => found,
            Ok(httparse::Status::Partial) => return pending(closed),
            Err(_) => return Err(Error::Framing),
        };
        rest = rest.get(line..).unwrap_or_default();
        if size == 0 {
            // Trailers end at an empty line; the close after the last chunk
            // ends them as well.
            let ended = rest.starts_with(b"\r\n") || rest.windows(4).any(|w| w == b"\r\n\r\n");
            return if ended || closed {
                Ok(Some(body))
            } else {
                Ok(None)
            };
        }
        let size = usize::try_from(size).map_err(|_| Error::TooLarge)?;
        if size > cap.saturating_sub(body.len()) {
            return Err(Error::TooLarge);
        }
        let Some(data) = rest.get(..size) else {
            return pending(closed);
        };
        match rest.get(size..).unwrap_or_default() {
            [b'\r', b'\n', after @ ..] => {
                body.extend_from_slice(data);
                rest = after;
            }
            [] | [b'\r'] => return pending(closed),
            _ => return Err(Error::Framing),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::Ipv4Addr;

    const GATEWAY: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 5000);

    /// Every split of `wire` into two pieces reads to `expected`, and no
    /// prefix alone reads as whole.
    fn every_split(wire: &[u8], cap: usize, expected: &Response) {
        for at in 0..=wire.len() {
            let mut reader = Reader::new(cap);
            let first = reader.push(&wire[..at]).unwrap();
            if at < wire.len() {
                assert_eq!(first, None, "a prefix of {at} bytes read as whole");
                assert_eq!(reader.push(&wire[at..]).unwrap().as_ref(), Some(expected));
            } else {
                assert_eq!(first.as_ref(), Some(expected));
            }
        }
    }

    fn ok(body: &[u8]) -> Response {
        Response {
            status: 200,
            body: body.to_vec(),
        }
    }

    #[test]
    fn a_get_asks_for_a_close() {
        assert_eq!(
            request(Method::Get, GATEWAY, "/rootDesc.xml", &[], b"ignored"),
            b"GET /rootDesc.xml HTTP/1.1\r\nHost: 192.168.1.1:5000\r\nConnection: close\r\n\r\nignored"
        );
    }

    #[test]
    fn a_post_states_its_length() {
        assert_eq!(
            request(
                Method::Post,
                GATEWAY,
                "/ctl/IPConn",
                &[("SOAPAction", "\"x#y\"")],
                b"<a/>"
            ),
            b"POST /ctl/IPConn HTTP/1.1\r\nHost: 192.168.1.1:5000\r\nConnection: close\r\n\
              SOAPAction: \"x#y\"\r\nContent-Length: 4\r\n\r\n<a/>"
        );
    }

    #[test]
    fn a_stated_length_ends_the_body() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloTRAILING";
        every_split(&wire[..wire.len() - 8], 64, &ok(b"hello"));
        // Bytes past the stated length are not the body's.
        assert_eq!(Reader::new(64).push(wire).unwrap(), Some(ok(b"hello")));
    }

    #[test]
    fn chunks_are_joined_with_extensions_and_trailers() {
        let wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                     4;name=v\r\nWiki\r\n5\r\npedia\r\n0\r\nExpires: never\r\n\r\n";
        every_split(wire, 64, &ok(b"Wikipedia"));
        let bare = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n0\r\n\r\n";
        every_split(bare, 64, &ok(b"Wiki"));
    }

    #[test]
    fn the_close_ends_a_body_with_no_framing() {
        let wire = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nall of it";
        let mut reader = Reader::new(64);
        assert_eq!(reader.push(wire).unwrap(), None);
        assert_eq!(reader.finish().unwrap(), ok(b"all of it"));
        // The close after the last chunk ends its trailers.
        let mut chunked = Reader::new(64);
        let wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n";
        assert_eq!(chunked.push(wire).unwrap(), None);
        assert_eq!(chunked.finish().unwrap(), ok(b"abc"));
    }

    #[test]
    fn a_close_before_the_end_is_a_truncation() {
        for wire in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel"[..],
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Le"[..],
            &b""[..],
        ] {
            let mut reader = Reader::new(64);
            assert_eq!(reader.push(wire).unwrap(), None);
            assert_eq!(reader.finish(), Err(Error::Truncated));
        }
    }

    #[test]
    fn a_body_is_read_whatever_the_status() {
        let wire = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 6\r\n\r\n<fault";
        assert_eq!(
            Reader::new(64).push(wire).unwrap(),
            Some(Response {
                status: 500,
                body: b"<fault".to_vec()
            })
        );
    }

    #[test]
    fn an_interim_answer_is_passed_over() {
        let wire = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        every_split(wire, 64, &ok(b"ok"));
    }

    #[test]
    fn a_redirect_is_refused() {
        let wire = b"HTTP/1.1 301 Moved Permanently\r\nLocation: http://192.0.2.1/\r\n\r\n";
        assert_eq!(Reader::new(64).push(wire), Err(Error::Redirect(301)));
    }

    #[test]
    fn sizes_are_refused_before_they_are_held() {
        // A stated length past the cap, refused before its body arrives.
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 65\r\n\r\n";
        assert_eq!(Reader::new(64).push(wire), Err(Error::TooLarge));
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999999\r\n\r\n";
        assert_eq!(Reader::new(64).push(wire), Err(Error::TooLarge));
        // Chunks adding up past it.
        let wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n20\r\n";
        let mut reader = Reader::new(48);
        assert_eq!(reader.push(wire).unwrap(), None);
        let mut chunk = vec![b'x'; 32];
        chunk.extend_from_slice(b"\r\n11\r\n");
        assert_eq!(reader.push(&chunk), Err(Error::TooLarge));
        // A body framed by the close.
        let mut wire = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        wire.resize(wire.len() + 65, b'x');
        assert_eq!(Reader::new(64).push(&wire), Err(Error::TooLarge));
        // A header block that never ends.
        let mut head = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
        head.resize(HEAD_CAP + 1, b'a');
        assert_eq!(Reader::new(64).push(&head), Err(Error::TooLarge));
        // More headers than are read.
        let mut many = b"HTTP/1.1 200 OK\r\n".to_vec();
        for _ in 0..=MAX_HEADERS {
            many.extend_from_slice(b"X: y\r\n");
        }
        many.extend_from_slice(b"\r\n");
        assert_eq!(Reader::new(64).push(&many), Err(Error::TooLarge));
        // Framing that grows while the body does not: one-byte chunks with
        // long extensions, past the bound on everything held.
        let mut framed = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for _ in 0..60 {
            framed.extend_from_slice(b"1;");
            framed.extend_from_slice(&[b'e'; 200]);
            framed.extend_from_slice(b"\r\nx\r\n");
        }
        let mut reader = Reader::new(64);
        assert_eq!(reader.push(&framed[..4000]).unwrap(), None);
        assert_eq!(reader.push(&framed[4000..]), Err(Error::TooLarge));
    }

    #[test]
    fn a_whole_response_is_whole_whatever_follows_it() {
        let mut wire = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec();
        wire.resize(wire.len() + 3 * HEAD_CAP, b'!');
        assert_eq!(Reader::new(64).push(&wire).unwrap(), Some(ok(b"ok")));
        // The same bytes in two reads read the same.
        let mut reader = Reader::new(64);
        assert_eq!(reader.push(&wire[..20]).unwrap(), None);
        assert_eq!(reader.push(&wire[20..]).unwrap(), Some(ok(b"ok")));
    }

    /// A fault past the head's cap is never seen before the cap is: read
    /// whole or a byte at a time, the same refusal.
    #[test]
    fn a_head_past_its_cap_is_refused_alike_at_every_cut() {
        let mut wire = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
        wire.resize(8200, b'a');
        wire.extend_from_slice(b"\x00\r\n\r\n");
        let whole = Reader::new(512).push(&wire).map(|_| ());
        for step in [1, 7, 64, 4096] {
            let mut reader = Reader::new(512);
            let mut outcome = Ok(());
            for piece in wire.chunks(step) {
                match reader.push(piece) {
                    Ok(None) => {}
                    Ok(Some(_)) => break,
                    Err(error) => {
                        outcome = Err(error);
                        break;
                    }
                }
            }
            assert_eq!(outcome, whole, "read {step} bytes at a time");
        }
        assert_eq!(whole, Err(Error::TooLarge));
    }

    /// A header this side does not read may hold any byte the protocol
    /// allows; only the framing's own must be text.
    #[test]
    fn a_header_not_read_may_hold_any_byte() {
        let wire = b"HTTP/1.1 200 OK\r\nServer: Caf\xe9 Router\r\nContent-Length: 2\r\n\r\nok";
        every_split(wire, 64, &ok(b"ok"));
        let framing = b"HTTP/1.1 200 OK\r\nContent-Length: 2\xe9\r\n\r\nok";
        assert_eq!(Reader::new(64).push(framing), Err(Error::Framing));
    }

    #[test]
    fn codings_and_framings_not_taken() {
        for wire in [
            &b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 1\r\n\r\nx"[..],
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nxx"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n"[..],
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n"[..],
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nxyz"[..],
        ] {
            assert_eq!(Reader::new(64).push(wire), Err(Error::Framing), "{wire:?}");
        }
        // The same length twice is one length.
        let twice = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\ncontent-length: 1\r\n\r\nx";
        assert_eq!(Reader::new(64).push(twice).unwrap(), Some(ok(b"x")));
    }

    #[test]
    fn not_http_is_refused() {
        assert_eq!(
            Reader::new(64).push(b"SSH-2.0-dropbear\r\n\r\n"),
            Err(Error::Http)
        );
        assert_eq!(
            Reader::new(64).push(b"HTTP/1.1 101 Switching\r\n\r\n"),
            Err(Error::Http)
        );
    }
}

#[cfg(test)]
mod captured {
    use super::*;

    #[test]
    fn every_captured_reply_reads_whole_at_every_split() {
        for (wire, status, len) in [
            (
                &include_bytes!("../tests/data/openwrt/description.http")[..],
                200,
                2581,
            ),
            (
                include_bytes!("../tests/data/openwrt/external-address.http"),
                200,
                357,
            ),
            (include_bytes!("../tests/data/openwrt/add.http"), 200, 263),
            (include_bytes!("../tests/data/openwrt/entry.http"), 200, 540),
            (
                include_bytes!("../tests/data/openwrt/no-such-entry.http"),
                500,
                402,
            ),
            (
                include_bytes!("../tests/data/openwrt/delete.http"),
                200,
                295,
            ),
            (
                include_bytes!("../tests/data/f670y/description.http"),
                200,
                3982,
            ),
            (
                include_bytes!("../tests/data/f670y/external-address.http"),
                200,
                343,
            ),
            (include_bytes!("../tests/data/f670y/add.http"), 200, 268),
            (include_bytes!("../tests/data/f670y/entry.http"), 200, 531),
            (
                include_bytes!("../tests/data/f670y/no-such-entry.http"),
                500,
                416,
            ),
            (
                include_bytes!("../tests/data/f670y/action-failed.http"),
                500,
                411,
            ),
            (include_bytes!("../tests/data/f670y/delete.http"), 200, 274),
            (
                include_bytes!("../tests/data/debian/description.http"),
                200,
                2936,
            ),
            (
                include_bytes!("../tests/data/debian/external-address-empty.http"),
                200,
                346,
            ),
        ] {
            let expected = Response {
                status,
                body: wire[wire.len() - len..].to_vec(),
            };
            for at in 0..wire.len() {
                let mut reader = Reader::new(DESCRIPTION_CAP);
                assert_eq!(reader.push(&wire[..at]).unwrap(), None);
                assert_eq!(reader.push(&wire[at..]).unwrap().as_ref(), Some(&expected));
            }
        }
    }
}
