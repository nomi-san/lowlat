//! The gateway's description: which connection services it has, and where.
//!
//! Read for one thing, the control address of each service a mapping can use,
//! and read defensively:
//!
//! - element names are matched without their namespace and without case,
//!   because devices get both wrong;
//! - a base element is honoured when it parses and passed over when it does
//!   not, the description's own address standing in;
//! - the device tree is walked as deep as a description has it, to a bound;
//! - a description with no service of interest is not an error, it is a
//!   gateway that maps nothing.
//!
//! The device is read from here and never from a search answer, which may
//! echo the version asked for. A gateway may list several connection
//! services, some of them down; which one to use is for asking each.
//!
//! A document type declaration is refused and the node count bounded, so a
//! description cannot expand into more than its bytes.

use crate::url::Url;
use crate::{Error, Result};

/// The connection services, by type.
pub const IP_CONNECTION_2: &str = "urn:schemas-upnp-org:service:WANIPConnection:2";
pub const IP_CONNECTION_1: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";
pub const PPP_CONNECTION_1: &str = "urn:schemas-upnp-org:service:WANPPPConnection:1";
/// The same two services under an older vendor namespace, which some
/// gateways still describe.
pub const VENDOR_IP_CONNECTION_1: &str = "urn:dslforum-org:service:WANIPConnection:1";
pub const VENDOR_PPP_CONNECTION_1: &str = "urn:dslforum-org:service:WANPPPConnection:1";

/// The connection services a mapping can use, in the order they are tried.
pub const CONNECTIONS: [&str; 5] = [
    IP_CONNECTION_2,
    IP_CONNECTION_1,
    PPP_CONNECTION_1,
    VENDOR_IP_CONNECTION_1,
    VENDOR_PPP_CONNECTION_1,
];

const MAX_NODES: u32 = 16 * 1024;
const MAX_DEPTH: usize = 8;

/// A connection service and its control address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// One of [`CONNECTIONS`], never the device's own string, so it goes into
    /// a header and an envelope as it is.
    pub service_type: &'static str,
    pub control: Url,
}

/// What a description offers a mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Description {
    /// In the order the description lists them.
    pub services: Vec<Service>,
}

impl Description {
    /// The connection services, in the order they are tried.
    pub fn connections(&self) -> impl Iterator<Item = &Service> {
        CONNECTIONS
            .into_iter()
            .flat_map(|kind| self.services.iter().filter(move |s| s.service_type == kind))
    }
}

/// Read a description fetched from `location`.
pub fn parse(body: &[u8], location: &Url) -> Result<Description> {
    // Read as the protocol's encoding whatever the device declares: a byte
    // that is not one stands for itself in a name nothing here reads, and
    // refusing the whole description for it would map nothing.
    let text = String::from_utf8_lossy(body);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let options = roxmltree::ParsingOptions {
        nodes_limit: MAX_NODES,
        ..roxmltree::ParsingOptions::default()
    };
    let document =
        roxmltree::Document::parse_with_options(text, options).map_err(|_| Error::Xml)?;
    let root = document.root_element();
    let base = text_of(root, "URLBase")
        .and_then(|base| Url::parse(base).ok())
        .unwrap_or_else(|| location.clone());
    let mut services = Vec::new();
    if let Some(device) = child(root, "device") {
        walk(device, &base, 0, &mut services);
    }
    Ok(Description { services })
}

fn walk(device: roxmltree::Node<'_, '_>, base: &Url, depth: usize, out: &mut Vec<Service>) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Some(list) = child(device, "serviceList") {
        for service in children(list, "service") {
            let Some(service_type) = text_of(service, "serviceType").and_then(known) else {
                continue;
            };
            let Some(control) = text_of(service, "controlURL").and_then(|c| base.join(c).ok())
            else {
                continue;
            };
            let found = Service {
                service_type,
                control,
            };
            if !out.contains(&found) {
                out.push(found);
            }
        }
    }
    if let Some(list) = child(device, "deviceList") {
        for device in children(list, "device") {
            walk(device, base, depth + 1, out);
        }
    }
}

