//! UPnP control: the actions a mapping needs, and their answers.
//!
//! An action is an envelope posted to its service's control address and named
//! in a header as well. Three properties are easy to get wrong:
//!
//! **Every argument the action declares is sent, in its declared order**, and
//! escaped: a strict gateway refuses an action missing one or out of order.
//!
//! **An answer is read whatever its status.** A fault comes as a server error
//! with its code in the body, and the codes are what a mapping's decisions
//! turn on: taken, permanent only, not authorized, no such entry.
//!
//! **The protocol is named in capitals.** A gateway may refuse it otherwise.

use core::net::Ipv4Addr;

use crate::{Error, Result};

/// The content type an action is posted as.
pub const CONTENT_TYPE: &str = "text/xml; charset=\"utf-8\"";
/// The header naming the action.
pub const ACTION_HEADER: &str = "SOAPAction";

const MAX_NODES: u32 = 4 * 1024;

/// An action ready to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub name: &'static str,
    /// The value of [`ACTION_HEADER`].
    pub header: String,
    pub body: String,
}

/// A fault's code, as a gateway sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultCode(pub u16);

impl FaultCode {
    /// Also what a gateway that takes only permanent mappings may answer to
    /// a lease.
    pub const INVALID_ARGS: Self = Self(402);
    pub const ACTION_FAILED: Self = Self(501);
    /// The internal client is not the address the request came from.
    pub const NOT_AUTHORIZED: Self = Self(606);
    /// Past the end of the mapping table.
    pub const ARRAY_INDEX_INVALID: Self = Self(713);
    pub const NO_SUCH_ENTRY: Self = Self(714);
    /// The port is mapped to another client.
    pub const CONFLICT: Self = Self(718);
    pub const SAME_PORT_REQUIRED: Self = Self(724);
    pub const ONLY_PERMANENT_LEASES: Self = Self(725);
    pub const REMOTE_HOST_WILDCARD_ONLY: Self = Self(726);
    pub const EXTERNAL_PORT_WILDCARD_ONLY: Self = Self(727);
}

/// An action's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Done, with the action's out arguments in the order the gateway sent
    /// them, their values trimmed.
    Done(Vec<(String, String)>),
    /// Refused.
    Fault(FaultCode),
}

impl Answer {
    /// An out argument of a completed action.
    pub fn get(&self, name: &str) -> Option<&str> {
        match self {
            Answer::Done(arguments) => arguments
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str()),
            Answer::Fault(_) => None,
        }
    }
}

/// Map UDP `port` to the same port on `client` for `lease_s` seconds, zero
/// asking for a mapping without a lease. `client` must be the address the
/// request is sent from.
pub fn add_port_mapping(
    service_type: &'static str,
    port: u16,
    client: Ipv4Addr,
    description: &str,
    lease_s: u32,
) -> Action {
    let port = port.to_string();
    action(
        service_type,
        "AddPortMapping",
        &[
            ("NewRemoteHost", ""),
            ("NewExternalPort", &port),
            ("NewProtocol", "UDP"),
            ("NewInternalPort", &port),
            ("NewInternalClient", &client.to_string()),
            ("NewEnabled", "1"),
            ("NewPortMappingDescription", description),
            ("NewLeaseDuration", &lease_s.to_string()),
        ],
    )
}

/// Delete the UDP mapping of `port`.
pub fn delete_port_mapping(service_type: &'static str, port: u16) -> Action {
    entry_action(service_type, "DeletePortMapping", port)
}

/// The UDP mapping of `port`: its client, description and remaining lease.
pub fn get_specific_port_mapping_entry(service_type: &'static str, port: u16) -> Action {
    entry_action(service_type, "GetSpecificPortMappingEntry", port)
}

/// The external address the gateway states.
pub fn get_external_ip_address(service_type: &'static str) -> Action {
    action(service_type, "GetExternalIPAddress", &[])
}

/// Whether the connection is up.
pub fn get_status_info(service_type: &'static str) -> Action {
    action(service_type, "GetStatusInfo", &[])
}

fn entry_action(service_type: &'static str, name: &'static str, port: u16) -> Action {
    action(
        service_type,
        name,
        &[
            ("NewRemoteHost", ""),
            ("NewExternalPort", &port.to_string()),
            ("NewProtocol", "UDP"),
        ],
    )
}

fn action(service_type: &'static str, name: &'static str, arguments: &[(&str, &str)]) -> Action {
    let mut body = String::from(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>",
    );
    body.push_str(&format!("<u:{name} xmlns:u=\"{service_type}\">"));
    for (key, value) in arguments {
        body.push_str(&format!("<{key}>"));
        escape(&mut body, value);
        body.push_str(&format!("</{key}>"));
    }
    body.push_str(&format!("</u:{name}></s:Body></s:Envelope>\r\n"));
    Action {
        name,
        header: format!("\"{service_type}#{name}\""),
        body,
    }
}

