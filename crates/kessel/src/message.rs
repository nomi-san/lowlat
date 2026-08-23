//! The message set.
//!
//! Every message is `{ version, action, payload }`. Field order within a
//! payload is the declaration order of these structs, which is deliberate:
//! a strict parser on the far side is entitled to care, and matching the order
//! a peer emits costs nothing here while being invisible to find later.
//!
//! **Two fields are not the type they look like.** `app_v` is a string even
//! though it holds a build number, and the candidate's `port` is a number while
//! its `ip` is a dotted string -- the pair are easy to transpose and a peer
//! silently ignores a candidate that gets them wrong.

use serde::{Deserialize, Serialize};

/// Protocol generation of the message envelope itself.
pub const VERSION: u32 = 1;

/// What a host claims on each of the six independently versioned subprotocols.
///
/// **All ones, deliberately.** The number is a promise the peer holds us to,
/// and a higher value asks it to select framing generations we do not
/// implement. Raise an axis only when the framing behind it is actually built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versions {
    pub bud: u32,
    pub control: u32,
    pub p2p: u32,
    pub audio: u32,
    pub init: u32,
    pub video: u32,
}

impl Default for Versions {
    fn default() -> Self {
        Self {
            bud: 1,
            control: 1,
            p2p: 1,
            audio: 1,
            init: 1,
            video: 1,
        }
    }
}

/// The block every offer, answer and candidate carries.
///
/// `ver_data` must be non-zero on a candidate or the peer rejects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDataBase {
    pub ver_data: u32,
    pub versions: Versions,
}

impl Default for HostDataBase {
    fn default() -> Self {
        Self {
            ver_data: 1,
            versions: Versions::default(),
        }
    }
}

/// One guest as it appears in the advertisement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guest {
    pub guest_id: u32,
    pub user_id: u32,
    pub gamepad: bool,
    pub keyboard: bool,
    pub mouse: bool,
}

/// The advertisement that publishes a host into the discovery listing.
///
/// Without it the host exists and is reachable but cannot be found. Emitted on
/// state change only, never on a timer: the service derives liveness from the
/// connection itself, so a periodic advertisement adds load and buys nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnUpdate {
    pub loader_v: u32,
    pub service_v: u32,
    /// Operating system name.
    pub os: String,
    pub os_v: String,
    /// Platform family. A string, and distinct from `os`.
    pub platform: String,
    /// **A string, not a number**, whatever a schema says.
    pub app_v: String,
    pub sdk_v: u32,
    pub device_id: String,
    /// `desktop` or `game`. Only the former is implemented.
    pub mode: String,
    pub name: String,
    pub desc: String,
    pub game_id: String,
    /// Empty unless invite-only, and at least eight characters when set.
    pub secret: String,
    /// Read from the configured guest limit, never a constant. A listing that
    /// promises more capacity than admission will grant is a listing that lies.
    pub max_players: u32,
    pub players: u32,
    /// Serialized as `public`, which is a keyword in more than one language.
    #[serde(rename = "public")]
    pub is_public: bool,
    pub guests: Vec<Guest>,
}

/// Credentials for one attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// Present selects the 256-bit cipher; absent selects the legacy path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aes256: Option<String>,
    pub fingerprint: String,
    pub ice_ufrag: String,
    pub ice_pwd: String,
}

/// A candidate, as it goes on the wire.
///
/// `ip` is a dotted or colon-separated string and `port` is a number. The three
/// flags after them are the booleans; transposing `port` into that group is the
/// mistake this layout exists to make hard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateData {
    #[serde(flatten)]
    pub base: HostDataBase,
    pub ip: String,
    pub port: u16,
    pub lan: bool,
    pub from_stun: bool,
    pub sync: bool,
}