fn known(service_type: &str) -> Option<&'static str> {
    CONNECTIONS.into_iter().find(|kind| *kind == service_type)
}

fn children<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> {
    node.children()
        .filter(move |c| c.is_element() && c.tag_name().name().eq_ignore_ascii_case(name))
}

fn child<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'input>> {
    children(node, name).next()
}

fn text_of<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    child(node, name).and_then(|c| c.text()).map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location() -> Url {
        Url::parse("http://192.168.1.1:5000/desc/root.xml").unwrap()
    }

    fn description(devices: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\">{devices}</root>"
        )
    }

    fn service(service_type: &str, control: &str) -> String {
        format!(
            "<service><serviceType>{service_type}</serviceType><controlURL>{control}</controlURL></service>"
        )
    }

    fn controls(found: &Description) -> Vec<(&'static str, String)> {
        found
            .connections()
            .map(|s| (s.service_type, s.control.path.clone()))
            .collect()
    }

    #[test]
    fn connections_are_tried_in_order_whatever_the_listing() {
        let body = description(&format!(
            "<device><serviceList>{}{}</serviceList><deviceList><device><serviceList>{}{}\
             </serviceList></device></deviceList></device>",
            service(PPP_CONNECTION_1, "/ppp"),
            service(VENDOR_IP_CONNECTION_1, "/vendor"),
            service(IP_CONNECTION_1, "ip1"),
            service(IP_CONNECTION_2, "../ip2"),
        ));
        let found = parse(body.as_bytes(), &location()).unwrap();
        assert_eq!(
            controls(&found),
            vec![
                (IP_CONNECTION_2, "/ip2".to_string()),
                (IP_CONNECTION_1, "/desc/ip1".to_string()),
                (PPP_CONNECTION_1, "/ppp".to_string()),
                (VENDOR_IP_CONNECTION_1, "/vendor".to_string()),
            ]
        );
    }

    #[test]
    fn names_are_matched_without_namespace_or_case() {
        let body = "<r:root xmlns:r=\"urn:wrong\"><r:Device><r:ServiceList><r:service>\
                    <r:SERVICETYPE> urn:schemas-upnp-org:service:WANIPConnection:1 </r:SERVICETYPE>\
                    <r:ControlUrl>/ctl</r:ControlUrl></r:service></r:ServiceList></r:Device></r:root>";
        let found = parse(body.as_bytes(), &location()).unwrap();
        assert_eq!(
            controls(&found),
            vec![(IP_CONNECTION_1, "/ctl".to_string())]
        );
    }

    #[test]
    fn a_base_is_honoured_when_it_parses() {
        let devices = format!(
            "<device><serviceList>{}</serviceList></device>",
            service(IP_CONNECTION_1, "ctl")
        );
        let based = format!("<URLBase>http://192.168.1.1:49000/igd/</URLBase>{devices}");
        let found = parse(description(&based).as_bytes(), &location()).unwrap();
        let control = &found.services[0].control;
        assert_eq!(
            (control.addr.port(), control.path.as_str()),
            (49000, "/igd/ctl")
        );
        // A base naming its host by name is passed over for the location.
        let named = format!("<URLBase>http://gateway.lan:49000/</URLBase>{devices}");
        let found = parse(description(&named).as_bytes(), &location()).unwrap();
        let control = &found.services[0].control;
        assert_eq!(
            (control.addr.port(), control.path.as_str()),
            (5000, "/desc/ctl")
        );
    }

    #[test]
    fn what_cannot_be_used_is_passed_over() {
        let body = description(&format!(
            "<device><serviceList>{}{}{}{}{}</serviceList></device>",
            service("urn:schemas-upnp-org:service:Layer3Forwarding:1", "/l3f"),
            service(IP_CONNECTION_1, "https://192.168.1.1/ctl"),
            service(IP_CONNECTION_1, "/ctl\tx"),
            service(IP_CONNECTION_1, "/ctl"),
            service(IP_CONNECTION_1, "/ctl"),
        ));
        let found = parse(body.as_bytes(), &location()).unwrap();
        assert_eq!(
            controls(&found),
            vec![(IP_CONNECTION_1, "/ctl".to_string())]
        );
        // Nothing usable is a gateway that maps nothing, not an error.
        let none = parse(description("<device/>").as_bytes(), &location()).unwrap();
        assert_eq!(none, Description::default());
    }

    #[test]
    fn the_device_tree_is_walked_to_a_bound() {
        let deep = |levels: usize| {
            let mut devices = format!(
                "<device><serviceList>{}</serviceList></device>",
                service(IP_CONNECTION_1, "/deep")
            );
            for _ in 0..levels {
                devices = format!("<device><deviceList>{devices}</deviceList></device>");
            }
            description(&devices)
        };
        let reached = |levels| {
            parse(deep(levels).as_bytes(), &location())
                .unwrap()
                .services
                .len()
        };
        assert_eq!(reached(MAX_DEPTH), 1);
        assert_eq!(reached(MAX_DEPTH + 1), 0);
    }

    /// A device that declares another encoding and writes its name in it is
    /// read all the same: the services are what is read, and they are text.
    #[test]
    fn a_name_in_another_encoding_hides_no_service() {
        let text = description(&format!(
            "<device><friendlyName>Routeur #</friendlyName><serviceList>{}</serviceList></device>",
            service(IP_CONNECTION_1, "/ctl")
        ));
        // The name's last character as a Latin-1 byte, which is no UTF-8.
        let mut body = text.into_bytes();
        let at = body.iter().position(|&byte| byte == b'#').unwrap();
        body[at] = 0xe9;
        let found = parse(&body, &location()).unwrap();
        assert_eq!(
            controls(&found),
            vec![(IP_CONNECTION_1, "/ctl".to_string())]
        );
    }

    #[test]
    fn what_does_not_parse() {
        assert_eq!(parse(b"\xff\xfe<root/>", &location()), Err(Error::Xml));
        assert_eq!(parse(b"<root>", &location()), Err(Error::Xml));
        let bomb = b"<?xml version=\"1.0\"?><!DOCTYPE r [<!ENTITY a \"aaaa\">]><root>&a;</root>";
        assert_eq!(parse(bomb, &location()), Err(Error::Xml));
        let mut huge = String::from("<root>");
        for _ in 0..MAX_NODES {
            huge.push_str("<x/>");
        }
        huge.push_str("</root>");
        assert_eq!(parse(huge.as_bytes(), &location()), Err(Error::Xml));
        // A byte order mark is not an error.
        let marked = format!("\u{feff}{}", description("<device/>"));
        assert!(parse(marked.as_bytes(), &location()).is_ok());
    }
}