fn escape(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
}

/// Read the answer to the action `name` whatever its `status`.
pub fn parse(name: &str, status: u16, body: &[u8]) -> Result<Answer> {
    let not_xml = if status == 200 {
        Error::Xml
    } else {
        Error::Status(status)
    };
    // As a description is read: a byte that is not the protocol's encoding
    // never refuses the whole answer.
    let text = String::from_utf8_lossy(body);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let options = roxmltree::ParsingOptions {
        nodes_limit: MAX_NODES,
        ..roxmltree::ParsingOptions::default()
    };
    let document = roxmltree::Document::parse_with_options(text, options).map_err(|_| not_xml)?;
    // A fault's code wherever the envelope puts it, whatever the status.
    if let Some(code) = document
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "errorCode")
    {
        let code = code.text().unwrap_or_default().trim();
        return code
            .parse()
            .map(|code| Answer::Fault(FaultCode(code)))
            .map_err(|_| Error::Xml);
    }
    if status != 200 {
        return Err(Error::Status(status));
    }
    let response = document
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name().strip_suffix("Response") == Some(name))
        .ok_or(Error::Xml)?;
    let arguments = response
        .children()
        .filter(|c| c.is_element())
        .map(|c| {
            let value = c.text().unwrap_or_default().trim();
            (c.tag_name().name().to_string(), value.to_string())
        })
        .collect();
    Ok(Answer::Done(arguments))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::IP_CONNECTION_1;

    /// The arguments an envelope carries, read back by a full XML reader.
    fn read_back(action: &Action) -> Vec<(String, String)> {
        let document = roxmltree::Document::parse(&action.body).unwrap();
        let call = document
            .descendants()
            .find(|n| n.tag_name().name() == action.name)
            .unwrap();
        assert_eq!(call.tag_name().namespace(), Some(IP_CONNECTION_1));
        call.children()
            .filter(|c| c.is_element())
            .map(|c| {
                (
                    c.tag_name().name().into(),
                    c.text().unwrap_or_default().into(),
                )
            })
            .collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn an_add_sends_every_argument_in_order() {
        let add = add_port_mapping(
            IP_CONNECTION_1,
            24137,
            Ipv4Addr::new(192, 168, 1, 166),
            "lowlat",
            2700,
        );
        assert_eq!(
            add.header,
            "\"urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping\""
        );
        assert_eq!(
            read_back(&add),
            pairs(&[
                ("NewRemoteHost", ""),
                ("NewExternalPort", "24137"),
                ("NewProtocol", "UDP"),
                ("NewInternalPort", "24137"),
                ("NewInternalClient", "192.168.1.166"),
                ("NewEnabled", "1"),
                ("NewPortMappingDescription", "lowlat"),
                ("NewLeaseDuration", "2700"),
            ])
        );
        let entry = [
            ("NewRemoteHost", ""),
            ("NewExternalPort", "24137"),
            ("NewProtocol", "UDP"),
        ];
        assert_eq!(
            read_back(&delete_port_mapping(IP_CONNECTION_1, 24137)),
            pairs(&entry)
        );
        assert_eq!(
            read_back(&get_specific_port_mapping_entry(IP_CONNECTION_1, 24137)),
            pairs(&entry)
        );
        assert_eq!(
            read_back(&get_external_ip_address(IP_CONNECTION_1)),
            pairs(&[])
        );
        assert_eq!(read_back(&get_status_info(IP_CONNECTION_1)), pairs(&[]));
    }

    #[test]
    fn escaped_arguments_survive_a_parse() {
        // Every printable character, alone and among the ones that need it.
        let mut values: Vec<String> = (0x20u8..0x7f).map(|b| char::from(b).to_string()).collect();
        values.push((0x20u8..0x7f).map(char::from).collect());
        values.push("a<b>&c\"d'e]]>f".into());
        values.push("&amp; is not an entity here".into());
        for value in values {
            let add = add_port_mapping(IP_CONNECTION_1, 1, Ipv4Addr::LOCALHOST, &value, 0);
            let back = read_back(&add);
            assert_eq!(back[6], ("NewPortMappingDescription".to_string(), value));
        }
    }

    fn envelope(inner: &str) -> Vec<u8> {
        format!(
            "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>{inner}</s:Body></s:Envelope>"
        )
        .into_bytes()
    }

    #[test]
    fn an_answer_is_read_by_its_action() {
        let body = envelope(
            "<u:GetExternalIPAddressResponse xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">\
             <NewExternalIPAddress>\r\n  203.0.113.7 </NewExternalIPAddress></u:GetExternalIPAddressResponse>",
        );
        let answer = parse("GetExternalIPAddress", 200, &body).unwrap();
        assert_eq!(answer.get("NewExternalIPAddress"), Some("203.0.113.7"));
        assert_eq!(answer.get("NewSomethingElse"), None);
        // Another action's response is not this one's.
        assert_eq!(parse("GetStatusInfo", 200, &body), Err(Error::Xml));
        // An empty response element: done, with nothing out.
        let added = envelope("<u:AddPortMappingResponse xmlns:u=\"urn:x\"/>");
        assert_eq!(
            parse("AddPortMapping", 200, &added),
            Ok(Answer::Done(Vec::new()))
        );
    }

    #[test]
    fn a_fault_is_read_whatever_the_status() {
        let fault = envelope(
            "<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>\
             <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode> 718 </errorCode>\
             <errorDescription>ConflictInMappingEntry</errorDescription></UPnPError></detail></s:Fault>",
        );
        for status in [500, 200, 400] {
            assert_eq!(
                parse("AddPortMapping", status, &fault),
                Ok(Answer::Fault(FaultCode::CONFLICT))
            );
        }
        assert_eq!(
            Answer::Fault(FaultCode::CONFLICT).get("NewExternalIPAddress"),
            None
        );
        let unreadable = envelope("<UPnPError><errorCode>seven</errorCode></UPnPError>");
        assert_eq!(parse("AddPortMapping", 500, &unreadable), Err(Error::Xml));
    }

    #[test]
    fn a_failure_with_no_fault_keeps_its_status() {
        assert_eq!(
            parse("AddPortMapping", 500, b"Internal Server Error"),
            Err(Error::Status(500))
        );
        assert_eq!(
            parse("AddPortMapping", 403, &envelope("")),
            Err(Error::Status(403))
        );
        assert_eq!(parse("AddPortMapping", 200, b"not xml"), Err(Error::Xml));
        // A declared entity is refused rather than expanded.
        let bomb = b"<?xml version=\"1.0\"?><!DOCTYPE x [<!ENTITY a \"aaaa\">]><x>&a;</x>";
        assert_eq!(parse("AddPortMapping", 200, bomb), Err(Error::Xml));
    }
}