/// The host's reply to an offer. Approval carries the credentials the session
/// is keyed from, because the key that encrypts it is the host's.
#[derive(Debug, Clone, Serialize)]
pub struct Answer<'a> {
    pub approved: bool,
    pub attempt_id: &'a str,
    pub data: AnswerData<'a>,
    /// The peer, which is the offer's `from`.
    pub to: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnswerData<'a> {
    #[serde(flatten)]
    pub base: HostDataBase,
    pub creds: &'a Credentials,
}

/// Credentials that carry nothing, for an answer that refuses.
///
/// A refusal is read for its `approved` flag and closed on; the credential
/// object is still present because the shape is the same either way.
pub fn no_credentials() -> Credentials {
    Credentials {
        aes256: None,
        fingerprint: String::new(),
        ice_ufrag: String::new(),
        ice_pwd: String::new(),
    }
}

/// One candidate, or a readiness marker.
#[derive(Debug, Clone, Serialize)]
pub struct Candex<'a> {
    pub attempt_id: &'a str,
    pub data: CandidateData,
    pub to: &'a str,
}

/// What a peer told us, in the shape the relay forwards.
#[derive(Debug, Clone, Deserialize)]
pub struct OfferRelay {
    pub attempt_id: String,
    pub from: String,
    pub data: OfferData,
    #[serde(default)]
    pub skip_approval: bool,
    /// The peer owns this machine, which is what lets it take the pointer from
    /// another guest ([05 §7.1](../../../docs/05-host.md)).
    #[serde(default)]
    pub is_owner: bool,
    /// What the peer may drive.
    ///
    /// **Read here and never from the peer**, which is the whole reason it
    /// arrives relayed rather than in the offer's own data
    /// ([04 §3](../../../docs/04-signaling.md)).
    #[serde(default)]
    pub permissions: Permissions,
}

/// What a guest may drive.
///
/// **Everything, when the field is absent.** The service sends it on every
/// offer; a host that read silence as a refusal would deny input to a peer
/// whose service simply did not say, and the failure would look like broken
/// input rather than a policy.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Permissions {
    #[serde(default = "yes")]
    pub keyboard: bool,
    #[serde(default = "yes")]
    pub mouse: bool,
    #[serde(default = "yes")]
    pub gamepad: bool,
}

const fn yes() -> bool {
    true
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            keyboard: true,
            mouse: true,
            gamepad: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OfferData {
    pub creds: PeerCredentials,
}

/// The peer's half of the credentials. Only the check fields are used; the
/// media key is the host's, and a peer supplying one is signalling support.
#[derive(Debug, Clone, Deserialize)]
pub struct PeerCredentials {
    pub ice_ufrag: String,
    pub ice_pwd: String,
    #[serde(default)]
    pub aes256: Option<String>,
}

/// What a relayed candidate exchange asks a host to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Relayed {
    /// A readiness barrier. **Its address is ignored and is not parsed**, which
    /// is the whole reason this is decided before the address is looked at.
    Ready,
    /// An address to probe.
    Probe(core::net::SocketAddr),
    /// Not an address a host can probe.
    Unreadable,
}

impl RelayedCandidate {
    /// Read one relayed candidate exchange.
    ///
    /// **The barrier is decided first.** A peer sends one candidate marked
    /// ready and the receiver ignores the address on it, so peers put different
    /// things there: captures carry both the well-known placeholder and a
    /// sender's own reflexive address. Parsing before checking the flag makes
    /// the barrier depend on a field nothing reads, and a barrier that is
    /// dropped leaves a peer that withholds its real candidates waiting for
    /// something that already arrived -- silent at both ends
    /// ([04 §3](../../../docs/04-signaling.md)).
    ///
    /// **The address is parsed, never edited as text.** A v4-mapped address is
    /// IPv4 and has two textual forms: the trailing bytes may be written
    /// dotted, as `::ffff:192.0.2.7`, or in hex, as `::ffff:c000:207`. Stripping
    /// the prefix as text handles the first and turns the second into a
    /// fragment that parses as nothing, so that candidate is dropped without a
    /// word and the peer is never probed there. The parser knows both forms,
    /// and collapsing the result to the IPv4 it is belongs to the connectivity
    /// engine, which does it to every address it is handed.
    pub fn read(&self) -> Relayed {
        if self.sync {
            return Relayed::Ready;
        }
        match self.ip.trim().parse::<core::net::IpAddr>() {
            Ok(ip) => Relayed::Probe(core::net::SocketAddr::new(ip, self.port)),
            Err(_) => Relayed::Unreadable,
        }
    }
}