#[cfg(test)]
mod captured {
    use super::*;
    use crate::captured::response;

    fn connections(wire: &[u8], location: &str) -> Vec<(&'static str, String)> {
        let location = Url::parse(location).unwrap();
        let found = parse(&response(wire).body, &location).unwrap();
        found
            .connections()
            .map(|s| {
                (
                    s.service_type,
                    format!("{}{}", s.control.addr, s.control.path),
                )
            })
            .collect()
    }

    #[test]
    fn each_gateways_connection_service() {
        // Relative to the description's location, with no base.
        assert_eq!(
            connections(
                include_bytes!("../tests/data/openwrt/description.http"),
                "http://192.168.1.1:5000/rootDesc.xml"
            ),
            vec![(IP_CONNECTION_1, "192.168.1.1:5000/ctl/IPConn".to_string())]
        );
        // Against a base, past services of no interest and the IPv6 firewall.
        assert_eq!(
            connections(
                include_bytes!("../tests/data/f670y/description.http"),
                "http://192.168.1.1:52869/gatedesc.xml"
            ),
            vec![(
                IP_CONNECTION_1,
                "192.168.1.1:52869/upnp/control/WANIPConn1".to_string()
            )]
        );
        // Version 2.
        assert_eq!(
            connections(
                include_bytes!("../tests/data/debian/description.http"),
                "http://192.168.10.1:45345/rootDesc.xml"
            ),
            vec![(IP_CONNECTION_2, "192.168.10.1:45345/ctl/IPConn".to_string())]
        );
    }
}
