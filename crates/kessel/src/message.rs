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