/// A candidate forwarded from the peer.
#[derive(Debug, Clone, Deserialize)]
pub struct CandexRelay {
    pub attempt_id: String,
    pub data: RelayedCandidate,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelayedCandidate {
    pub ip: String,
    pub port: u16,
    #[serde(default)]
    pub sync: bool,
}

/// A withdrawal, which is addressed by attempt and carries no reason.
#[derive(Debug, Clone, Deserialize)]
pub struct CancelRelay {
    pub attempt_id: String,
}

/// An envelope with a payload that has not been interpreted yet.
#[derive(Debug, Clone, Deserialize)]
pub struct Inbound {
    pub action: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Serialize one message, envelope and all.
pub fn envelope<T: Serialize>(action: &str, payload: &T) -> serde_json::Result<String> {
    #[derive(Serialize)]
    struct Envelope<'a, T> {
        version: u32,
        action: &'a str,
        payload: &'a T,
    }
    serde_json::to_string(&Envelope {
        version: VERSION,
        action,
        payload,
    })
}

#[cfg(test)]
mod tests {
    /// **A readiness barrier is not a candidate and must not need to parse.**
    ///
    /// The receiver ignores the address on one and peers put different things
    /// there -- a capture carries both the well-known placeholder and a peer's
    /// own reflexive address, and a peer that anonymises its host candidates
    /// behind a `.local` name could put one of those there too. Deciding the
    /// barrier after parsing drops it, and a peer that withholds its real
    /// candidates until the barrier arrives then waits for what already came.
    #[test]
    fn a_readiness_barrier_does_not_depend_on_its_address() {
        let marker = |ip: &str| RelayedCandidate {
            ip: ip.to_string(),
            port: 1234,
            sync: true,
        };
        assert_eq!(
            marker("1c4d9ae8-f7a8-4513-affb-dcbb40048922.local").read(),
            Relayed::Ready,
            "a marker with an unreadable address lost the barrier"
        );
        assert_eq!(marker("1.2.3.4").read(), Relayed::Ready);
        assert_eq!(marker("::ffff:171.246.76.160").read(), Relayed::Ready);
    }

    /// **A v4-mapped address has two textual forms and both are IPv4.**
    ///
    /// The hex form is the one a textual strip loses: taking `::ffff:` off the
    /// front of it leaves `c000:207`, which is not an address, so the candidate
    /// goes in the bin without a log line and the peer is never probed there.
    #[test]
    fn a_v4_mapped_candidate_reads_in_either_textual_form() {
        let candidate = |ip: &str| RelayedCandidate {
            ip: ip.to_string(),
            port: 41000,
            sync: false,
        };
        let dotted = candidate("::ffff:192.0.2.7").read();
        let hex = candidate("::ffff:c000:207").read();

        assert_eq!(dotted, hex, "the two spellings named different addresses");
        assert!(matches!(dotted, Relayed::Probe(_)));
    }