#[cfg(test)]
mod captured {
    use super::*;
    use crate::captured::response;

    fn answer(name: &str, wire: &[u8]) -> Answer {
        let reply = response(wire);
        parse(name, reply.status, &reply.body).unwrap()
    }

    #[test]
    fn external_addresses_stated_and_not() {
        let openwrt = answer(
            "GetExternalIPAddress",
            include_bytes!("../tests/data/openwrt/external-address.http"),
        );
        assert_eq!(openwrt.get("NewExternalIPAddress"), Some("203.0.113.7"));
        let f670y = answer(
            "GetExternalIPAddress",
            include_bytes!("../tests/data/f670y/external-address.http"),
        );
        assert_eq!(f670y.get("NewExternalIPAddress"), Some("198.51.100.133"));
        // Behind a reserved address the daemon states none, and still maps.
        let debian = answer(
            "GetExternalIPAddress",
            include_bytes!("../tests/data/debian/external-address-empty.http"),
        );
        assert_eq!(debian.get("NewExternalIPAddress"), Some(""));
    }

    #[test]
    fn a_mapping_added_read_back_and_deleted() {
        for (add, entry, delete, client) in [
            (
                &include_bytes!("../tests/data/openwrt/add.http")[..],
                &include_bytes!("../tests/data/openwrt/entry.http")[..],
                &include_bytes!("../tests/data/openwrt/delete.http")[..],
                "192.168.1.166",
            ),
            (
                include_bytes!("../tests/data/f670y/add.http"),
                include_bytes!("../tests/data/f670y/entry.http"),
                include_bytes!("../tests/data/f670y/delete.http"),
                "192.168.1.100",
            ),
        ] {
            assert_eq!(answer("AddPortMapping", add), Answer::Done(Vec::new()));
            let entry = answer("GetSpecificPortMappingEntry", entry);
            assert_eq!(entry.get("NewInternalPort"), Some("47913"));
            assert_eq!(entry.get("NewInternalClient"), Some(client));
            assert_eq!(entry.get("NewEnabled"), Some("1"));
            assert_eq!(entry.get("NewPortMappingDescription"), Some("lowlat-probe"));
            assert_eq!(entry.get("NewLeaseDuration"), Some("120"));
            assert_eq!(
                answer("DeletePortMapping", delete),
                Answer::Done(Vec::new())
            );
        }
    }

    #[test]
    fn faults_from_a_server_error() {
        for wire in [
            &include_bytes!("../tests/data/openwrt/no-such-entry.http")[..],
            include_bytes!("../tests/data/f670y/no-such-entry.http"),
        ] {
            assert_eq!(
                answer("GetSpecificPortMappingEntry", wire),
                Answer::Fault(FaultCode::NO_SUCH_ENTRY)
            );
        }
        assert_eq!(
            answer(
                "GetFirewallStatus",
                include_bytes!("../tests/data/f670y/action-failed.http")
            ),
            Answer::Fault(FaultCode::ACTION_FAILED)
        );
    }
}
