//! The connecting side's declared preferences.
//!
//! Session initialization is opcode 11 on the control channel: a length in the
//! message's own arguments and a NUL-terminated flat JSON object after the
//! header. See docs/01-protocol.md section 11.5.
//!
//! **Read by name rather than tokenised.** The object is flat, every value is
//! an integer, and a host must tolerate keys it does not know: peers exist
//! that behave differently when the object carries more than the eight it is
//! supposed to, so the shape is not something to enforce. Scanning for the
//! keys we care about accepts extras without a parser that has to model them,
//! and with no string values in the object there is nothing a key name can
//! hide inside.
//!
//! Nothing here allocates, so the whole of it runs in the sans-IO core.

use crate::error::{Error, Result};

/// The only wire-compatible version.
pub const VERSION: u32 = 1;

/// What `_max_w` and `_max_h` carry when the peer has no limit.
///
/// **Not a resolution.** A host reading it as one tries to encode a picture
/// sixty thousand samples wide.
pub const NO_LIMIT: u32 = 60000;

/// HEVC support.
pub const FLAG_HEVC: u32 = 0x01;
/// 4:4:4 chroma, which implies HEVC.
pub const FLAG_COLOR444: u32 = 0x02;
/// **Set on every offer.** A base flag rather than a capability, so its
/// presence says nothing and its absence would be the surprise.
pub const FLAG_BASE: u32 = 0x08;
/// Ten-bit, which implies HEVC. **Bit four, not bit two**, and reading it at
/// bit two is a mistake that has been made before.
pub const FLAG_10BIT: u32 = 0x10;

/// What the connecting side asked for.
///
/// Every field but the version has a default, so a key the peer left out is a
/// default rather than a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Init {
    pub version: u32,
    /// Largest picture the peer will accept, or [`NO_LIMIT`].
    pub max_width: u32,
    pub max_height: u32,
    pub flags: u32,
    /// Preferred picture size, or zero for no preference.
    pub resolution_x: u32,
    pub resolution_y: u32,
    pub media_container: u32,
    pub refresh_rate: u32,
    /// The peer keeps pointer images it has been sent and will accept one
    /// named by its checksum instead of resent.
    ///
    /// **A peer that did not say this must be sent the picture every time.**
    /// Naming one it never kept leaves it drawing whatever it last had, which
    /// is a pointer stuck in the shape it happened to be in when the guest
    /// joined.
    pub caches_cursor: bool,
}

impl Init {
    pub const fn hevc(&self) -> bool {
        self.flags & FLAG_HEVC != 0
    }

    pub const fn color444(&self) -> bool {
        self.flags & FLAG_COLOR444 != 0
    }

    pub const fn ten_bit(&self) -> bool {
        self.flags & FLAG_10BIT != 0
    }

    /// True when the peer stated a size it wants rather than leaving it to us.
    pub const fn has_preferred_size(&self) -> bool {
        self.resolution_x != 0 && self.resolution_y != 0
    }

    /// True when the peer stated a ceiling rather than declaring none.
    ///
    /// **Zero is not a ceiling.** A peer that leaves these out, and at least
    /// one that sends them as zero, means no limit exactly as [`NO_LIMIT`]
    /// does; reading either as a size gives a ceiling of nothing, and anything
    /// that sized a picture from it would encode a picture of no width.
    pub const fn has_size_limit(&self) -> bool {
        self.max_width != NO_LIMIT
            && self.max_height != NO_LIMIT
            && self.max_width != 0
            && self.max_height != 0
    }
}

/// Find `"key"` used as a key, and return the bytes after its colon.
fn value_after<'a>(body: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let key = key.as_bytes();
    let mut at = 0usize;
    while at + key.len() + 2 <= body.len() {
        let quote = body.get(at..).and_then(|rest| {
            rest.iter()
                .position(|byte| *byte == b'"')
                .map(|offset| at + offset)
        })?;
        let name = body.get(quote + 1..quote + 1 + key.len())?;
        let closing = body.get(quote + 1 + key.len()).copied();
        if name == key && closing == Some(b'"') {
            let mut cursor = quote + 2 + key.len();
            while body.get(cursor).copied().is_some_and(is_space) {
                cursor += 1;
            }
            if body.get(cursor).copied() != Some(b':') {
                // A string that happens to match a key name, used as a value.
                at = quote + 1;
                continue;
            }
            cursor += 1;
            while body.get(cursor).copied().is_some_and(is_space) {
                cursor += 1;
            }
            return body.get(cursor..);
        }
        at = quote + 1;
    }
    None
}

const fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// Read an unsigned integer, stopping at the first byte that is not a digit.
///
/// A negative or fractional value is not something these fields carry, and
/// clamping one to zero is a better answer than refusing the whole object over
/// a field that has a default anyway.
fn number(bytes: &[u8], fallback: u32) -> u32 {
    let mut value = 0u32;
    let mut digits = 0usize;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value
            .saturating_mul(10)
            .saturating_add(u32::from(byte - b'0'));
        digits += 1;
    }
    if digits == 0 { fallback } else { value }
}

fn field(body: &[u8], key: &str, fallback: u32) -> u32 {
    value_after(body, key).map_or(fallback, |rest| number(rest, fallback))
}

/// Read a boolean, which is absent far more often than it is false.
///
/// **Only a literal `true` counts.** A key a peer left out, a key it sent as
/// `false`, and a key carrying something else are all the same answer, and it
/// is the one that costs nothing to be wrong about: the picture is sent rather
/// than named.
fn truth(body: &[u8], key: &str) -> bool {
    value_after(body, key).is_some_and(|rest| rest.starts_with(b"true"))
}

/// Parse an initialization body.
///
/// `body` is everything after the control header, terminating NUL included.
///
/// **Only the version is mandatory**, and it must be [`VERSION`]. Refusing on
/// anything else would reject peers over fields that have defaults, and the
/// host is authoritative over all of them regardless.
pub fn parse(body: &[u8]) -> Result<Init> {
    let body = match body.split_last() {
        Some((0, head)) => head,
        _ => body,
    };
    if body.first() != Some(&b'{') {
        return Err(Error::Malformed);
    }
    let version = field(body, "_version", 0);
    if version != VERSION {
        return Err(Error::Malformed);
    }
    Ok(Init {
        version,
        max_width: field(body, "_max_w", 0),
        max_height: field(body, "_max_h", 0),
        flags: field(body, "_flags", 0),
        resolution_x: field(body, "resolutionX", 0),
        resolution_y: field(body, "resolutionY", 0),
        media_container: field(body, "mediaContainer", 0),
        refresh_rate: field(body, "refreshRate", 60),
        caches_cursor: truth(body, "_cache_cursor"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Verbatim from a recorded session**, NUL included. This is the whole
    /// point of the fixture: a body we wrote ourselves would only prove the
    /// parser agrees with the writer.
    const RECORDED: &[u8] = b"{\"_version\":1,\"_max_w\":60000,\"_max_h\":60000,\"_flags\":8,\
\"resolutionX\":0,\"resolutionY\":0,\"mediaContainer\":0,\"refreshRate\":60}\0";

    /// **Zero is not a ceiling.** A live client declared no maximum at all,
    /// which reads as zero here, and a host that took it for a size would
    /// encode a picture of no width. Both spellings of "no limit" have to
    /// answer the same.
    #[test]
    fn no_maximum_is_not_a_maximum_of_nothing() {
        let none = Init {
            version: VERSION,
            max_width: 0,
            max_height: 0,
            flags: FLAG_BASE,
            resolution_x: 2560,
            resolution_y: 1440,
            media_container: 0,
            refresh_rate: 60,
            caches_cursor: false,
        };
        assert!(!none.has_size_limit(), "zero was read as a ceiling");

        let sentinel = Init {
            max_width: NO_LIMIT,
            max_height: NO_LIMIT,
            ..none
        };
        assert!(!sentinel.has_size_limit());

        // A real ceiling still reads as one.
        let real = Init {
            max_width: 1920,
            max_height: 1080,
            ..none
        };
        assert!(real.has_size_limit(), "a stated ceiling was discarded");
    }

    #[test]
    fn a_recorded_client_body_parses_field_for_field() {
        assert_eq!(
            RECORDED.len(),
            124,
            "the fixture is not the recorded length"
        );
        let init = parse(RECORDED).expect("a recorded body was refused");
        assert_eq!(init.version, 1);
        assert_eq!(init.max_width, NO_LIMIT);
        assert_eq!(init.max_height, NO_LIMIT);
        assert_eq!(init.flags, FLAG_BASE);
        assert_eq!(init.resolution_x, 0);
        assert_eq!(init.resolution_y, 0);
        assert_eq!(init.media_container, 0);
        assert_eq!(init.refresh_rate, 60);
    }

    /// The base flag alone is the ordinary case, and it is not a capability.
    #[test]
    fn the_base_flag_alone_asks_for_nothing() {
        let init = parse(RECORDED).expect("parsed");
        assert!(!init.hevc(), "the base flag was read as a codec request");
        assert!(!init.color444());
        assert!(!init.ten_bit());
    }

    /// **Bit two is not ten-bit.** Setting it must not turn ten-bit on.
    #[test]
    fn ten_bit_is_bit_four_and_bit_two_means_nothing_here() {
        let with = |flags: u32| Init {
            version: 1,
            max_width: 0,
            max_height: 0,
            flags,
            resolution_x: 0,
            resolution_y: 0,
            media_container: 0,
            refresh_rate: 60,
            caches_cursor: false,
        };
        assert!(!with(0x04).ten_bit(), "bit two was read as ten-bit");
        assert!(with(0x10).ten_bit());
        assert!(with(0x13).hevc() && with(0x13).color444() && with(0x13).ten_bit());
    }

    /// Sentinels, not measurements. A host that took these literally would try
    /// to encode a picture nobody asked for.
    #[test]
    fn the_sentinels_are_not_sizes() {
        let init = parse(RECORDED).expect("parsed");
        assert!(!init.has_size_limit(), "no-limit was read as a limit");
        assert!(
            !init.has_preferred_size(),
            "no-preference was read as a size"
        );

        let stated = parse(b"{\"_version\":1,\"_max_w\":2560,\"_max_h\":1440,\"resolutionX\":1920,\"resolutionY\":1080}")
            .expect("parsed");
        assert!(stated.has_size_limit());
        assert!(stated.has_preferred_size());
        assert_eq!((stated.max_width, stated.max_height), (2560, 1440));
    }

    #[test]
    fn only_the_version_is_mandatory_and_it_must_be_one() {
        assert!(
            parse(b"{\"_version\":1}").is_ok(),
            "a minimal body was refused"
        );
        assert_eq!(
            parse(b"{\"_version\":1}").expect("parsed").refresh_rate,
            60,
            "a missing key did not take its default"
        );
        for body in [
            &b"{\"_version\":0}"[..],
            b"{\"_version\":2}",
            b"{}",
            b"{\"_max_w\":1920}",
        ] {
            assert_eq!(parse(body), Err(Error::Malformed), "accepted {body:?}");
        }
    }

    /// A host must tolerate keys it does not know: peers send different
    /// objects, and requiring a shape would refuse them over fields nothing
    /// reads.
    #[test]
    fn unknown_keys_are_ignored_rather_than_refused() {
        let init = parse(
            b"{\"_version\":1,\"rawAudio\":true,\"_cache_cursor\":true,\"_flags\":9,\
              \"resolutions\":[[1920,1080]],\"refreshRate\":144}",
        )
        .expect("extras were refused");
        assert_eq!(init.flags, 9);
        assert_eq!(init.refresh_rate, 144);
        assert!(init.hevc());
        // Not an unknown key any more: a real peer states its pointer cache
        // here and nowhere else.
        assert!(init.caches_cursor);
    }

    /// **A peer that did not say it caches must be sent the picture every
    /// time.** Absent, false, and anything unexpected are one answer, and it
    /// is the one that costs a few hundred bytes rather than leaving a peer
    /// drawing whatever pointer it last had.
    #[test]
    fn only_a_stated_cursor_cache_counts() {
        let of = |body: &[u8]| parse(body).expect("parsed").caches_cursor;
        assert!(of(b"{\"_version\":1,\"_cache_cursor\":true}"));
        assert!(of(b"{\"_version\":1, \"_cache_cursor\" : true }"));
        assert!(!of(b"{\"_version\":1,\"_cache_cursor\":false}"));
        assert!(!of(b"{\"_version\":1}"));
        // A key that merely begins the same way is a different key.
        assert!(!of(b"{\"_version\":1,\"_cache_cursors\":true}"));
    }

    #[test]
    fn junk_is_refused_rather_than_read_as_defaults() {
        for body in [&b""[..], b"not json", b"[]", b"[1,2,3]"] {
            assert_eq!(parse(body), Err(Error::Malformed), "accepted {body:?}");
        }
    }

    /// A trailing NUL is present on the wire and must not become a parse
    /// failure or a stray digit.
    #[test]
    fn the_terminator_is_optional_and_never_part_of_a_value() {
        let with = parse(b"{\"_version\":1,\"refreshRate\":30}\0").expect("with NUL");
        let without = parse(b"{\"_version\":1,\"refreshRate\":30}").expect("without NUL");
        assert_eq!(with, without);
        assert_eq!(with.refresh_rate, 30);
    }
}