    /// An ordinary candidate is an address to probe; anything else is declined
    /// rather than mistaken for one.
    #[test]
    fn an_ordinary_candidate_is_read_or_declined() {
        let candidate = |ip: &str| RelayedCandidate {
            ip: ip.to_string(),
            port: 31064,
            sync: false,
        };
        assert_eq!(
            candidate("2405:4802:d0f5:6ec0:c048:4183:5759:8357").read(),
            Relayed::Probe(core::net::SocketAddr::new(
                "2405:4802:d0f5:6ec0:c048:4183:5759:8357"
                    .parse()
                    .expect("reference"),
                31064
            ))
        );
        assert_eq!(
            candidate(" 203.0.113.9 ").read(),
            Relayed::Probe(core::net::SocketAddr::new(
                "203.0.113.9".parse().expect("reference"),
                31064
            ))
        );
        assert_eq!(
            candidate("1c4d9ae8-f7a8-4513-affb-dcbb40048922.local").read(),
            Relayed::Unreadable
        );
        assert_eq!(candidate("").read(), Relayed::Unreadable);
    }

    use super::*;

    fn advertisement() -> ConnUpdate {
        ConnUpdate {
            loader_v: 0,
            service_v: 0,
            os: "linux".into(),
            os_v: "unknown".into(),
            platform: "linux".into(),
            app_v: "150-104a".into(),
            sdk_v: 0x0006_0000,
            device_id: "device".into(),
            mode: "desktop".into(),
            name: "host".into(),
            desc: String::new(),
            game_id: String::new(),
            secret: String::new(),
            max_players: 4,
            players: 0,
            is_public: false,
            guests: Vec::new(),
        }
    }

    /// The advertisement's field order is what a strict parser sees, so it is
    /// pinned rather than left to whatever the struct happens to look like.
    #[test]
    fn the_advertisement_serializes_in_order() {
        let wire = envelope("conn_update", &advertisement()).expect("serialize");
        let expected = concat!(
            r#"{"version":1,"action":"conn_update","payload":{"#,
            r#""loader_v":0,"service_v":0,"os":"linux","os_v":"unknown","#,
            r#""platform":"linux","app_v":"150-104a","sdk_v":393216,"#,
            r#""device_id":"device","mode":"desktop","name":"host","desc":"","#,
            r#""game_id":"","secret":"","max_players":4,"players":0,"#,
            r#""public":false,"guests":[]}}"#
        );
        assert_eq!(wire, expected);
    }

    /// `app_v` holds a build number and is still a string. A schema says
    /// otherwise and the wire does not. *Named regression test.*
    #[test]
    fn the_build_version_is_a_string() {
        let wire = envelope("conn_update", &advertisement()).expect("serialize");
        assert!(wire.contains(r#""app_v":"150-104a""#), "{wire}");
        assert!(!wire.contains(r#""app_v":150"#), "{wire}");
    }

    /// A candidate's address is a string and its port is a number. Transposing
    /// them produces a candidate a peer accepts and silently ignores, which is
    /// the worst failure shape available. *Named regression test.*
    #[test]
    fn a_candidate_carries_a_string_address_and_a_numeric_port() {
        let candidate = CandidateData {
            base: HostDataBase::default(),
            ip: "203.0.113.7".into(),
            port: 41_000,
            lan: false,
            from_stun: true,
            sync: false,
        };
        let wire = serde_json::to_string(&candidate).expect("serialize");
        assert!(wire.contains(r#""ip":"203.0.113.7""#), "{wire}");
        assert!(wire.contains(r#""port":41000"#), "{wire}");
        assert!(wire.contains(r#""lan":false"#), "{wire}");
        assert!(wire.contains(r#""from_stun":true"#), "{wire}");
        assert!(wire.contains(r#""sync":false"#), "{wire}");
        // The version block rides at the same level, not nested under a key.
        assert!(wire.contains(r#""ver_data":1"#), "{wire}");
    }

    /// Absent means the legacy cipher, so an empty key must not appear at all
    /// rather than appear as null or as an empty string.
    #[test]
    fn an_absent_media_key_is_omitted_entirely() {
        let creds = Credentials {
            aes256: None,
            fingerprint: "fp".into(),
            ice_ufrag: "u".into(),
            ice_pwd: "p".into(),
        };
        let wire = serde_json::to_string(&creds).expect("serialize");
        assert!(!wire.contains("aes256"), "{wire}");
    }
}
